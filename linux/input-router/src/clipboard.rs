//! Ubuntu clipboard agent: syncs the local clipboard with the Windows
//! helper through the Pi data relay.
//!
//! Text and images are eager (small, and eager makes paste instant).
//! Files are lazy: a copy sends only an OFFER; the bytes move when the
//! user crosses over to the other machine (the router signals crossings
//! over a local socket) or asks explicitly (`beaglewing-router pull`).
//! Copies that never cross machines never transfer.
//!
//! Local clipboard access goes through XWayland (xclip): mutter mirrors
//! the Wayland and X11 clipboards both ways, and X11 reads are silent
//! (wl-paste polling creates a transient window per call and visibly
//! churns the desktop). Line endings: LF out, CRLF normalized in. Echo
//! loops are broken by hashing what we last sent and last set locally.
//! Clipboard contents are never logged.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clipframe::{
    ack_frame, encode_transfer, eta_secs, files, fnv1a, frame, offer_frame, parse_ack,
    pull_frame, FeedResult, Offer, Reassembler, ACK_EVERY, ACK_WINDOW, FRAME_LEN, KIND_PNG,
    KIND_TAR, KIND_TEXT, MAX_TRANSFER, RATE_TO_LINUX, RATE_TO_WINDOWS, T_ABORT, T_PONG,
};

enum Ev {
    Local(u8, Vec<u8>, u64), // kind, content, change hash
    Frame([u8; FRAME_LEN]),
    Control(String), // "remote" | "local" from the router, "pull" from the CLI
    Disconnected,
}

/// What the local side currently offers to the other machine.
struct LocalOffer {
    id: u32,
    paths: Vec<PathBuf>,
}

/// What the other machine currently offers to us.
struct RemoteOffer {
    offer: Offer,
    requested: bool,
}

fn content_hash(kind: u8, data: &[u8]) -> u64 {
    fnv1a(data) ^ (kind as u64)
}

fn offer_id(hash: u64) -> u32 {
    (hash as u32) ^ ((hash >> 32) as u32)
}

fn stage_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/beaglewing/staged")
}

/// Datagram socket the router and CLI use to poke the agent.
pub fn control_socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("beaglewing-clip.sock")
}

/// Fire-and-forget control message to the agent (used by the router on
/// crossings and by `beaglewing-router pull`).
pub fn send_control(msg: &str) -> bool {
    UnixDatagram::unbound()
        .and_then(|s| s.send_to(msg.as_bytes(), control_socket_path()))
        .is_ok()
}

