//! Status icon (StatusNotifierItem, the KDE/AppIndicator standard): shows
//! the router's state at a glance and gives one-click control over the
//! services. Works on KDE and on GNOME with the AppIndicator extension
//! (Ubuntu enables it by default). Icons are drawn in-process, so there
//! is nothing to install into an icon theme.

use std::process::Command;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Icon, Status, ToolTip, Tray};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RouterState {
    Stopped,
    NoBridge,
    Local,
    Remote,
}

struct BeaglewingTray {
    router: RouterState,
    router_at_login: bool,
    clipboard_running: bool,
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn observe() -> (RouterState, bool, bool) {
    let router = if !systemctl(&["is-active", "--quiet", "beaglewing-router"]) {
        RouterState::Stopped
    } else {
        match std::fs::read_to_string(crate::router::state_path())
            .unwrap_or_default()
            .trim()
        {
            "remote" => RouterState::Remote,
            "no-bridge" => RouterState::NoBridge,
            _ => RouterState::Local,
        }
    };
    (
        router,
        systemctl(&["is-enabled", "--quiet", "beaglewing-router"]),
        systemctl(&["is-active", "--quiet", "beaglewing-clipboard"]),
    )
}

/// A filled, anti-aliased disc in the state's color; a thin ring when the
/// router is stopped so "off" still reads as our icon.
fn draw_icon(state: RouterState, size: i32) -> Icon {
    let (r, g, b) = match state {
        RouterState::Stopped => (0x90, 0x90, 0x90),
        RouterState::NoBridge => (0xE8, 0x5D, 0x2E),
        RouterState::Local => (0x3C, 0xB8, 0x64),
        RouterState::Remote => (0x3A, 0x82, 0xE6),
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let c = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let coverage = (radius - d + 0.5).clamp(0.0, 1.0);
            let inner = if state == RouterState::Stopped {
                // ring: hollow inside
                (d - (radius - 2.5) + 0.5).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let a = (coverage * inner * 255.0) as u8;
            data.extend_from_slice(&[a, r, g, b]);
        }
    }
    Icon { width: size, height: size, data }
}

fn describe(state: RouterState) -> &'static str {
    match state {
        RouterState::Stopped => "Router stopped",
        RouterState::NoBridge => "Router running, Pi bridge unreachable",
        RouterState::Local => "Input: this machine",
        RouterState::Remote => "Input: other machine",
    }
}

impl Tray for BeaglewingTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "beaglewing".into()
    }

    fn title(&self) -> String {
        "Beaglewing".into()
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![draw_icon(self.router, 22), draw_icon(self.router, 32), draw_icon(self.router, 48)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Beaglewing".into(),
            description: format!(
                "{}\nClipboard sync: {}",
                describe(self.router),
                if self.clipboard_running { "running" } else { "stopped" }
            ),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let router_running = self.router != RouterState::Stopped;
        vec![
            StandardItem {
                label: describe(self.router).into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: if router_running { "Stop router" } else { "Start router" }.into(),
                activate: Box::new(move |t: &mut Self| {
                    systemctl(&[if router_running { "stop" } else { "start" }, "beaglewing-router"]);
                    t.refresh();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Restart router".into(),
                enabled: router_running,
                activate: Box::new(|t: &mut Self| {
                    systemctl(&["restart", "beaglewing-router"]);
                    t.refresh();
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Start router at login".into(),
                checked: self.router_at_login,
                activate: Box::new(|t: &mut Self| {
                    let verb = if t.router_at_login { "disable" } else { "enable" };
                    systemctl(&[verb, "beaglewing-router"]);
                    t.refresh();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Fetch offered files now".into(),
                enabled: self.clipboard_running,
                activate: Box::new(|_t: &mut Self| {
                    crate::clipboard::send_control("pull");
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.clipboard_running { "Stop clipboard sync" } else { "Start clipboard sync" }
                    .into(),
                activate: Box::new(|t: &mut Self| {
                    let verb = if t.clipboard_running { "stop" } else { "start" };
                    systemctl(&[verb, "beaglewing-clipboard"]);
                    t.refresh();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit tray".into(),
                activate: Box::new(|_t: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl BeaglewingTray {
    fn refresh(&mut self) {
        let (router, at_login, clip) = observe();
        self.router = router;
        self.router_at_login = at_login;
        self.clipboard_running = clip;
    }
}

pub fn run() -> ! {
    let (router, router_at_login, clipboard_running) = observe();
    let tray = BeaglewingTray { router, router_at_login, clipboard_running };
    let handle = tray.spawn().unwrap_or_else(|e| {
        eprintln!("tray: no StatusNotifier host available ({e}); on GNOME enable the AppIndicator extension");
        std::process::exit(1);
    });
    println!("[tray] status icon up");
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if handle.is_closed() {
            std::process::exit(0);
        }
        let (router, at_login, clip) = observe();
        handle.update(|t| {
            t.router = router;
            t.router_at_login = at_login;
            t.clipboard_running = clip;
        });
    }
}
