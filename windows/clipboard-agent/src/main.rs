//! beaglewing-clip: tiny unprivileged Windows helper. Opens the vendor
//! HID interface of the Beaglewing Input Bridge (no driver, no admin) and
//! syncs the clipboard with Ubuntu through it.
//!
//! Text and images are eager. Files are lazy: a copy sends only an OFFER
//! and the batch goes out when Ubuntu PULLs it (which its router does
//! when the user crosses over). Copies that stay local never transfer.
//!
//! Images travel as PNG; the Windows clipboard speaks DIB, so conversion
//! happens here at the edge and images are published in both the
//! registered PNG format (Chromium apps) and CF_DIB (classic apps).
//! Echo loops are broken by hashing what we last sent and last set.
//! Clipboard contents are never logged. The HID device is reopened
//! automatically if the bridge re-enumerates.
//!
//! Run it in a terminal: beaglewing-clip.exe

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clipframe::{
    ack_frame, encode_transfer, eta_secs, files, fnv1a, frame, offer_frame, parse_ack,
    FeedResult, Offer, Reassembler, ACK_EVERY, ACK_WINDOW, FRAME_LEN, KIND_PNG, KIND_TAR,
    KIND_TEXT, MAX_TRANSFER, RATE_TO_LINUX, T_ABORT, T_PONG,
};
use hidapi::{HidApi, HidDevice};
use image::ImageFormat;

const VID: u16 = 0x1d6b;
const PID: u16 = 0x0104;
const USAGE_PAGE: u16 = 0xff60;
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;

fn content_hash(kind: u8, data: &[u8]) -> u64 {
    fnv1a(data) ^ (kind as u64)
}

fn offer_id(hash: u64) -> u32 {
    (hash as u32) ^ ((hash >> 32) as u32)
}

fn mb(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn stage_root() -> PathBuf {
    std::env::temp_dir().join("beaglewing").join("staged")
}

// --- device ------------------------------------------------------------

type SharedDev = Arc<Mutex<Option<HidDevice>>>;

fn try_open() -> Option<HidDevice> {
    let api = HidApi::new().ok()?;
    let path = api
        .device_list()
        .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == USAGE_PAGE)
        .map(|d| d.path().to_owned())?;
    api.open_path(&path).ok()
}

/// Windows allows several handles to one HID collection. Reading and
/// writing use separate handles so a blocked read never holds up a
/// write (the shared-handle design throttled Windows -> Linux sends to
/// the gaps between read timeouts).
fn ensure_open(dev: &SharedDev, role: &str) {
    let mut guard = dev.lock().unwrap();
    if guard.is_none() {
        if let Some(d) = try_open() {
            println!("[clip] data channel open ({role})");
            *guard = Some(d);
        }
    }
}

/// hidapi wants the report id (0 for none) prepended on writes.
/// Drops the device on failure so it gets reopened.
fn hid_send(dev: &SharedDev, f: &[u8; FRAME_LEN]) -> bool {
    let mut buf = [0u8; FRAME_LEN + 1];
    buf[1..].copy_from_slice(f);
    let mut guard = dev.lock().unwrap();
    match guard.as_ref() {
        Some(d) if d.write(&buf).is_ok() => true,
        Some(_) => {
            println!("[clip] device write failed; will reopen");
            *guard = None;
            false
        }
        None => false,
    }
}

/// Windowed send with one retry after an ACK stall. Keeps at most
/// ACK_WINDOW DATA frames in flight so the Linux side's queue never
/// overflows, and logs percent progress on big transfers.
fn send_frames(dev: &SharedDev, acked: &AtomicU32, frames: &[[u8; FRAME_LEN]], total: usize) -> bool {
    let last = frames.len() - 1;
    for attempt in 0..2 {
        acked.store(0, Ordering::SeqCst);
        let mut sent_data: u32 = 0;
        let mut last_log = Instant::now();
        let mut stalled = false;
        let mut ok = true;
        'send: for (i, f) in frames.iter().enumerate() {
            if i > 0 && i < last {
                let stall = Instant::now();
                while sent_data.saturating_sub(acked.load(Ordering::SeqCst)) >= ACK_WINDOW {
                    if stall.elapsed() > Duration::from_secs(5) {
                        hid_send(dev, &frame(T_ABORT, 0, &[]));
                        stalled = true;
                        break 'send;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                sent_data += 1;
                if total > 1_000_000 && last_log.elapsed() > Duration::from_secs(2) {
                    last_log = Instant::now();
                    println!("[clip] sending: {}%", (i * 100 / frames.len().max(1)).min(99));
                }
            }
            if !hid_send(dev, f) {
                println!("[clip] send failed; device gone?");
                ok = false;
                break;
            }
        }
        if ok && !stalled {
            return true;
        }
        if stalled && attempt == 0 {
            println!("[clip] send stalled (receiver not acking); retrying once");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        if stalled {
            println!("[clip] send abandoned: receiver stopped acking");
        }
        return false;
    }
    false
}

// --- clipboard ---------------------------------------------------------

fn png_to_bmp(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, ImageFormat::Png).ok()?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Bmp).ok()?;
    Some(out)
}