fn notify(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["--app-name=Beaglewing", "--expire-time=6000", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn mb(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

// --- local clipboard ---------------------------------------------------

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let raw = uri.trim().strip_prefix("file://")?;
    let mut out = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

fn path_to_uri(path: &Path) -> String {
    let mut enc = String::from("file://");
    for &b in path.to_string_lossy().as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    enc
}

/// Parse an x-special/gnome-copied-files or text/uri-list payload.
fn parse_file_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != "copy" && *l != "cut")
        .filter_map(uri_to_path)
        .collect()
}

fn xclip_out(target: &str) -> Option<Vec<u8>> {
    let out = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", target, "-o"])
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// Decide the kind from the advertised TARGETS list, never by probing
/// content targets blindly: an xclip owner answers any requested target
/// with its data, so a text-first probe would misread an image owner
/// (including our own after receiving one). Files come before text
/// because a file-manager copy also offers the path as plain text.
/// Returns (kind, payload, change hash).
fn read_local_clipboard() -> Option<(u8, Vec<u8>, u64)> {
    let targets = xclip_out("TARGETS")?;
    let targets = String::from_utf8_lossy(&targets);
    let has = |t: &str| targets.lines().any(|l| l == t);
    for t in ["x-special/gnome-copied-files", "text/uri-list"] {
        if has(t) {
            let data = xclip_out(t)?;
            let paths = parse_file_list(&data);
            if paths.is_empty() {
                return None;
            }
            // Signature from filesystem metadata: cheap enough to poll,
            // and it matches what we compute for staged batches we set.
            let (sig, _) = files::batch_signature(&paths)?;
            return Some((KIND_TAR, data, sig ^ KIND_TAR as u64));
        }
    }
    if has("UTF8_STRING") || has("STRING") || has("text/plain;charset=utf-8") || has("text/plain")
    {
        let t = if has("UTF8_STRING") { "UTF8_STRING" } else { "STRING" };
        return xclip_out(t).map(|d| {
            let h = content_hash(KIND_TEXT, &d);
            (KIND_TEXT, d, h)
        });
    }
    if has("image/png") {
        return xclip_out("image/png").map(|d| {
            let h = content_hash(KIND_PNG, &d);
            (KIND_PNG, d, h)
        });
    }
    None
}

/// The spawned xclip forks a child that keeps serving the selection; it
/// lives in our cgroup, so it survives until this service stops. xclip
/// serves a single target, so file batches go up as
/// x-special/gnome-copied-files (what Nautilus pastes).
fn set_local_clipboard(kind: u8, data: &[u8]) -> bool {
    let mut args = vec!["-selection", "clipboard"];
    if kind == KIND_PNG {
        args.extend(["-t", "image/png"]);
    } else if kind == KIND_TAR {
        args.extend(["-t", "x-special/gnome-copied-files"]);
    }
    args.push("-i");
    let Ok(mut child) = Command::new("xclip").args(&args).stdin(Stdio::piped()).spawn() else {
        return false;
    };
    let ok = child
        .stdin
        .take()
        .map(|mut s| s.write_all(data).is_ok())
        .unwrap_or(false);
    let _ = child.wait();
    ok
}

// --- threads -----------------------------------------------------------

/// mutter has no data-control protocol for event-driven watching, so
/// poll the clipboard's change hash with one-shot xclip every 500ms.
fn spawn_watcher(tx: Sender<Ev>) {
    std::thread::spawn(move || {
        let mut prev: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let Some((kind, content, h)) = read_local_clipboard() else {
                continue;
            };
            if h == prev {
                continue;
            }
            prev = h;
            if tx.send(Ev::Local(kind, content, h)).is_err() {
                return;
            }
        }
    });
}

fn spawn_control_listener(tx: Sender<Ev>) {
    let path = control_socket_path();
    let _ = std::fs::remove_file(&path);
    let Ok(sock) = UnixDatagram::bind(&path) else {
        println!("[clip] control socket unavailable; crossing-triggered transfers disabled");
        return;
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok(n) = sock.recv(&mut buf) {
            let msg = String::from_utf8_lossy(&buf[..n]).trim().to_string();
            if tx.send(Ev::Control(msg)).is_err() {
                return;
            }
        }
    });
}

fn spawn_reader(mut stream: TcpStream, tx: Sender<Ev>, acked: Arc<AtomicU32>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; FRAME_LEN];
        loop {
            match stream.read_exact(&mut buf) {
                Ok(()) => {
                    // ACKs feed the sender's flow-control window directly;
                    // routing them through the busy main loop would
                    // deadlock a large send against its own ACKs.
                    if let Some(count) = parse_ack(&buf) {
                        acked.store(count, Ordering::SeqCst);
                        continue;
                    }
                    if tx.send(Ev::Frame(buf)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = tx.send(Ev::Disconnected);
                    return;
                }
            }
        }
    });
}

// --- sending -----------------------------------------------------------

