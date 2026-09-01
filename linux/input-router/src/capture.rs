//! EI (emulated input) receiver thread: reads captured input events from the
//! compositor over the fd handed out by ConnectToEIS and forwards them to the
//! router core. Events only flow while a capture is active.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;

use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

use crate::router::Event;

#[derive(Debug)]
pub enum CaptureEvent {
    /// Evdev keycode + pressed
    Key(u32, bool),
    /// Evdev button code (BTN_LEFT=0x110...) + pressed
    Button(u32, bool),
    /// Relative motion, device units
    Motion(f64, f64),
    /// Discrete scroll steps (positive = down/right in evdev convention)
    ScrollDiscrete(i32, i32),
    /// Continuous scroll, logical-pixel units (touchpads, hi-res wheels)
    ScrollDelta(f32, f32),
    /// Batch boundary: apply accumulated motion now
    Frame,
    /// EI stream ended (session closed compositor-side)
    Disconnected,
}

pub fn spawn_ei_thread(stream: UnixStream, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let context = match ei::Context::new(stream) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[capture] EI context failed: {e}");
                let _ = tx.send(Event::Capture(CaptureEvent::Disconnected));
                return;
            }
        };
        let handshake =
            context.handshake_blocking("beaglewing-router", ei::handshake::ContextType::Receiver);
        let (_connection, events) = match handshake {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[capture] EI handshake failed: {e:?}");
                let _ = tx.send(Event::Capture(CaptureEvent::Disconnected));
                return;
            }
        };
        println!("[capture] EI receiver connected");

        for event in events {
            let Ok(event) = event else { break };
            let mapped = match event {
                EiEvent::SeatAdded(s) => {
                    // Mandatory: the compositor streams nothing until the
                    // client binds the seat's capabilities.
                    s.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll,
                    );
                    let _ = context.flush();
                    println!("[capture] seat bound (keyboard/pointer/button/scroll)");
                    None
                }
                EiEvent::KeyboardKey(k) => {
                    Some(CaptureEvent::Key(k.key, k.state == ei::keyboard::KeyState::Press))
                }
                EiEvent::Button(b) => {
                    Some(CaptureEvent::Button(b.button, b.state == ei::button::ButtonState::Press))
                }
                EiEvent::PointerMotion(m) => {
                    Some(CaptureEvent::Motion(m.dx as f64, m.dy as f64))
                }
                EiEvent::ScrollDiscrete(s) => {
                    Some(CaptureEvent::ScrollDiscrete(s.discrete_dx as i32, s.discrete_dy as i32))
                }
                EiEvent::ScrollDelta(s) => {
                    Some(CaptureEvent::ScrollDelta(s.dx, s.dy))
                }
                EiEvent::Frame(_) => Some(CaptureEvent::Frame),
                EiEvent::Disconnected(_) => Some(CaptureEvent::Disconnected),
                _ => None,
            };
            if let Some(ev) = mapped {
                let disconnect = matches!(ev, CaptureEvent::Disconnected);
                if tx.send(Event::Capture(ev)).is_err() || disconnect {
                    return;
                }
            }
        }
        let _ = tx.send(Event::Capture(CaptureEvent::Disconnected));
    });
}