fn bmp_to_png(bmp: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(bmp, ImageFormat::Bmp).ok()?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).ok()?;
    Some(out)
}

/// Build a CF_HDROP payload: a 20-byte DROPFILES header, then the paths
/// as UTF-16, each NUL-terminated, with a final extra NUL.
fn build_hdrop(paths: &[PathBuf]) -> Vec<u8> {
    let mut buf = vec![0u8; 20];
    buf[0] = 20; // pFiles: offset of the path list
    buf[16] = 1; // fWide: UTF-16
    for p in paths {
        for unit in p.as_os_str().to_string_lossy().encode_utf16() {
            buf.extend_from_slice(&unit.to_le_bytes());
        }
        buf.extend_from_slice(&0u16.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf
}

/// Publish staged files: CF_HDROP plus Preferred DropEffect = COPY so
/// Explorer pastes as a copy rather than a move.
fn set_clipboard_files(paths: &[PathBuf]) -> bool {
    let Ok(_clip) = clipboard_win::Clipboard::new_attempts(10) else {
        return false;
    };
    if clipboard_win::empty().is_err() {
        return false;
    }
    let mut ok = clipboard_win::raw::set_without_clear(CF_HDROP, &build_hdrop(paths)).is_ok();
    if let Some(fmt) = clipboard_win::register_format("Preferred DropEffect") {
        ok &= clipboard_win::raw::set_without_clear(fmt.get(), &1u32.to_le_bytes()).is_ok();
    }
    ok
}

fn png_format_id() -> Option<u32> {
    clipboard_win::register_format("PNG").map(|n| n.get())
}

/// Publish an image the way Snipping Tool does: several formats at once.
fn set_clipboard_image(png: &[u8], bmp: &[u8]) -> bool {
    let Ok(_clip) = clipboard_win::Clipboard::new_attempts(10) else {
        return false;
    };
    if clipboard_win::empty().is_err() {
        return false;
    }
    let mut ok = false;
    if let Some(fmt) = png_format_id() {
        ok |= clipboard_win::raw::set_without_clear(fmt, png).is_ok();
    }
    if bmp.len() > 14 {
        // A DIB is a BMP without its 14-byte file header.
        ok |= clipboard_win::raw::set_without_clear(CF_DIB, &bmp[14..]).is_ok();
    }
    ok
}

/// Files first (an Explorer copy also offers the path as text), then
/// text, then the registered PNG format verbatim, then bitmap converted
/// to PNG. Returns (kind, payload, change hash); for files the payload
/// is the newline-joined path list.
fn get_clipboard_content() -> Option<(u8, Vec<u8>, u64)> {
    if let Ok(list) =
        clipboard_win::get_clipboard::<Vec<String>, _>(clipboard_win::formats::FileList)
    {
        if !list.is_empty() {
            let paths: Vec<PathBuf> = list.iter().map(PathBuf::from).collect();
            let (sig, _) = files::batch_signature(&paths)?;
            return Some((KIND_TAR, list.join("\n").into_bytes(), sig ^ KIND_TAR as u64));
        }
    }
    if let Ok(text) = clipboard_win::get_clipboard::<String, _>(clipboard_win::formats::Unicode) {
        if !text.is_empty() {
            let h = content_hash(KIND_TEXT, text.as_bytes());
            return Some((KIND_TEXT, text.into_bytes(), h));
        }
    }
    if let Some(fmt) = png_format_id() {
        if clipboard_win::is_format_avail(fmt) {
            if let Ok(_clip) = clipboard_win::Clipboard::new_attempts(10) {
                let mut png = Vec::new();
                if clipboard_win::raw::get_vec(fmt, &mut png).is_ok() && !png.is_empty() {
                    let h = content_hash(KIND_PNG, &png);
                    return Some((KIND_PNG, png, h));
                }
            }
        }
    }
    if let Ok(bmp) = clipboard_win::get_clipboard::<Vec<u8>, _>(clipboard_win::formats::Bitmap) {
        if let Some(png) = bmp_to_png(&bmp) {
            let h = content_hash(KIND_PNG, &png);
            return Some((KIND_PNG, png, h));
        }
    }
    None
}

// --- main --------------------------------------------------------------

fn main() {
    println!("beaglewing-clip: clipboard sync helper (text, images, files). Ctrl-C to quit.");
    files::cleanup_stage(&stage_root(), Duration::from_secs(3 * 24 * 3600));
    let dev_r: SharedDev = Arc::new(Mutex::new(None));
    let dev_w: SharedDev = Arc::new(Mutex::new(None));
    ensure_open(&dev_r, "read");
    ensure_open(&dev_w, "write");
    if dev_w.lock().unwrap().is_none() {
        println!("[clip] Beaglewing data interface not found yet; will keep looking");
    }

    let last_sent = Arc::new(Mutex::new(0u64));
    let last_set = Arc::new(Mutex::new(0u64));
    // Flow-control window: the reader thread stores the latest ACK count.
    let acked = Arc::new(AtomicU32::new(0));
    // A PULL from Ubuntu for the offer with this id, handled by the send loop.
    let pull_request: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // Applier thread: everything slow (image conversion, staging,
    // clipboard sets) happens here, never on the reader. A reader stall
    // overflows the Windows HID input queue (as small as 32 reports,
    // oldest dropped first) and loses incoming frames.
    let (apply_tx, apply_rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    {
        let last_set = Arc::clone(&last_set);
        std::thread::spawn(move || {
            for (kind, data) in apply_rx {
                match kind {
                    KIND_TEXT => {
                        let text = String::from_utf8_lossy(&data)
                            .replace("\r\n", "\n")
                            .replace('\n', "\r\n");
                        if clipboard_win::set_clipboard(clipboard_win::formats::Unicode, &text)
                            .is_ok()
                        {
                            *last_set.lock().unwrap() = content_hash(KIND_TEXT, text.as_bytes());
                            println!("[clip] received text ({} bytes)", text.len());
                        }
                    }
                    KIND_PNG => {
                        let Some(bmp) = png_to_bmp(&data) else {
                            println!("[clip] received image failed to decode");
                            continue;
                        };
                        if set_clipboard_image(&data, &bmp) {
                            *last_set.lock().unwrap() = content_hash(KIND_PNG, &data);
                            println!("[clip] received image ({} bytes)", data.len());
                        } else {
                            println!("[clip] failed to set image on clipboard");
                        }
                    }
                    KIND_TAR => match files::unpack_to_stage(&data, &stage_root()) {
                        Ok(staged) => {
                            if set_clipboard_files(&staged) {
                                if let Some((sig, _)) = files::batch_signature(&staged) {
                                    *last_set.lock().unwrap() = sig ^ KIND_TAR as u64;
                                }
                                println!(
                                    "[clip] received {} item(s), staged in {}; paste away",
                                    staged.len(),
                                    stage_root().display()
                                );
                            } else {
                                println!("[clip] failed to set files on clipboard");
                            }
                        }
                        Err(e) => println!("[clip] receiving files failed: {e}"),
                    },
                    other => println!("[clip] ignoring unsupported kind {other}"),
                }
            }
        });
    }

    // Reader thread: reads, ACKs, hands off. Never blocks on anything
    // but the device read itself.
    {
        let dev = Arc::clone(&dev_r);
        let dev_w = Arc::clone(&dev_w);
        let acked = Arc::clone(&acked);
        let pull_request = Arc::clone(&pull_request);
        std::thread::spawn(move || {
            let mut reasm = Reassembler::default();
            let mut buf = [0u8; FRAME_LEN];
            loop {
                let n = {
                    let mut guard = dev.lock().unwrap();
                    match guard.as_ref() {
                        Some(d) => match d.read_timeout(&mut buf, 100) {
                            Ok(n) => n,
                            Err(_) => {
                                println!("[clip] device read failed; will reopen");
                                *guard = None;
                                0
                            }
                        },
                        None => 0,
                    }
                };
                if n != FRAME_LEN {
                    std::thread::sleep(Duration::from_millis(10));
                    ensure_open(&dev, "read");
                    continue;
                }
                if let Some(count) = parse_ack(&buf) {
                    acked.store(count, Ordering::SeqCst);
                    continue;
                }
                match reasm.feed(&buf) {
                    FeedResult::Ping => {
                        hid_send(&dev_w, &frame(T_PONG, 0, &[]));
                    }
                    FeedResult::Progress(count) => {
                        if count % ACK_EVERY == 0 {
                            hid_send(&dev_w, &ack_frame(count));
                        }
                        if count % 8192 == 0 {
                            if let Some((got, total)) = reasm.status() {
                                println!("[clip] receiving: {}/{} KB", got / 1024, total / 1024);
                            }
                        }
                    }
                    FeedResult::Complete(kind, data) => {
                        let _ = apply_tx.send((kind, data));
                    }
                    FeedResult::Offer(o) => {
                        println!(
                            "[clip] Ubuntu offers {} item(s) ({}); they arrive when you cross over",
                            o.items,
                            mb(o.total as u64)
                        );
                    }
                    FeedResult::Pull(id) => {
                        *pull_request.lock().unwrap() = Some(id);
                    }
                    FeedResult::Error(e) => println!("[clip] protocol error: {e}"),
                    FeedResult::None => {}
                }
            }
        });
    }

    // Send loop: poll the clipboard sequence number; offer files, send
    // everything else eagerly; serve pulls for the current offer.
    let mut local_offer: Option<(u32, Vec<PathBuf>)> = None;
    let mut last_seq = clipboard_win::raw::seq_num().map(|n| n.get()).unwrap_or(0);
    let dev = dev_w;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        ensure_open(&dev, "write");

        if let Some(id) = pull_request.lock().unwrap().take() {
            match &local_offer {
                Some((oid, paths)) if *oid == id => match files::pack(paths, MAX_TRANSFER as u64) {
                    Ok(tar) => {
                        println!(
                            "[clip] sending {} item(s), {}, roughly {}s",
                            paths.len(),
                            mb(tar.len() as u64),
                            eta_secs(tar.len(), RATE_TO_LINUX)
                        );
                        let frames = encode_transfer(KIND_TAR, &tar);
                        if send_frames(&dev, &acked, &frames, tar.len()) {
                            println!("[clip] sent files ({} bytes)", tar.len());
                            local_offer = None;
                        }
                    }
                    Err(e) => println!("[clip] not sending files: {e}"),
                },
                _ => println!("[clip] pull for an offer we no longer hold; ignoring"),
            }
        }

        let seq = clipboard_win::raw::seq_num().map(|n| n.get()).unwrap_or(0);
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        let Some((kind, content, h)) = get_clipboard_content() else {
            continue;
        };
        if h == *last_sent.lock().unwrap() || h == *last_set.lock().unwrap() {
            continue;
        }

        if kind == KIND_TAR {
            let paths: Vec<PathBuf> = String::from_utf8_lossy(&content)
                .lines()
                .map(PathBuf::from)
                .collect();
            let Some((_, total)) = files::batch_signature(&paths) else {
                continue;
            };
            let offer = Offer {
                kind: KIND_TAR,
                total: total.min(u32::MAX as u64) as u32,
                items: paths.len().min(u16::MAX as usize) as u16,
                id: offer_id(h),
            };
            if hid_send(&dev, &offer_frame(&offer)) {
                println!(
                    "[clip] offering {} item(s) ({}); sends when you cross to Ubuntu",
                    paths.len(),
                    mb(total)
                );
                local_offer = Some((offer.id, paths));
                *last_sent.lock().unwrap() = h;
            }
            continue;
        }

        // A new local copy supersedes any file offer.
        local_offer = None;
        let frames = encode_transfer(kind, &content);
        if send_frames(&dev, &acked, &frames, content.len()) {
            let what = if kind == KIND_PNG { "image" } else { "text" };
            println!("[clip] sent {what} ({} bytes)", content.len());
        }
        *last_sent.lock().unwrap() = h; // never spin on a bad batch
    }
}