/// Windowed send: never more than ACK_WINDOW unacked DATA frames in
/// flight, so a bursty transfer cannot overflow the Windows HID input
/// queue (which drops oldest, killing the whole transfer).
fn send_transfer(
    writer: &mut TcpStream,
    frames: &[[u8; FRAME_LEN]],
    acked: &AtomicU32,
    total_bytes: usize,
) -> Result<(), &'static str> {
    acked.store(0, Ordering::SeqCst);
    let mut sent_data: u32 = 0;
    let last = frames.len() - 1;
    let mut last_log = Instant::now();
    for (i, f) in frames.iter().enumerate() {
        if i > 0 && i < last {
            let stall_start = Instant::now();
            while sent_data.saturating_sub(acked.load(Ordering::SeqCst)) >= ACK_WINDOW {
                if stall_start.elapsed() > Duration::from_secs(5) {
                    let _ = writer.write_all(&frame(T_ABORT, 0, &[]));
                    return Err("receiver stopped acking (helper not running?)");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            sent_data += 1;
            if total_bytes > 1_000_000 && last_log.elapsed() > Duration::from_secs(2) {
                last_log = Instant::now();
                println!("[clip] sending: {}%", (i * 100 / frames.len().max(1)).min(99));
            }
        }
        writer.write_all(f).map_err(|_| "relay connection lost")?;
    }
    Ok(())
}

/// Send one payload with a single retry after an ACK stall (a receiver
/// busy applying the previous transfer can miss a START frame).
/// Err(true) means the relay connection is gone.
fn send_with_retry(
    writer: &mut TcpStream,
    acked: &AtomicU32,
    kind: u8,
    data: &[u8],
) -> Result<(), bool> {
    let frames = encode_transfer(kind, data);
    for attempt in 0..2 {
        match send_transfer(writer, &frames, acked, data.len()) {
            Ok(()) => {
                let what = match kind {
                    KIND_PNG => "image",
                    KIND_TAR => "files",
                    _ => "text",
                };
                println!("[clip] sent {what} ({} bytes)", data.len());
                return Ok(());
            }
            Err("relay connection lost") => return Err(true),
            Err(e) if attempt == 0 => {
                println!("[clip] send stalled ({e}); retrying once");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => println!("[clip] send abandoned: {e}"),
        }
    }
    Err(false)
}

/// Pack and push the local file offer. Err(true) = relay gone.
fn push_local_offer(
    writer: &mut TcpStream,
    acked: &AtomicU32,
    offer: &LocalOffer,
) -> Result<(), bool> {
    match files::pack(&offer.paths, MAX_TRANSFER as u64) {
        Ok(tar) => {
            println!(
                "[clip] sending {} item(s), {}, roughly {}s",
                offer.paths.len(),
                mb(tar.len() as u64),
                eta_secs(tar.len(), RATE_TO_WINDOWS)
            );
            send_with_retry(writer, acked, KIND_TAR, &tar)
        }
        Err(e) => {
            println!("[clip] not sending files: {e}");
            Err(false)
        }
    }
}

// --- receiving ---------------------------------------------------------

enum Incoming {
    Offer(Offer),
    Pull(u32),
    Done(u8),
}

fn handle_frame(
    reasm: &mut Reassembler,
    f: &[u8; FRAME_LEN],
    writer: &mut TcpStream,
    last_set: &mut u64,
) -> Option<Incoming> {
    match reasm.feed(f) {
        FeedResult::Ping => {
            let _ = writer.write_all(&frame(T_PONG, 0, &[]));
        }
        FeedResult::Progress(n) => {
            if n % ACK_EVERY == 0 {
                let _ = writer.write_all(&ack_frame(n));
            }
            if n % 8192 == 0 {
                if let Some((got, total)) = reasm.status() {
                    println!("[clip] receiving: {}/{} KB", got / 1024, total / 1024);
                }
            }
        }
        FeedResult::Offer(o) => return Some(Incoming::Offer(o)),
        FeedResult::Pull(id) => return Some(Incoming::Pull(id)),
        FeedResult::Complete(KIND_TEXT, data) => {
            let text = String::from_utf8_lossy(&data).replace("\r\n", "\n");
            if set_local_clipboard(KIND_TEXT, text.as_bytes()) {
                *last_set = content_hash(KIND_TEXT, text.as_bytes());
                println!("[clip] received text ({} bytes)", text.len());
                return Some(Incoming::Done(KIND_TEXT));
            }
        }
        FeedResult::Complete(KIND_PNG, data) => {
            if set_local_clipboard(KIND_PNG, &data) {
                *last_set = content_hash(KIND_PNG, &data);
                println!("[clip] received image ({} bytes)", data.len());
                return Some(Incoming::Done(KIND_PNG));
            }
        }
        FeedResult::Complete(KIND_TAR, data) => match files::unpack_to_stage(&data, &stage_root())
        {
            Ok(staged) => {
                let mut list = String::from("copy");
                for p in &staged {
                    list.push('\n');
                    list.push_str(&path_to_uri(p));
                }
                if set_local_clipboard(KIND_TAR, list.as_bytes()) {
                    if let Some((sig, _)) = files::batch_signature(&staged) {
                        *last_set = sig ^ KIND_TAR as u64;
                    }
                    println!(
                        "[clip] received {} item(s), staged under {}",
                        staged.len(),
                        stage_root().display()
                    );
                    return Some(Incoming::Done(KIND_TAR));
                }
            }
            Err(e) => println!("[clip] receiving files failed: {e}"),
        },
        FeedResult::Complete(kind, _) => {
            println!("[clip] ignoring transfer of unsupported kind {kind}");
        }
        FeedResult::Error(e) => println!("[clip] protocol error: {e}"),
        FeedResult::None => {}
    }
    None
}

// --- main loop ---------------------------------------------------------

pub fn run(addr: &str) -> ! {
    files::cleanup_stage(&stage_root(), Duration::from_secs(3 * 24 * 3600));
    let (tx, rx): (Sender<Ev>, Receiver<Ev>) = channel();
    spawn_watcher(tx.clone());
    spawn_control_listener(tx.clone());

    let mut last_sent: u64 = 0;
    let mut last_set: u64 = 0;
    let mut local_offer: Option<LocalOffer> = None;
    let mut remote_offer: Option<RemoteOffer> = None;

    loop {
        let stream = match TcpStream::connect(addr) {
            Ok(s) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        stream.set_nodelay(true).ok();
        println!("[clip] connected to data relay at {addr}");
        let mut writer = stream.try_clone().expect("clone stream");
        let acked = Arc::new(AtomicU32::new(0));
        spawn_reader(stream, tx.clone(), Arc::clone(&acked));

        let mut reasm = Reassembler::default();
        'session: loop {
            match rx.recv() {
                Ok(Ev::Local(kind, content, h)) => {
                    if h == last_sent || h == last_set {
                        continue;
                    }
                    if kind == KIND_TAR {
                        // Lazy: offer now, send on crossing or pull.
                        let paths = parse_file_list(&content);
                        let Some((_, total)) = files::batch_signature(&paths) else {
                            continue;
                        };
                        let offer = Offer {
                            kind: KIND_TAR,
                            total: total.min(u32::MAX as u64) as u32,
                            items: paths.len().min(u16::MAX as usize) as u16,
                            id: offer_id(h),
                        };
                        if writer.write_all(&offer_frame(&offer)).is_err() {
                            break 'session;
                        }
                        println!(
                            "[clip] offering {} item(s) ({}); sends when you cross over or pull",
                            paths.len(),
                            mb(total)
                        );
                        local_offer = Some(LocalOffer { id: offer.id, paths });
                        last_sent = h;
                    } else {
                        // A new local copy supersedes any file offer.
                        local_offer = None;
                        match send_with_retry(&mut writer, &acked, kind, &content) {
                            Ok(()) | Err(false) => last_sent = h,
                            Err(true) => break 'session,
                        }
                    }
                }
                Ok(Ev::Frame(f)) => {
                    match handle_frame(&mut reasm, &f, &mut writer, &mut last_set) {
                        Some(Incoming::Offer(o)) => {
                            println!(
                                "[clip] Windows offers {} item(s) ({}); cross over or run: beaglewing-router pull",
                                o.items,
                                mb(o.total as u64)
                            );
                            notify(
                                "Files available from Windows",
                                &format!(
                                    "{} item(s), {}. Cross over to fetch them.",
                                    o.items,
                                    mb(o.total as u64)
                                ),
                            );
                            remote_offer = Some(RemoteOffer { offer: o, requested: false });
                        }
                        Some(Incoming::Pull(id)) => match &local_offer {
                            Some(lo) if lo.id == id => {
                                match push_local_offer(&mut writer, &acked, lo) {
                                    Ok(()) => local_offer = None,
                                    Err(true) => break 'session,
                                    Err(false) => {}
                                }
                            }
                            _ => println!("[clip] pull for an offer we no longer hold; ignoring"),
                        },
                        Some(Incoming::Done(KIND_TAR)) => {
                            remote_offer = None;
                            notify("Files ready", "Paste them in Files.");
                        }
                        Some(Incoming::Done(_)) | None => {}
                    }
                }
                Ok(Ev::Control(msg)) => {
                    let push = matches!(msg.as_str(), "remote" | "pull");
                    let pull = matches!(msg.as_str(), "local" | "pull");
                    if push {
                        if let Some(lo) = local_offer.take() {
                            match push_local_offer(&mut writer, &acked, &lo) {
                                Ok(()) => {}
                                Err(true) => break 'session,
                                Err(false) => local_offer = Some(lo),
                            }
                        }
                    }
                    if pull {
                        if let Some(ro) = remote_offer.as_mut() {
                            if !ro.requested || msg == "pull" {
                                ro.requested = true;
                                println!(
                                    "[clip] pulling {} item(s) ({}) from Windows, roughly {}s",
                                    ro.offer.items,
                                    mb(ro.offer.total as u64),
                                    eta_secs(ro.offer.total as usize, RATE_TO_LINUX)
                                );
                                if writer.write_all(&pull_frame(ro.offer.id)).is_err() {
                                    break 'session;
                                }
                            }
                        }
                    }
                }
                Ok(Ev::Disconnected) | Err(_) => break 'session,
            }
        }
        println!("[clip] relay connection lost; reconnecting");
        std::thread::sleep(Duration::from_secs(2));
    }
}
