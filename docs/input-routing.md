# Input Routing (Phase 4)

## Layout

The router crate (`linux/input-router`), all pieces live and verified:

- `portal.rs`: InputCapture portal client (v1 flow with automatic v2 +
  restore_token upgrade when the compositor ships it), plus the
  screensaver watcher.
- `capture.rs`: EI receiver thread (reis), seat capability binding.
- `router.rs`: the routing loop; activation, crossing, emergency
  hotkey, failsafes, bridge reconnect, pointer/wheel coalescing.
- `protocol.rs`: bridge-protocol v1 client.
- `hidmap.rs`: Linux keycode -> HID usage table (serves both raw evdev
  and the InputCapture portal, which also delivers evdev keycodes).
- `geometry.rs`: pure logical-space model: adjacent screens, edge
  crossing, normalized vertical mapping, canonical 0..65535 conversion,
  emergency force-local. Fully unit-tested.
- CLI: `run` (the router), `clipboard` (the clipboard agent), `list`,
  `observe`, `send-test`.

## Capture strategy on GNOME Wayland

Beaglebase runs Ubuntu 26.04, GNOME Shell 50.1, **Wayland**. The session
bus exposes `org.freedesktop.portal.InputCapture`. Two viable designs:

### Option A - InputCapture portal + libei (recommended)

The XDG portal built exactly for KVM-style software (InputLeap uses it):
the compositor places barriers at screen edges, notifies us when the
cursor hits one, and hands us the input event stream; we release capture
to give input back.

- Local input stays 100% native (feel, acceleration, gestures untouched).
- Edge detection is the compositor's own, with no cursor-position model drift.
- Capture/release is compositor-supervised: a wedged router cannot
  permanently trap input (GNOME can always yank capture), which
  structurally satisfies the no-lockout rule (docs/engineering-rules.md).
- One-time user consent dialog; the grant can persist.
- Costs: Wayland-session-only, needs libei bindings (`reis`) + portal
  D-Bus (`ashpd`/`zbus`), heavier dependencies, newer code paths.

### Option B - raw evdev EVIOCGRAB + uinput mirror (as originally spec'd)

Grab the physical devices exclusively; re-emit locally via a uinput
virtual device while local, send to the Pi while remote.

- Works in any session type, no portals, few dependencies.
- BUT on Wayland there is no sane way to read the real cursor position,
  so edge detection must come from our own delta accumulation, and a
  relative uinput mirror goes through compositor acceleration, so model
  and screen drift apart. Fixing that means an absolute-positioning local
  mirror with our own acceleration curve (changes local mouse feel), and
  carries the full lockout risk the spec's safety rails exist for.

### Recommendation

Option A. It is the upstream-blessed mechanism, avoids
the model-drift problem entirely, and converts the scariest failure mode
(input lockout) into a compositor-managed one. Option B stays in reserve
for non-Wayland environments.

**Decided 2026-08-31: Option A, InputCapture portal.**
Empirical probe results on this machine (`portal-probe.py`, GNOME 50.1):

- CreateSession triggers one consent dialog per session (persistence
  restore-tokens not yet implemented compositor-side in mutter; it is
  the same dialog Synergy 3 shows on Wayland). ~2.8s round-trip
  including the click.
- Granted capabilities: keyboard + pointer (3).
- GetZones reports the real desktop: one zone 2560x1440 at (0,0). The
  compositor is the authority on local geometry; no manual local config.

## Live status (2026-08-31, first working day)

The full routing loop is implemented on Option A and verified live:
edge crossing both directions, absolute motion, clicks, wheel, typing.

Findings from live tuning:

- GNOME 50.1's backend implements only portal v1 (`CreateSession2` is
  advertised by the frontend but returns UnknownMethod, so trust the
  `version` property). v1 = consent dialog inside CreateSession, one per
  router start, no persistence yet. The code auto-falls-back and will use
  v2 + restore_token the moment GNOME ships it.
- An EI receiver MUST bind seat capabilities on SeatAdded or the
  compositor streams nothing while still capturing, so input goes into a
  void. A 3s no-events failsafe now auto-releases if that ever recurs.
- mutter delivers wheel scroll as ScrollDiscrete in 120ths per detent
  (no ScrollDelta for wheels).
- USB HID gadget writes block until the host polls: positions and wheel
  detents are coalesced (newest-wins / summed) and flushed at 250 Hz,
  and the pointer endpoint polls at bInterval 1. Without this, fast
  motion or hi-res scrolling queues reports and the cursor "ice-skates".
- Windows' stock "hide pointer while typing" (aggressive in Notepad)
  looks like a cursor bug if you've never watched for it. It isn't ours.

Remaining for Phase 4 polish: configurable edge/remote geometry via
config file. Emergency hotkey
(LCtrl+LAlt+Backslash) live-tested OK.

