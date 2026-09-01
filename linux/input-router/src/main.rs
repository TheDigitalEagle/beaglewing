//! beaglewing-router: Ubuntu-side input router (Phase 4, in progress).
//!
//! Working today:
//!   list       enumerate evdev input devices
//!   observe    print events from one device (read-only; --grab opt-in)
//!   send-test  drive the Pi bridge over TCP: pointer diamond + marker text
//!
//! The capture strategy (InputCapture portal vs raw evdev grab) is an open
//! architecture decision; see docs/input-routing.md. The geometry, HID
//! mapping, and protocol client here serve either choice.

// geometry (and parts of protocol) are consumed by the routing loop, which
// lands once the capture-strategy decision is made; keep them warning-free
// until then.
mod capture;
mod clipboard;
mod geometry;
mod hidmap;
mod portal;
mod protocol;
mod router;

use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "usage: beaglewing-router <command>\n\
         \n\
         commands:\n\
           run [--bridge HOST:PORT] [--remote-size WxH] [--verbose]\n\
                                      route input via the InputCapture portal\n\
           clipboard [--relay HOST:PORT]   sync clipboard with Windows\n\
           pull                       fetch/send any pending file offer now\n\
           list                       enumerate evdev input devices\n\
           observe --device PATH [--grab]   print events (Ctrl-C to stop)\n\
           send-test [--host HOST:PORT] [--click]   exercise the bridge"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("clipboard") => {
            let mut relay = "beaglewing:4871".to_string();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--relay" => relay = it.next().cloned().unwrap_or_else(|| usage()),
                    _ => usage(),
                }
            }
            clipboard::run(&relay)
        }
        Some("pull") => {
            if clipboard::send_control("pull") {
                println!("pull requested; the clipboard agent logs progress");
            } else {
                eprintln!("clipboard agent not running (no control socket)");
                std::process::exit(1);
            }
        }
        Some("list") => cmd_list(),
        Some("observe") => cmd_observe(&args[1..]),
        Some("send-test") => cmd_send_test(&args[1..]),
        _ => usage(),
    }
}

fn cmd_run(args: &[String]) {
    let mut bridge_addr = "beaglewing:4870".to_string();
    let mut remote = geometry::Screen { width: 1920.0, height: 1080.0 };
    let mut verbose = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bridge" => bridge_addr = it.next().cloned().unwrap_or_else(|| usage()),
            "--remote-size" => {
                let v = it.next().cloned().unwrap_or_else(|| usage());
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage());
                remote = geometry::Screen {
                    width: w.parse().unwrap_or_else(|_| usage()),
                    height: h.parse().unwrap_or_else(|_| usage()),
                };
            }
            "--verbose" => verbose = true,
            _ => usage(),
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let cfg = router::Config { bridge_addr, remote, verbose };
    if let Err(e) = router::run(cfg, tx, rx) {
        eprintln!("router exited: {e}");
        std::process::exit(1);
    }
}

fn cmd_list() {
    let mut devices: Vec<_> = evdev::enumerate().collect();
    if devices.is_empty() {
        eprintln!(
            "no readable devices in /dev/input (permission? try sudo or the input group)"
        );
        std::process::exit(1);
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, dev) in devices {
        let name = dev.name().unwrap_or("<unnamed>");
        let keys = dev
            .supported_keys()
            .map(|k| k.iter().count())
            .unwrap_or(0);
        let rel = dev.supported_relative_axes().is_some();
        println!(
            "{}  {:40}  keys={:<4} relative={}",
            path.display(),
            name,
            keys,
            if rel { "yes" } else { "no" }
        );
    }
}

fn cmd_observe(args: &[String]) {
    let mut device = None;
    let mut grab = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next().cloned(),
            "--grab" => grab = true,
            _ => usage(),
        }
    }
    let path = device.unwrap_or_else(|| usage());

    let mut dev = evdev::Device::open(&path).unwrap_or_else(|e| {
        eprintln!("cannot open {path}: {e}");
        std::process::exit(1);
    });
    println!("observing {} ({})", path, dev.name().unwrap_or("<unnamed>"));
    if grab {
        // Exclusive grab: ONLY on explicit request. The kernel releases the
        // grab automatically when this process exits for any reason.
        dev.grab().expect("EVIOCGRAB failed");
        println!("GRABBED: this device no longer reaches the desktop; Ctrl-C to release");
    }
    loop {
        for ev in dev.fetch_events().expect("fetch_events") {
            use evdev::InputEventKind as K;
            match ev.kind() {
                K::Key(k) => {
                    let code = k.code();
                    let action = match ev.value() {
                        0 => "up",
                        1 => "down",
                        _ => "repeat",
                    };
                    let usage = hidmap::keycode_to_usage(code)
                        .map(|u| format!("usage={u:#04x}"))
                        .or_else(|| hidmap::button_to_bit(code).map(|b| format!("button={b:#04x}")))
                        .unwrap_or_else(|| "UNMAPPED".into());
                    println!("key {code:4} {action:6} {usage}");
                }
                K::RelAxis(a) => println!("rel {a:?} {:+}", ev.value()),
                _ => {}
            }
        }
    }
}

fn cmd_send_test(args: &[String]) {
    let mut host = "beaglewing:4870".to_string();
    let mut click = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it.next().cloned().unwrap_or_else(|| usage()),
            "--click" => click = true,
            _ => usage(),
        }
    }

    let mut c = protocol::BridgeClient::connect(&host).unwrap_or_else(|e| {
        eprintln!("cannot connect to bridge at {host}: {e}");
        std::process::exit(1);
    });
    println!("connected to {host}, protocol v{}", protocol::VERSION);

    let diamond: [(u16, u16); 6] = [
        (32768, 32768), (16384, 32768), (32768, 16384),
        (49152, 32768), (32768, 49152), (32768, 32768),
    ];
    for (x, y) in diamond {
        c.pointer_abs(x, y).expect("pointer_abs");
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("pointer diamond sent");

    if click {
        c.pointer_button(0x01).expect("button down");
        std::thread::sleep(Duration::from_millis(50));
        c.pointer_button(0x00).expect("button up");
        println!("left click sent");
    }

    // "router ok\n" as HID usages
    for usage in [0x15, 0x12, 0x18, 0x17, 0x08, 0x15, 0x2c, 0x12, 0x0e, 0x28u8] {
        c.key_down(usage).expect("key_down");
        std::thread::sleep(Duration::from_millis(20));
        c.key_up(usage).expect("key_up");
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("typed 'router ok'");

    c.release_all().expect("release_all");
    let st = c.status().expect("status");
    println!(
        "STATUS: modifiers={:#04x} held_keys={} buttons={:#04x} hid_ok={}",
        st.modifiers, st.held_keys, st.buttons, st.hid_ok
    );
    assert!(st.modifiers == 0 && st.held_keys == 0 && st.buttons == 0, "state not clean");
    assert!(st.hid_ok, "bridge reports HID writes failing");
    println!("send-test complete");
}
