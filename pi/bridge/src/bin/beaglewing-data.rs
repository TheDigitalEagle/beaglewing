//! beaglewing-data: dumb relay between TCP (port 4871) and the vendor raw
//! HID data channel (/dev/hidg2). Every TCP message is exactly one HID
//! report (FRAME bytes) in either direction; all protocol intelligence lives at the
//! endpoints (Ubuntu clipboard agent, Windows helper). Stateless, so a
//! plain kill is a safe stop.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAME: usize = 1024;

/// Wait until the HID node is readable/writable, waking the instant it
/// is. Sleeping instead of polling capped throughput at one frame per
/// sleep interval once frames grew to 1KB.
fn wait_fd(fd: i32, events: i16, timeout_ms: i32) {
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    unsafe {
        libc::poll(&mut pfd, 1, timeout_ms);
    }
}

/// IN-endpoint write with a bounded wait. If the host stops polling
/// (helper killed, laptop asleep) a blocking write would wedge this pump
/// forever and take the whole toward-host direction down with it; after
/// 10s of no drain the frame is dropped instead (flow control upstream
/// recovers the transfer).
fn write_frame(hid: &mut &File, buf: &[u8; FRAME]) {
    use std::os::unix::io::AsRawFd;
    let start = Instant::now();
    let mut warned = false;
    loop {
        match hid.write(buf) {
            Ok(_) => return,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if start.elapsed() > Duration::from_secs(10) {
                    log("dropping frame: host stopped draining for 10s");
                    return;
                }
                if !warned && start.elapsed() > Duration::from_secs(1) {
                    log("host not draining; holding frames");
                    warned = true;
                }
                wait_fd(hid.as_raw_fd(), libc::POLLOUT, 200);
            }
            Err(e) => {
                log(&format!("hid write error: {e}"));
                return;
            }
        }
    }
}

fn log(msg: &str) {
    println!("[data] {msg}");
}

fn main() {
    let mut listen = "0.0.0.0:4871".to_string();
    let mut hid_path = "/dev/hidg2".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = args.next().expect("--listen value"),
            "--hid" => hid_path = args.next().expect("--hid value"),
            _ => {
                eprintln!("usage: beaglewing-data [--listen ADDR:PORT] [--hid DEV]");
                std::process::exit(2);
            }
        }
    }

    // Nonblocking on both directions: reads poll gently, and writes can
    // never wedge the pump when the host stops draining.
    let hid_write = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&hid_path)
        .unwrap_or_else(|e| {
            eprintln!("cannot open {hid_path}: {e} (is the gadget set up?)");
            std::process::exit(1);
        });
    let hid_read = hid_write.try_clone().expect("clone hid fd");
    log(&format!("data channel open: {hid_path}"));

    // The one client allowed to receive host->Ubuntu frames right now.
    let current: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));

    // Host -> TCP pump: read OUT reports from the host, forward to the
    // connected agent if any, drop them otherwise (clipboard traffic is
    // transient; there is nobody to save it for).
    {
        let current = Arc::clone(&current);
        let mut hid = hid_read;
        std::thread::spawn(move || {
            let mut buf = [0u8; FRAME];
            loop {
                match hid.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        let mut guard = current.lock().unwrap();
                        if let Some(stream) = guard.as_mut() {
                            if stream.write_all(&buf[..FRAME]).is_err() {
                                *guard = None;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        use std::os::unix::io::AsRawFd;
                        wait_fd(hid.as_raw_fd(), libc::POLLIN, 1000);
                    }
                    Err(e) => {
                        log(&format!("hid read error: {e}; retrying in 1s"));
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        });
    }

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("cannot listen on {listen}: {e}");
        std::process::exit(1);
    });
    log(&format!("listening on {listen}"));

    // TCP -> host pump, one client at a time; a new connection replaces
    // the old one.
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        log(&format!("agent connected: {peer}"));
        *current.lock().unwrap() = Some(stream.try_clone().expect("clone tcp"));

        let mut hid: &File = &hid_write;
        let mut stream = stream;
        let mut buf = [0u8; FRAME];
        loop {
            match stream.read_exact(&mut buf) {
                Ok(()) => write_frame(&mut hid, &buf),
                Err(_) => break,
            }
        }
        log(&format!("agent disconnected: {peer}"));
        let mut guard = current.lock().unwrap();
        if guard
            .as_ref()
            .and_then(|s| s.peer_addr().ok())
            .map(|a| a.to_string())
            == Some(peer)
        {
            *guard = None;
        }
    }
}