Autostart (2026-09-07): all three user units (router, clipboard agent,
tray) are enabled at login. The router's consent dialog appears once
after login; the alternative, a dead edge until someone remembers to
start it, was worse. The router publishes its state (local, remote,
no-bridge) to $XDG_RUNTIME_DIR/beaglewing-router.state, which the tray
polls along with systemd unit state.

Screen lock (observed 2026-08-31 ~23:53) silently kills the
compositor's barriers; no Disabled or ZonesChanged signal arrives and
the router sits armed but never activates. Fixed 2026-09-01: the router
watches org.gnome.ScreenSaver ActiveChanged and re-arms barriers inside
the existing session on unlock (no consent dialog); if the session
itself died, it exits so systemd restarts it and the one dialog appears
while the user is at the machine.

## Safety rails (either option)

- No automatic start; verbose diagnostics; easy kill via SSH.
- Emergency return-to-local hotkey, configurable.
- RELEASE_ALL sent to the bridge on every transition and on shutdown
  (the Pi side additionally enforces this via heartbeat, proven in
  `pi/bridge/test-safety.py`).

## Network notes (2026-09-01)

A ~10s episode of choppy (current but sparse) pointer updates traced to
Wi-Fi link jitter: both machines are on 5GHz, and both radios had
power save enabled (now disabled and persisted via NetworkManager on
each end). Spikes improved but 3-23ms jitter remains; that is ambient
airtime contention, not tunable. The coalescing design degrades
gracefully through such episodes by intent: positions stay current,
nothing queues.

Real fix when desired: wire the Pi's eth0 to the LAN. Mandatory before
any video-capture phase, which would otherwise compete with input for
airtime.

Dip diagnosis (2026-09-01): the choppy-EI detector caught one ~20s
episode interleaved second-by-second with a heavy GPU compute job on
this machine; mutter composites on the same GPU, so a saturated GPU
janks its frame loop and captured-input delivery goes bursty. That episode was local by construction (gaps
measured at EI intake, before any network). Earlier dips predate the
detector and remain unattributed; target-host or Wi-Fi causes stay
plausible for those. Rare enough to accept; the detector stays armed so
any future dip self-attributes.

Hard power cut recovery (2026-09-15): a yanked cable power-cycles the
bus-powered Pi. Two follow-on faults seen afterwards, both diagnosable
from the logs: a replacement charge-grade cable produced 618 USB link
resets in a day (bridge log "transport endpoint shutdown", dmesg "new
address" every few seconds; fixed by a real data cable), and the Pi's
Wi-Fi came back in a sleepy state with ~25ms average latency and ~11ms
jitter to the gateway despite powersave off and full signal (felt as a
faint stutter; fixed by bouncing the radio: `nmcli radio wifi off; sleep
4; nmcli radio wifi on`, after which latency returned to ~6ms). Both
would be moot with the Pi on ethernet.

Wi-Fi self-heal (2026-09-15, pi/wifi-heal, optional): a NetworkManager
dispatcher hook plus a 15-minute timer run wifi-heal.sh, which pings a
target (set TARGET to the controlling desktop in
/etc/default/beaglewing-wifi-heal; gateways rate-limit ICMP and read as
jittery when the real path is fine), compares against a learned healthy
baseline, and bounces the radio with bounded retries (4 attempts over
~8 minutes, at most 3 bounces per hour, single-instance lock). Caveat
learned the hard way: a bounce fixes a stuck driver state but not an RF
problem; when the degradation is intermittent and physical (cable or
board placement near the antenna), bouncing just adds outages. Deploy it
once the link has a known-good baseline, not while it is misbehaving.

Stranded Pi (2026-09-15 to 09-17): while chasing the post-power-cut Wi-Fi
jitter, a live band switch (802-11-wireless.band=bg) was written into the
Pi's saved connection to test for a 5GHz interferer. The Pi never
re-associated and dropped off the LAN (no IPv4, no IPv6 link-local), and
since the setting was persistent it could not fall back on its own. It
took a power cycle two days later, after which it did associate on 2.4GHz
and a watcher reverted the band to auto. Root cause of the stranding was
process, not hardware; the rule is now in docs/engineering-rules.md. The
jitter itself remains attributed to placement or a nearby 5GHz source
and is moot with ethernet on eth0.

Latency instrumentation (2026-09-17): the router logs a per-minute
summary of key delivery lag (compositor timestamp to router receipt)
and the bridge logs a per-minute summary of keyboard report write time
(the USB IN endpoint wait). Measured while typing: 0.1ms avg/max handoff,
0.06ms avg / 2ms max USB write, plus the Wi-Fi hop (about 4ms avg, up to
12ms in a good minute and 40-50ms in a bad one). Note the compositor
timestamp is applied when mutter emits the event, so the first figure
proves the handoff is instant, not that mutter never queues internally.
The Wi-Fi hop is the only variable term; ethernet on the Pi would make
the whole path deterministic at a few ms.
