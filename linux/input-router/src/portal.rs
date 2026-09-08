//! Blocking client for org.freedesktop.portal.InputCapture (docs/input-routing.md).
//!
//! Uses the CreateSession2/Start flow (with persist_mode + restore_token so
//! the consent dialog can be remembered where the compositor supports it).

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.InputCapture";

pub const CAP_KEYBOARD: u32 = 1;
pub const CAP_POINTER: u32 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Zone {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

/// Portal-originated control events, forwarded to the router core.
#[derive(Debug)]
pub enum PortalEvent {
    Activated {
        activation_id: u32,
        cursor_x: f64,
        cursor_y: f64,
    },
    Deactivated,
    Disabled,
    ZonesChanged,
}

pub struct InputCapture {
    conn: Connection,
    session: OwnedObjectPath,
    /// Portal interface version 2+ has Start/restore_token; on v1 the
    /// consent dialog happens inside CreateSession and cannot persist.
    v2: bool,
    token_counter: std::cell::Cell<u32>,
}

type Vardict = HashMap<String, OwnedValue>;

/// v1 responses carry session_handle as a string; v2 as an object path.
fn vardict_object_path(v: &OwnedValue) -> Option<OwnedObjectPath> {
    if let Ok(p) = OwnedObjectPath::try_from(v.clone()) {
        return Some(p);
    }
    let s = String::try_from(v.clone()).ok()?;
    OwnedObjectPath::try_from(s).ok()
}

impl InputCapture {
    pub fn new() -> zbus::Result<Self> {
        let conn = Connection::session()?;
        let placeholder = OwnedObjectPath::try_from("/").expect("static path");
        let mut this = InputCapture {
            conn,
            session: placeholder,
            v2: true,
            token_counter: std::cell::Cell::new(0),
        };

        // Prefer CreateSession2 (returns results directly, consent deferred
        // to Start, persistence possible). GNOME's backend may advertise it
        // but not implement it, so fall back to the v1 flow then.
        let opts: HashMap<&str, Value> = HashMap::from([
            ("session_handle_token", Value::from("beaglewing")),
            ("capabilities", Value::from(CAP_KEYBOARD | CAP_POINTER)),
        ]);
        match this
            .conn
            .call_method(Some(DEST), PATH, Some(IFACE), "CreateSession2", &(opts,))
        {
            Ok(reply) => {
                let results: Vardict = reply.body().deserialize()?;
                this.session = results
                    .get("session_handle")
                    .and_then(vardict_object_path)
                    .ok_or_else(|| zbus::Error::Failure("no session_handle".into()))?;
            }
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str().ends_with("UnknownMethod") =>
            {
                println!("[portal] CreateSession2 unavailable; using v1 flow (dialog now)");
                this.v2 = false;
                let conn = this.conn.clone();
                let (code, results) = this.request(
                    "CreateSession",
                    |opts| {
                        opts.insert("session_handle_token", Value::from("beaglewing"));
                        opts.insert("capabilities", Value::from(CAP_KEYBOARD | CAP_POINTER));
                    },
                    move |opts| {
                        conn.call_method(Some(DEST), PATH, Some(IFACE), "CreateSession", &("", opts))
                    },
                )?;
                if code != 0 {
                    return Err(zbus::Error::Failure(format!(
                        "CreateSession refused (response {code}; 1 = user cancelled)"
                    )));
                }
                this.session = results
                    .get("session_handle")
                    .and_then(vardict_object_path)
                    .ok_or_else(|| zbus::Error::Failure("no session_handle in response".into()))?;
            }
            Err(e) => return Err(e),
        }
        Ok(this)
    }

    fn proxy(&self) -> zbus::Result<Proxy<'_>> {
        Proxy::new(&self.conn, DEST, PATH, IFACE)
    }

    /// Portal request pattern: subscribe on the expected request path, call,
    /// then wait for the Response signal. Returns (response_code, results).
    fn request(
        &self,
        method: &str,
        append_token: impl FnOnce(&mut HashMap<&'static str, Value<'static>>),
        build: impl FnOnce(HashMap<&'static str, Value<'static>>) -> zbus::Result<zbus::Message>,
    ) -> zbus::Result<(u32, Vardict)> {
        let n = self.token_counter.get() + 1;
        self.token_counter.set(n);
        let token = format!("bw{n}");
        let unique = self
            .conn
            .unique_name()
            .ok_or_else(|| zbus::Error::Failure("no unique name".into()))?
            .trim_start_matches(':')
            .replace('.', "_");
        let req_path = format!("/org/freedesktop/portal/desktop/request/{unique}/{token}");

        let req_proxy = Proxy::new(
            &self.conn,
            DEST,
            req_path.clone(),
            "org.freedesktop.portal.Request",
        )?;
        let mut signals = req_proxy.receive_signal("Response")?;

        let mut opts: HashMap<&'static str, Value<'static>> = HashMap::new();
        opts.insert("handle_token", Value::from(token.clone()));
        append_token(&mut opts);
        let reply = build(opts)?;
        let actual: OwnedObjectPath = reply.body().deserialize()?;
        if actual.as_str() != req_path {
            // Old-style server-assigned path; re-subscribe there.
            let p = Proxy::new(&self.conn, DEST, actual, "org.freedesktop.portal.Request")?;
            signals = p.receive_signal("Response")?;
        }

        let msg = signals
            .next()
            .ok_or_else(|| zbus::Error::Failure(format!("{method}: no Response")))?;
        let (code, results): (u32, Vardict) = msg.body().deserialize()?;
        Ok((code, results))
    }

    /// Start the session (on v2 this is where the consent dialog appears).
    /// Returns (granted capabilities, restore_token if the portal gave one).
    /// On a v1 portal the dialog already happened in CreateSession; this is
    /// a no-op reporting the requested capabilities.
    pub fn start(&self, restore_token: Option<&str>) -> zbus::Result<(u32, Option<String>)> {
        if !self.v2 {
            return Ok((CAP_KEYBOARD | CAP_POINTER, None));
        }
        let session = self.session.clone();
        let conn = self.conn.clone();
        let (code, results) = self.request(
            "Start",
            |opts| {
                opts.insert("capabilities", Value::from(CAP_KEYBOARD | CAP_POINTER));
                // 2 = persist until explicitly revoked. Ignored by portals
                // without persistence support.
                opts.insert("persist_mode", Value::from(2u32));
                if let Some(t) = restore_token {
                    opts.insert("restore_token", Value::from(t.to_string()));
                }
            },
            move |opts| conn.call_method(Some(DEST), PATH, Some(IFACE), "Start", &(&session, "", opts)),
        )?;
        if code != 0 {
            return Err(zbus::Error::Failure(format!(
                "Start refused (response {code}; 1 = user cancelled)"
            )));
        }
        let caps = results
            .get("capabilities")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let token = results
            .get("restore_token")
            .and_then(|v| String::try_from(v.clone()).ok());
        Ok((caps, token))
    }

    pub fn connect_to_eis(&self) -> zbus::Result<OwnedFd> {
        let opts: HashMap<&str, Value> = HashMap::new();
        let reply = self.conn.call_method(
            Some(DEST),
            PATH,
            Some(IFACE),
            "ConnectToEIS",
            &(&self.session, opts),
        )?;
        reply.body().deserialize()
    }

    /// Returns (zones, zone_set id).
    pub fn zones(&self) -> zbus::Result<(Vec<Zone>, u32)> {
        let session = self.session.clone();
        let conn = self.conn.clone();
        let (code, results) = self.request(
            "GetZones",
            |_| {},
            move |opts| conn.call_method(Some(DEST), PATH, Some(IFACE), "GetZones", &(&session, opts)),
        )?;
        if code != 0 {
            return Err(zbus::Error::Failure(format!("GetZones failed ({code})")));
        }
        let raw: Vec<(u32, u32, i32, i32)> = results
            .get("zones")
            .cloned()
            .ok_or_else(|| zbus::Error::Failure("no zones".into()))?
            .try_into()
            .map_err(|_| zbus::Error::Failure("bad zones type".into()))?;
        let zone_set = results
            .get("zone_set")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let zones = raw
            .into_iter()
            .map(|(width, height, x, y)| Zone { width, height, x, y })
            .collect();
        Ok((zones, zone_set))
    }

    /// One vertical barrier line; returns barrier ids the compositor rejected.
    pub fn set_barrier(
        &self,
        id: u32,
        (x1, y1, x2, y2): (i32, i32, i32, i32),
        zone_set: u32,
    ) -> zbus::Result<Vec<u32>> {
        let session = self.session.clone();
        let conn = self.conn.clone();
        let (code, results) = self.request(
            "SetPointerBarriers",
            |_| {},
            move |opts| {
                let barrier: HashMap<&str, Value> = HashMap::from([
                    ("barrier_id", Value::from(id)),
                    ("position", Value::from((x1, y1, x2, y2))),
                ]);
                conn.call_method(
                    Some(DEST),
                    PATH,
                    Some(IFACE),
                    "SetPointerBarriers",
                    &(&session, opts, vec![barrier], zone_set),
                )
            },
        )?;
        if code != 0 {
            return Err(zbus::Error::Failure(format!(
                "SetPointerBarriers failed ({code})"
            )));
        }
        let failed: Vec<u32> = results
            .get("failed_barriers")
            .and_then(|v| Vec::<u32>::try_from(v.clone()).ok())
            .unwrap_or_default();
        Ok(failed)
    }

    pub fn enable(&self) -> zbus::Result<()> {
        let opts: HashMap<&str, Value> = HashMap::new();
        self.conn
            .call_method(Some(DEST), PATH, Some(IFACE), "Enable", &(&self.session, opts))?;
        Ok(())
    }

    pub fn disable(&self) -> zbus::Result<()> {
        let opts: HashMap<&str, Value> = HashMap::new();
        self.conn
            .call_method(Some(DEST), PATH, Some(IFACE), "Disable", &(&self.session, opts))?;
        Ok(())
    }

    /// Give input back to the compositor, warping the local cursor to
    /// `cursor` (zone coordinates) if provided.
    pub fn release(&self, activation_id: u32, cursor: Option<(f64, f64)>) -> zbus::Result<()> {
        let mut opts: HashMap<&str, Value> = HashMap::new();
        opts.insert("activation_id", Value::from(activation_id));
        if let Some((x, y)) = cursor {
            opts.insert("cursor_position", Value::from((x, y)));
        }
        self.conn
            .call_method(Some(DEST), PATH, Some(IFACE), "Release", &(&self.session, opts))?;
        Ok(())
    }

    /// Watch GNOME's screensaver state: a lock silently kills the
    /// compositor's pointer barriers without any portal signal, so the
    /// router re-arms on unlock (docs/input-routing.md, known issue).
    pub fn spawn_screensaver_watcher(tx: Sender<crate::router::Event>) -> zbus::Result<()> {
        let conn = Connection::session()?;
        let proxy = Proxy::new(
            &conn,
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        )?;
        let mut signals = proxy.receive_signal("ActiveChanged")?;
        std::thread::spawn(move || {
            while let Some(msg) = signals.next() {
                if let Ok(active) = msg.body().deserialize::<bool>() {
                    if tx.send(crate::router::Event::ScreenSaver(active)).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// Spawn a thread forwarding this session's portal signals into `tx`.
    pub fn spawn_signal_thread(&self, tx: Sender<crate::router::Event>) -> zbus::Result<()> {
        let proxy = self.proxy()?;
        let session = self.session.clone();
        let mut streams = vec![
            ("Activated", proxy.receive_signal("Activated")?),
            ("Deactivated", proxy.receive_signal("Deactivated")?),
            ("Disabled", proxy.receive_signal("Disabled")?),
            ("ZonesChanged", proxy.receive_signal("ZonesChanged")?),
        ];
        // zbus blocking SignalIterators each need their own thread.
        for (name, mut stream) in streams.drain(..) {
            let tx = tx.clone();
            let session = session.clone();
            std::thread::spawn(move || {
                while let Some(msg) = stream.next() {
                    let Ok((handle, opts)) =
                        msg.body().deserialize::<(OwnedObjectPath, Vardict)>()
                    else {
                        continue;
                    };
                    if handle != session {
                        continue;
                    }
                    let ev = match name {
                        "Activated" => {
                            let id = opts
                                .get("activation_id")
                                .and_then(|v| u32::try_from(v).ok())
                                .unwrap_or(0);
                            let (cx, cy) = opts
                                .get("cursor_position")
                                .and_then(|v| <(f64, f64)>::try_from(v.clone()).ok())
                                .unwrap_or((0.0, 0.0));
                            PortalEvent::Activated {
                                activation_id: id,
                                cursor_x: cx,
                                cursor_y: cy,
                            }
                        }
                        "Deactivated" => PortalEvent::Deactivated,
                        "Disabled" => PortalEvent::Disabled,
                        _ => PortalEvent::ZonesChanged,
                    };
                    if tx.send(crate::router::Event::Portal(ev)).is_err() {
                        return;
                    }
                }
            });
        }
        Ok(())
    }
}
