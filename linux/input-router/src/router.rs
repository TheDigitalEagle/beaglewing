//! Router core: merges portal control signals and captured EI input, drives
//! the geometry model, and speaks to the Pi bridge.
//!
//! Safety invariants (docs/engineering-rules.md):
//! - every transition to local sends RELEASE_ALL to the bridge;
//! - a dead bridge connection while remote => immediately Release capture
//!   so input is never routed into a void;
//! - emergency hotkey (LCtrl+LAlt+End) forces return to local;
//! - Ctrl-C cleans up both sides.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::capture::CaptureEvent;
use crate::geometry::{Crossing, Edge, Screen, Space, Target};
use crate::hidmap;
use crate::portal::{InputCapture, PortalEvent, Zone};
use crate::protocol::BridgeClient;

pub enum Event {
    Portal(PortalEvent),
    Capture(CaptureEvent),
    /// GNOME screensaver state: true = locked.
    ScreenSaver(bool),
}

pub struct Config {
    pub bridge_addr: String,
    pub remote: Screen,
    pub verbose: bool,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Fetch zones and place the right-edge barrier. Compositors differ on the
/// exact boundary coordinate convention, so try candidates until one
/// sticks. Reused for the initial arm and for post-unlock re-arms.
fn arm_barriers(portal: &InputCapture) -> Result<Zone, String> {
    let (zones, zone_set) = portal.zones().map_err(|e| format!("GetZones: {e}"))?;
    let zone: Zone = *zones.first().ok_or("no zones reported")?;
    println!(
        "[router] local zone {}x{} at ({},{}), zone_set {}",
        zone.width, zone.height, zone.x, zone.y, zone_set
    );
    let w = zone.width as i32;
    let h = zone.height as i32;
    let candidates = [
        (zone.x + w, zone.y, zone.x + w, zone.y + h - 1),
        (zone.x + w - 1, zone.y, zone.x + w - 1, zone.y + h - 1),
        (zone.x + w, zone.y, zone.x + w, zone.y + h),
    ];
    for pos in candidates {
        let failed = portal
            .set_barrier(1, pos, zone_set)
            .map_err(|e| format!("SetPointerBarriers: {e}"))?;
        if failed.is_empty() {
            println!("[router] right-edge barrier accepted at {pos:?}");
            return Ok(zone);
        }
        println!("[router] barrier {pos:?} rejected, trying next candidate");
    }
    Err("compositor rejected all barrier candidates".into())
}

fn token_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".config/beaglewing/restore_token")
}

fn load_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_token(token: &str) {
    let p = token_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&p, token).is_ok() {
        println!("[router] restore_token saved ({})", p.display());
    }
}

struct Emergency {
    held: [bool; 3], // LCtrl, LAlt, Backslash
}

impl Emergency {
    fn new() -> Self {
        Emergency { held: [false; 3] }
    }
    /// Track a usage; returns true when the full combo is held.
    fn update(&mut self, usage: u8, pressed: bool) -> bool {
        match usage {
            0xe0 => self.held[0] = pressed,
            0xe2 => self.held[1] = pressed,
            0x31 => self.held[2] = pressed, // backslash
            _ => {}
        }
        self.held.iter().all(|&h| h)
    }
    fn reset(&mut self) {
        self.held = [false; 3];
    }
}

