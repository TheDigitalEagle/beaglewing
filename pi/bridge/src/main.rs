//! beaglewing-bridge: receives input commands over TCP and writes USB HID
//! reports to the gadget devices. Protocol: docs/protocol.md (v1).
//!
//! Safety property above all others: on ANY exit from a client session
//! (clean, error, timeout, malformed frame, shutdown signal) all keyboard
//! and button state is released.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u16 = 1;
const MAX_PAYLOAD: usize = 32;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const POINTER_CENTER: u16 = 32768;

// Message types (docs/protocol.md)
const MSG_HELLO: u8 = 0x01;
const MSG_HELLO_ACK: u8 = 0x02;
const MSG_KEEPALIVE: u8 = 0x03;
const MSG_KEY_DOWN: u8 = 0x10;
const MSG_KEY_UP: u8 = 0x11;
const MSG_KEY_STATE: u8 = 0x12;
const MSG_POINTER_ABS: u8 = 0x20;
const MSG_POINTER_BUTTON: u8 = 0x21;
const MSG_POINTER_WHEEL: u8 = 0x22;
const MSG_RELEASE_ALL: u8 = 0x30;
const MSG_GET_STATUS: u8 = 0x40;
const MSG_STATUS: u8 = 0x41;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn log(msg: &str) {
    println!("[bridge] {msg}");
}

struct Hid {
    kbd: File,
    ptr: File,
    modifiers: u8,
    keys: [u8; 6],
    buttons: u8,
    x: u16,
    y: u16,
    hid_ok: bool,
}

impl Hid {
    fn open(kbd_path: &str, ptr_path: &str) -> std::io::Result<Self> {
        let open = |p: &str| OpenOptions::new().write(true).open(p);
        Ok(Hid {
            kbd: open(kbd_path)?,
            ptr: open(ptr_path)?,
            modifiers: 0,
            keys: [0; 6],
            buttons: 0,
            x: POINTER_CENTER,
            y: POINTER_CENTER,
            hid_ok: true,
        })
    }

    fn write_report(&mut self, which: Which, report: &[u8]) {
        let dev = match which {
            Which::Kbd => &mut self.kbd,
            Which::Ptr => &mut self.ptr,
        };
        match dev.write_all(report) {
            Ok(()) => {
                if !self.hid_ok {
                    log("HID writes recovered");
                }
                self.hid_ok = true;
            }
            Err(e) => {
                if self.hid_ok {
                    log(&format!("HID write failed (host detached?): {e}"));
                }
                self.hid_ok = false;
            }
        }
    }

    fn flush_kbd(&mut self) {
        let mut r = [0u8; 8];
        r[0] = self.modifiers;
        r[2..8].copy_from_slice(&self.keys);
        self.write_report(Which::Kbd, &r);
    }

    fn flush_ptr(&mut self, wheel: i8) {
        let x = self.x >> 1;
        let y = self.y >> 1;
        let r = [
            self.buttons,
            (x & 0xff) as u8,
            (x >> 8) as u8,
            (y & 0xff) as u8,
            (y >> 8) as u8,
            wheel as u8,
        ];
        self.write_report(Which::Ptr, &r);
    }

