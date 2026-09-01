# Engineering Rules

The standing rules this project is built under. Every phase has followed
them; future work should too.

## Hardware

- The Pi 4's USB-C connector is the only device-capable port and it
  belongs to the target machine. The USB-A ports are host-only.
- Never use a USB-A male-to-male cable.
- Never combine two 5V supplies without an intentional VBUS design. The
  Pi is currently bus-powered by the target over the gadget cable; any
  change to that must be deliberate.
- Never hardcode the USB device controller name. Detect it, and fail
  loudly if it is missing.
- Back up boot configuration before editing it, and keep a recovery path
  (SSH over the LAN is independent of everything USB).

## Safety

- Input must fail safe at every layer. Any disconnect, timeout,
  malformed message, crash, or shutdown releases all keys, modifiers,
  and buttons on the target. No stuck input, ever.
- The developer must never be locked out of the controlling desktop.
  Capture is compositor-supervised; there is always an emergency return
  hotkey and an SSH path to kill everything.
- Gadget setup is idempotent, teardown is safe on partial state, and
  nothing gets enabled at boot until it has proven stable manually.
- Large transfers must never block the input path; input and data ride
  separate connections end to end.

## Discipline

- Inspect the actual system before changing it; when documentation and
  observed hardware disagree, gather evidence before continuing.
- Keep changes small and testable, and test each rung of the ladder
  before climbing to the next (keyboard before pointer, manual toggle
  before edge switching, text before images before files).
- The docs directory is the lab notebook: record what was observed, what
  was changed, and how to recover it.

## Security posture

- The gadget reports honest identifiers and standard descriptors. It is
  transparent hardware, not a disguise.
- Nothing bypasses endpoint policy on the controlled machine: no
  drivers, no admin installs, no network listeners, no evasion.
- Clipboard contents are never logged on any side, and data features
  stay independently disableable from the input path.