pub fn run(cfg: Config, tx: Sender<Event>, rx: Receiver<Event>) -> Result<(), String> {
    unsafe {
        let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }

    // --- Portal setup -----------------------------------------------------
    println!("[router] creating portal session (a consent dialog may appear)...");
    let portal = InputCapture::new().map_err(|e| format!("portal session: {e}"))?;
    let (caps, new_token) = portal
        .start(load_token().as_deref())
        .map_err(|e| format!("portal start: {e}"))?;
    println!("[router] capture granted, capabilities={caps}");
    if let Some(t) = &new_token {
        save_token(t);
    } else {
        println!("[router] no restore_token from portal (expect a dialog next start)");
    }

    let eis_fd = portal.connect_to_eis().map_err(|e| format!("ConnectToEIS: {e}"))?;
    let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(eis_fd));
    crate::capture::spawn_ei_thread(stream, tx.clone());

    let zone = arm_barriers(&portal)?;

    portal
        .spawn_signal_thread(tx.clone())
        .map_err(|e| format!("signal subscribe: {e}"))?;
    if let Err(e) = InputCapture::spawn_screensaver_watcher(tx.clone()) {
        println!("[router] screensaver watcher unavailable ({e}); lock recovery disabled");
    }
    portal.enable().map_err(|e| format!("Enable: {e}"))?;
    println!("[router] armed: push the cursor through the RIGHT edge to switch");
    println!("[router] emergency return: LCtrl+LAlt+Backslash; Ctrl-C here to quit");

    // --- Bridge -----------------------------------------------------------
    let mut bridge = match BridgeClient::connect(&cfg.bridge_addr) {
        Ok(b) => {
            println!("[router] bridge connected at {}", cfg.bridge_addr);
            Some(b)
        }
        Err(e) => {
            println!("[router] bridge unavailable ({e}); will retry in background");
            None
        }
    };

    // --- Core state -------------------------------------------------------
    let mut space = Space::new(
        Screen { width: zone.width as f64, height: zone.height as f64 },
        cfg.remote,
        Edge::Right,
    );
    let mut active_id: Option<u32> = None;
    // Failsafe: if a capture activates but no events flow, release rather
    // than leave the user's input routed into a void.
    let mut activated_at = Instant::now();
    let mut events_this_activation = false;
    let mut last_capture_event = Instant::now();
    let mut gap_times: Vec<Instant> = Vec::new();
    let mut emergency = Emergency::new();
    let mut acc_dx = 0.0f64;
    let mut acc_dy = 0.0f64;
    let mut acc_scroll = 0.0f64;
    let mut buttons = 0u8;
    let mut last_send = Instant::now();
    let mut last_reconnect = Instant::now();
    // Positions are coalesced: only the latest matters (absolute protocol),
    // sent at most every POINTER_SEND_INTERVAL so the USB endpoint on the
    // Pi never builds a backlog ("ice skating" lag).
    const POINTER_SEND_INTERVAL: Duration = Duration::from_millis(4);
    let mut pending_pos: Option<(u16, u16)> = None;
    // Wheel detents coalesce on the same cadence: a hi-res wheel can emit
    // hundreds of events/sec, and a per-event HID report starves motion.
    let mut pending_wheel: i32 = 0;
    let mut last_ptr_send = Instant::now();

    // Return capture to the compositor, placing the local cursor at the
    // right edge with the model's current (already re-seated) local y.
    let go_local = |portal: &InputCapture,
                    space: &mut Space,
                    bridge: &mut Option<BridgeClient>,
                    active_id: &mut Option<u32>,
                    buttons: &mut u8,
                    why: &str| {
        if let Some(b) = bridge.as_mut() {
            let _ = b.release_all();
        }
        *buttons = 0;
        space.force_local();
        if let Some(id) = active_id.take() {
            let cursor = (
                zone.x as f64 + space.x,
                zone.y as f64 + space.y,
            );
            if let Err(e) = portal.release(id, Some(cursor)) {
                println!("[router] Release failed: {e}");
            }
        }
        println!("[router] -> local ({why})");
        // The clipboard agent uses crossings to trigger lazy transfers.
        crate::clipboard::send_control("local");
    };

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons, "shutdown");
            println!("[router] clean shutdown");
            return Ok(());
        }

        // Bridge keepalive / reconnect.
        let mut bridge_dead = false;
        if let Some(b) = bridge.as_mut() {
            if last_send.elapsed() > Duration::from_secs(1) {
                if b.keepalive().is_err() {
                    bridge_dead = true;
                } else {
                    last_send = Instant::now();
                }
            }
        }
        if bridge_dead {
            println!("[router] bridge connection lost");
            bridge = None;
            if active_id.is_some() {
                go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                         "bridge lost while remote");
            }
        }
        if bridge.is_none() && last_reconnect.elapsed() > Duration::from_secs(2) {
            last_reconnect = Instant::now();
            if let Ok(mut b) = BridgeClient::connect(&cfg.bridge_addr) {
                let _ = b.release_all();
                println!("[router] bridge reconnected");
                bridge = Some(b);
                last_send = Instant::now();
            }
        }

        if active_id.is_some()
            && !events_this_activation
            && activated_at.elapsed() > Duration::from_secs(3)
        {
            go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                     "FAILSAFE: capture active but no events arriving");
        }

        // Coalesced pointer state is only meaningful while remote.
        if active_id.is_none() {
            pending_pos = None;
            pending_wheel = 0;
        }
        // Flush coalesced position/wheel once the throttle window opens.
        if (pending_pos.is_some() || pending_wheel != 0)
            && last_ptr_send.elapsed() >= POINTER_SEND_INTERVAL
        {
            if let Some(b) = bridge.as_mut() {
                let mut ok = true;
                if let Some(pos) = pending_pos.take() {
                    ok &= b.pointer_abs(pos.0, pos.1).is_ok();
                }
                if pending_wheel != 0 {
                    let detents = pending_wheel.clamp(-127, 127) as i8;
                    pending_wheel = 0;
                    ok &= b.pointer_wheel(detents).is_ok();
                }
                if ok {
                    last_send = Instant::now();
                }
            } else {
                pending_pos = None;
                pending_wheel = 0;
            }
            last_ptr_send = Instant::now();
        }

        let timeout = if pending_pos.is_some() || pending_wheel != 0 {
            POINTER_SEND_INTERVAL
        } else {
            Duration::from_millis(300)
        };
        let ev = match rx.recv_timeout(timeout) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err("event channel closed".into()),
        };

        match ev {
            Event::Portal(PortalEvent::Activated { activation_id, cursor_x, cursor_y }) => {
                if bridge.is_none() {
                    // Never capture into a void.
                    active_id = Some(activation_id);
                    go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                             "no bridge; refusing capture");
                    continue;
                }
                active_id = Some(activation_id);
                activated_at = Instant::now();
                events_this_activation = false;
                emergency.reset();
                acc_dx = 0.0;
                acc_dy = 0.0;
                acc_scroll = 0.0;
                pending_pos = None;
                pending_wheel = 0;
                let local_y = cursor_y - zone.y as f64;
                space.enter_remote(local_y);
                if cfg.verbose {
                    println!("[router] Activated id={activation_id} at ({cursor_x:.0},{cursor_y:.0})");
                }
                if let Some(b) = bridge.as_mut() {
                    let (cx, cy) = space.canonical();
                    let _ = b.pointer_abs(cx, cy);
                    last_send = Instant::now();
                }
                println!("[router] -> remote");
                crate::clipboard::send_control("remote");
            }
            Event::Portal(PortalEvent::Deactivated) => {
                if active_id.is_some() {
                    go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                             "compositor deactivated");
                }
            }
            Event::Portal(PortalEvent::Disabled) => {
                println!("[router] capture disabled by compositor; re-arming");
                if active_id.is_some() {
                    go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                             "disabled");
                }
                std::thread::sleep(Duration::from_millis(500));
                if let Err(e) = portal.enable() {
                    return Err(format!("re-enable failed: {e}"));
                }
            }
            Event::ScreenSaver(true) => {
                println!("[router] screen locked");
                if active_id.is_some() {
                    go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                             "screen locked");
                }
            }
            Event::ScreenSaver(false) => {
                // A lock kills the barriers without any portal signal.
                // Re-arm inside the existing session (no consent needed);
                // if the session itself died, exit and let systemd restart
                // us, which puts the consent dialog up while the user is
                // right there at the unlocked machine.
                println!("[router] screen unlocked; re-arming barriers");
                let _ = portal.disable();
                arm_barriers(&portal).map_err(|e| format!("re-arm after unlock: {e}"))?;
                portal
                    .enable()
                    .map_err(|e| format!("re-enable after unlock: {e}"))?;
                println!("[router] re-armed after unlock");
            }
            Event::Portal(PortalEvent::ZonesChanged) => {
                println!("[router] zones changed (display reconfigured); exiting for a clean restart");
                go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons, "zones changed");
                return Err("zones changed".into());
            }

            Event::Capture(cev) => {
                if active_id.is_none() {
                    continue; // stray events while local
                }
                // Dip diagnostics. An idle user produces one long gap, which
                // is normal; a choppy stall produces a cluster of short gaps
                // while motion continues. Log only the cluster pattern: it
                // means the stall is upstream of us (compositor/EI), whereas
                // a felt dip with no cluster here and a clean network points
                // at the target host.
                if events_this_activation {
                    let gap = last_capture_event.elapsed();
                    if gap > Duration::from_millis(100) && gap < Duration::from_millis(500) {
                        gap_times.push(Instant::now());
                        gap_times.retain(|t| t.elapsed() < Duration::from_secs(10));
                        if gap_times.len() >= 5 {
                            println!(
                                "[router] choppy EI delivery: {} gaps of 100-500ms within 10s",
                                gap_times.len()
                            );
                            gap_times.clear();
                        }
                    }
                }
                last_capture_event = Instant::now();
                events_this_activation = true;
                match cev {
                    CaptureEvent::Key(code, pressed) => {
                        let Some(usage) = hidmap::keycode_to_usage(code as u16) else {
                            if cfg.verbose {
                                println!("[router] unmapped keycode {code}");
                            }
                            continue;
                        };
                        if emergency.update(usage, pressed) {
                            emergency.reset();
                            go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                                     "emergency hotkey");
                            continue;
                        }
                        if let Some(b) = bridge.as_mut() {
                            let r = if pressed { b.key_down(usage) } else { b.key_up(usage) };
                            if r.is_ok() {
                                last_send = Instant::now();
                            }
                            if cfg.verbose {
                                println!("[router] key {code} -> usage {usage:#04x} {}",
                                         if pressed { "down" } else { "up" });
                            }
                        }
                    }
                    CaptureEvent::Button(code, pressed) => {
                        let Some(bit) = hidmap::button_to_bit(code as u16) else { continue };
                        if pressed {
                            buttons |= bit;
                        } else {
                            buttons &= !bit;
                        }
                        if let Some(b) = bridge.as_mut() {
                            // Clicks must land at the newest position:
                            // flush any coalesced motion first.
                            if let Some(pos) = pending_pos.take() {
                                let _ = b.pointer_abs(pos.0, pos.1);
                                last_ptr_send = Instant::now();
                            }
                            if b.pointer_button(buttons).is_ok() {
                                last_send = Instant::now();
                            }
                        }
                    }
                    CaptureEvent::Motion(dx, dy) => {
                        acc_dx += dx;
                        acc_dy += dy;
                    }
                    CaptureEvent::ScrollDiscrete(_, y) => {
                        if cfg.verbose {
                            println!("[router] scroll discrete raw y={y}");
                        }
                        // EI spec counts discrete scroll in 120ths per
                        // detent, but tolerate compositors that send 1s.
                        let clicks = if y.abs() >= 120 { y / 120 } else { y };
                        // EI positive = scroll down; HID wheel positive = up.
                        pending_wheel -= clicks;
                    }
                    CaptureEvent::ScrollDelta(_, dy) => {
                        if cfg.verbose {
                            println!("[router] scroll delta raw dy={dy}");
                        }
                        // Continuous units; ~15 logical px per wheel detent.
                        acc_scroll += dy as f64;
                        let clicks = (acc_scroll / 15.0).trunc() as i32;
                        if clicks != 0 {
                            acc_scroll -= clicks as f64 * 15.0;
                            pending_wheel -= clicks;
                        }
                    }
                    CaptureEvent::Frame => {
                        if acc_dx == 0.0 && acc_dy == 0.0 {
                            continue;
                        }
                        let crossing = space.apply_delta(acc_dx, acc_dy);
                        acc_dx = 0.0;
                        acc_dy = 0.0;
                        match crossing {
                            Crossing::ToLocal => {
                                go_local(&portal, &mut space, &mut bridge, &mut active_id,
                                         &mut buttons, "crossed back");
                            }
                            _ => {
                                if space.target == Target::Remote {
                                    pending_pos = Some(space.canonical());
                                }
                            }
                        }
                    }
                    CaptureEvent::Disconnected => {
                        go_local(&portal, &mut space, &mut bridge, &mut active_id, &mut buttons,
                                 "EI disconnected");
                        return Err("EI stream disconnected".into());
                    }
                }
            }
        }
    }
}