    fn key_event(&mut self, usage: u8, down: bool) {
        if (0xe0..=0xe7).contains(&usage) {
            let bit = 1u8 << (usage - 0xe0);
            if down {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
        } else if down {
            if self.keys.contains(&usage) {
                return; // already held
            }
            match self.keys.iter().position(|&k| k == 0) {
                Some(slot) => self.keys[slot] = usage,
                None => {
                    log(&format!("dropping 7th held key usage {usage:#04x}"));
                    return;
                }
            }
        } else {
            for k in self.keys.iter_mut() {
                if *k == usage {
                    *k = 0;
                }
            }
        }
        self.flush_kbd();
    }

    fn release_all(&mut self) {
        let had_state =
            self.modifiers != 0 || self.buttons != 0 || self.keys.iter().any(|&k| k != 0);
        self.modifiers = 0;
        self.keys = [0; 6];
        self.buttons = 0;
        self.flush_kbd();
        self.flush_ptr(0);
        if had_state {
            log("release-all: cleared held input state");
        }
    }

    fn held_key_count(&self) -> u8 {
        self.keys.iter().filter(|&&k| k != 0).count() as u8
    }
}

enum Which {
    Kbd,
    Ptr,
}

/// Expected payload length per message type; None = unknown type.
fn expected_len(msg_type: u8) -> Option<usize> {
    Some(match msg_type {
        MSG_HELLO => 2,
        MSG_KEEPALIVE | MSG_RELEASE_ALL | MSG_GET_STATUS => 0,
        MSG_KEY_DOWN | MSG_KEY_UP | MSG_POINTER_BUTTON | MSG_POINTER_WHEEL => 1,
        MSG_KEY_STATE => 7,
        MSG_POINTER_ABS => 4,
        _ => return None,
    })
}

fn send(stream: &mut TcpStream, msg_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(2 + payload.len());
    frame.push(msg_type);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

/// Handle one client until disconnect/error/timeout. Caller releases state.
fn handle_client(stream: &mut TcpStream, hid: &mut Hid) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut last_rx = Instant::now();
    let mut greeted = false;
    // Coalesce bursts of absolute positions: only the newest needs a HID
    // report. Flushed before any button/wheel so ordering stays exact.
    let mut pending_abs: Option<(u16, u16)> = None;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return Ok(());
        }
        if last_rx.elapsed() > HEARTBEAT_TIMEOUT {
            log("heartbeat timeout; dropping client");
            return Ok(());
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()), // clean EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                last_rx = Instant::now();
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        }

        // Parse complete frames from the accumulator.
        while buf.len() >= 2 {
            let (msg_type, len) = (buf[0], buf[1] as usize);
            match expected_len(msg_type) {
                Some(want) if want == len && len <= MAX_PAYLOAD => {}
                _ => {
                    log(&format!(
                        "malformed frame (type {msg_type:#04x} len {len}); closing"
                    ));
                    return Ok(());
                }
            }
            if buf.len() < 2 + len {
                break; // incomplete frame; wait for more bytes
            }
            let payload: Vec<u8> = buf[2..2 + len].to_vec();
            buf.drain(..2 + len);

            if !greeted {
                if msg_type != MSG_HELLO {
                    log("client did not HELLO first; closing");
                    return Ok(());
                }
                let ver = u16::from_le_bytes([payload[0], payload[1]]);
                if ver != PROTOCOL_VERSION {
                    log(&format!("unsupported protocol version {ver}; closing"));
                    return Ok(());
                }
                send(stream, MSG_HELLO_ACK, &PROTOCOL_VERSION.to_le_bytes())?;
                greeted = true;
                log(&format!("client greeted, protocol v{ver}"));
                continue;
            }

            // Any non-position pointer action must see the newest position.
            let needs_flush = matches!(
                msg_type,
                MSG_POINTER_BUTTON | MSG_POINTER_WHEEL | MSG_RELEASE_ALL
            );
            if needs_flush {
                if let Some((x, y)) = pending_abs.take() {
                    hid.x = x;
                    hid.y = y;
                    hid.flush_ptr(0);
                }
            }

            match msg_type {
                MSG_KEEPALIVE => {}
                MSG_HELLO => { /* redundant HELLO: ignore */ }
                MSG_KEY_DOWN => hid.key_event(payload[0], true),
                MSG_KEY_UP => hid.key_event(payload[0], false),
                MSG_KEY_STATE => {
                    hid.modifiers = payload[0];
                    hid.keys.copy_from_slice(&payload[1..7]);
                    hid.flush_kbd();
                }
                MSG_POINTER_ABS => {
                    pending_abs = Some((
                        u16::from_le_bytes([payload[0], payload[1]]),
                        u16::from_le_bytes([payload[2], payload[3]]),
                    ));
                }
                MSG_POINTER_BUTTON => {
                    hid.buttons = payload[0] & 0x07;
                    hid.flush_ptr(0);
                }
                MSG_POINTER_WHEEL => hid.flush_ptr(payload[0] as i8),
                MSG_RELEASE_ALL => hid.release_all(),
                MSG_GET_STATUS => {
                    let status = [
                        hid.modifiers,
                        hid.held_key_count(),
                        hid.buttons,
                        hid.hid_ok as u8,
                    ];
                    send(stream, MSG_STATUS, &status)?;
                }
                _ => unreachable!("validated above"),
            }
        }

        // End of this read batch: emit one report for the newest position.
        if let Some((x, y)) = pending_abs.take() {
            hid.x = x;
            hid.y = y;
            hid.flush_ptr(0);
        }
    }
}

fn main() {
    let mut listen = "0.0.0.0:4870".to_string();
    let mut kbd = "/dev/hidg0".to_string();
    let mut ptr = "/dev/hidg1".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().unwrap_or_else(|| {
            eprintln!("missing value for {name}");
            std::process::exit(2);
        });
        match a.as_str() {
            "--listen" => listen = val("--listen"),
            "--kbd" => kbd = val("--kbd"),
            "--ptr" => ptr = val("--ptr"),
            _ => {
                eprintln!("usage: beaglewing-bridge [--listen ADDR:PORT] [--kbd DEV] [--ptr DEV]");
                std::process::exit(2);
            }
        }
    }

    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let mut hid = match Hid::open(&kbd, &ptr) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cannot open HID devices ({kbd}, {ptr}): {e}");
            eprintln!("is the gadget set up? (setup-gadget.sh)");
            std::process::exit(1);
        }
    };
    log(&format!("HID devices open: kbd={kbd} ptr={ptr}"));

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("cannot listen on {listen}: {e}");
        std::process::exit(1);
    });
    listener.set_nonblocking(true).expect("set_nonblocking");
    log(&format!("listening on {listen} (protocol v{PROTOCOL_VERSION})"));

    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                log(&format!("client connected: {peer}"));
                match handle_client(&mut stream, &mut hid) {
                    Ok(()) => log(&format!("client session ended: {peer}")),
                    Err(e) => log(&format!("client error ({peer}): {e}")),
                }
                // The safety property: no session ends with held state.
                hid.release_all();
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log(&format!("accept error: {e}"));
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    log("shutdown signal received");
    hid.release_all();
}
