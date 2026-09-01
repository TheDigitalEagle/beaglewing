# Beaglewing

**A keyboard/mouse/clipboard bridge where the second computer sees real
USB hardware.** Slide your cursor off the edge of your Linux desktop and
it comes out on the machine next to it, carried by an actual USB device,
not a software agent. Think Synergy or Barrier, except the controlled
machine needs nothing installed, nothing configured, and no network
access to you at all.

```
              your desk, one keyboard and mouse
                            |
                            v
   +------------------+          +--------------------------+
   |  Linux desktop   |   LAN    |  Raspberry Pi 4          |
   |  (input router)  | -------> |  USB gadget "beaglewing" |
   +------------------+          +------------+-------------+
                                              | USB-C
                                              v
                                 +--------------------------+
                                 |  Any USB host: it just   |
                                 |  sees a keyboard, mouse, |
                                 |  and one vendor device   |
                                 +--------------------------+
```

## Why hardware wins

Software KVMs run an agent on every machine and ship input over the
network. That works until it does not: the agent needs installing and
updating, the target needs a network path back to you, input dies at
elevated prompts, and managed machines often cannot run any of it.

Beaglewing puts a Raspberry Pi 4 in USB gadget mode on the target's USB
port. The target sees a standards-compliant composite HID device: a
keyboard, an absolute-positioning mouse, and a small vendor data
channel. From its point of view, somebody plugged in a very good
keyboard and mouse.

| | Beaglewing | Synergy / Barrier / Deskflow |
|---|---|---|
| Software on the controlled machine | None for input; one optional unprivileged exe for clipboard | Agent required, installed and updated |
| Network exposure of the controlled machine | Zero. Even clipboard rides the USB cable | Open port or outbound connection required |
| Works at the login screen | Yes, it is a real keyboard | Usually not |
| Works at UAC / elevated prompts | Yes | Not without an elevated agent |
| Works in safe mode, installers, a frozen OS | Wherever a USB keyboard works | No |
| Admin rights needed on the target | None | Often |
| Survives target reboot | Yes, the device just re-enumerates | Agent must come back up |

The controlled machine can be as locked down as it likes. If it accepts
a keyboard, it accepts Beaglewing.

## What works today

- **Edge switching.** Push the cursor through the configured screen edge
  and input routes to the other machine; push back and you are home.
  Vertical position carries across the boundary. Uses compositor-native
  edge barriers (Wayland InputCapture portal), so local input feel is
  completely untouched while you are local.
- **Absolute pointer.** The cursor lands exactly where the model says,
  every time: no drift, no acceleration mismatch, no relative-mouse
  weirdness. Motion is coalesced and the gadget endpoints poll at 8 kHz,
  tuned until fast circles feel local.
- **Full keyboard** with correct modifier handling, plus a configurable
  emergency return hotkey (default LCtrl+LAlt+Backslash).
- **Clipboard, both directions, over USB.** Text, images, and files.
  Images are published on Windows in multiple clipboard formats, so they
  paste into both classic apps (Paint) and Chromium-based ones (Teams,
  browsers). Files are lazy: a copy only announces itself, and the
  batch moves when you cross over to the other machine (or ask for it),
  so local-only copies never transfer. Batches travel as tar with
  windowed flow control at several MB/s over 1KB HID reports, stage
  locally, and paste as ordinary local copies. The Windows side
  is one small exe that runs without admin rights and talks to the
  bridge through a vendor HID interface, QMK-style. No network, no
  driver, no installer.
- **Self-healing.** Bridge or network drops release all keys and buttons
  instantly (nobody likes a stuck modifier); the router re-arms its edge
  barriers after a screen lock; the Windows helper reopens the device
  after re-enumeration; everything restarts under systemd.

## Safety design

Input routing fails safe at every layer. The Pi releases all keys and
buttons on any disconnect, timeout, or malformed message (protocol
heartbeat, tested). The router returns input to the local machine on
bridge loss, compositor deactivation, screen lock, or the emergency
hotkey. Capture is compositor-supervised, so a wedged router cannot trap
your input. Clipboard contents are never logged on any side.

## Status

| Phase | Description | State |
|-------|-------------|-------|
| 0 | Pi 4 USB device controller bring-up | ✅ |
| 1 | HID keyboard gadget | ✅ |
| 2 | Absolute pointer gadget | ✅ |
| 3 | Pi network bridge daemon (Rust, TCP) | ✅ |
| 4 | Linux input router (InputCapture portal, edge switching) | ✅ |
| 5 | Clipboard: text | ✅ |
| 5 | Clipboard: images (incl. Chromium app paste) | ✅ |
| 5 | Clipboard: files and directories (nested, both directions) | ✅ |
| - | Windows helper autostart, latency instrumentation, config file | planned |

## Running it

Everything is systemd-managed and comes back on its own after reboots.

On the Pi (enabled at boot, nothing to do day to day):

```
beaglewing-gadget    builds and binds the USB composite gadget
beaglewing-bridge    input daemon, TCP 4870 -> HID reports
beaglewing-data      clipboard relay, TCP 4871 <-> vendor HID channel
```

On the Linux desktop (once):

```bash
cd linux/input-router && cargo build --release
cp target/release/beaglewing-router ~/.local/bin/
cp systemd/*.service ~/.config/systemd/user/   # edit --remote-size first
systemctl --user daemon-reload
systemctl --user enable --now beaglewing-clipboard
```

Then day to day:

```bash
systemctl --user start beaglewing-router     # one portal consent click per start
journalctl --user -u beaglewing-router -f    # watch it work
beaglewing-router pull                       # force a pending file transfer
```

On the target machine: run `beaglewing-clip.exe` if you want clipboard
sync (input needs nothing at all). Emergency return to local is
LCtrl+LAlt+Backslash; stopping the router always hands input back.

## Hardware

- Raspberry Pi 4 Model B running Debian 13, its USB-C port in dwc2
  peripheral mode, plugged into the target machine (which also powers
  the Pi over the same cable).
- The controlling desktop talks to the Pi over the LAN. The target never
  does.
- The Pi's USB-A ports are host-only. Never use a USB-A male-to-male
  cable, and never combine two 5V supplies without an intentional VBUS
  design.

## Repository layout

```
pi/gadget/       ConfigFS gadget setup/teardown + manual test utilities
pi/bridge/       Rust daemons: input bridge (TCP 4870) and data relay (4871)
linux/           Rust input router + clipboard agent (one binary)
windows/         clipboard helper exe (cross-compiled with mingw)
protocol/        shared frame protocol crate
docs/            bring-up records, protocol spec, design decisions
```

The docs directory is the project's lab notebook: every phase records
what was observed, what was changed, and how to recover.

## Security posture

Beaglewing is built to be transparent, not sneaky. The gadget reports
honest identifiers and standard descriptors; the input path is ordinary
USB HID behavior; the optional clipboard helper is an unprivileged
userspace program; nothing bypasses endpoint policy or opens listeners
on the controlled machine. If your organization does not allow USB input
devices or unapproved executables, follow your organization's rules.

## License

Licensed under either of

- Apache License, Version 2.0 (LICENSE-APACHE)
- MIT license (LICENSE-MIT)

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you shall be dual
licensed as above, without any additional terms or conditions.
