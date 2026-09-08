# USB Gadget Bring-up (Phases 0-2)

## Discovered state (2026-08-31)

- Raspberry Pi 4 Model B Rev 1.5, Debian 13 (trixie), kernel `6.18.39+rpt-rpi-v8`.
- `/sys/class/udc` was empty: no USB device controller exposed.
- `/boot/firmware/config.txt` contained `otg_mode=1` under `[cm4]` and
  `dtoverlay=dwc2,dr_mode=host` under `[cm5]`; both are **inert on a Pi 4B**
  (wrong board filters). Effectively the Pi had no dwc2 configuration at all,
  so the USB-C controller (`fe980000.usb`) was never enabled as a device.
- The xHCI controller in dmesg is the VL805 (PCIe) serving the USB-A host
  ports; it is unrelated to the USB-C port and untouched by this change.
- dwc2 is built into this kernel (not a module), so `lsmod` shows nothing;
  the devicetree overlay alone activates it.

## Boot configuration change

One line appended to the `[all]` section of `/boot/firmware/config.txt`:

```
# beaglewing input bridge: expose USB-C as USB device controller (dwc2 peripheral)
dtoverlay=dwc2,dr_mode=peripheral
```

- Backup: `/boot/firmware/config.txt.bak.20260831-133725`
- `cmdline.txt` was **not** modified (no `modules-load=` needed; dwc2 binds
  via devicetree).
- `dr_mode=peripheral`, not `otg`: this port's role is permanently
  device-side in our architecture.

After reboot, `ls /sys/class/udc` shows `fe980000.usb`.

Recovery: restore the backup over `config.txt` and reboot. SSH over Wi-Fi is
unaffected by any of this.

## Gadget layout (Phase 1: keyboard only)

Built by `pi/gadget/setup-gadget.sh` under
`/sys/kernel/config/usb_gadget/beaglewing`:

- VID/PID `0x1d6b:0x0104` (Linux Foundation / multifunction composite),
  `bcdUSB 2.0`, serial from `/proc/cpuinfo`.
- Strings: "Beaglewing Project" / "Beaglewing HID Keyboard".
- One config `c.1`, `MaxPower 500 mA` (honest: the Pi is bus-powered from
  the Windows host over the same cable at this stage).
- One function `hid.kbd`: protocol 1 (keyboard), subclass 1 (boot),
  8-byte reports, standard boot-keyboard report descriptor
  (byte 0 modifiers, byte 1 reserved, bytes 2-7 keycodes).
- The UDC is detected dynamically; the script fails loudly if none (or more
  than one) exists, and binds only after the whole gadget tree is complete.

## Phase 2 additions: absolute pointer

A second HID function `hid.mouse` in the same gadget/config:

- No boot protocol (protocol 0, subclass 0), since absolute pointers require
  report mode.
- 6-byte report: `buttons(1) | X lo,hi | Y lo,hi | wheel(1)`; 3 buttons,
  signed wheel.
- X/Y are **absolute**, logical 0..32767, the widely proven range for
  Windows absolute pointer devices. The project's canonical 0..65535 range
  lives at the protocol layer; emitters scale down deterministically
  (`>> 1`). Sub-pixel precision on any real monitor either way.
- Function creation order fixes device nodes: keyboard `/dev/hidg0`,
  pointer `/dev/hidg1` (setup prints the mapping; nothing hardcodes it
  beyond creation order).
- `bcdDevice` bumped 0x0100 -> 0x0101 and product string generalized to
  "Beaglewing Input Bridge": Windows caches descriptors per VID/PID, and
  the revision bump forces a clean re-read after the interface was added.

## Operation

On the Pi (scripts deployed to `/opt/beaglewing/gadget/`):

```bash
/opt/beaglewing/gadget/setup-gadget.sh      # build + bind; idempotent
/opt/beaglewing/gadget/teardown-gadget.sh   # unbind + remove; safe on partial state
python3 /opt/beaglewing/gadget/test-keyboard.py [--key a]
python3 /opt/beaglewing/gadget/test-pointer.py [--click left] [--wheel N]
```

- Setup refuses to run over a half-built gadget; tear down first.
- The test utility sends exactly one unmodified letter (default `a`) as a
  down/up pair and always writes a release-all report (twice, in a finally
  block), so it cannot leave a key stuck.
- Since manual testing proved stable, the gadget and both daemons are
  systemd services enabled at boot (beaglewing-gadget, beaglewing-bridge,
  beaglewing-data); the scripts above remain the manual path and what the
  units call.

## Verified

- [x] UDC `fe980000.usb` appears after reboot
- [x] Gadget builds and binds; `/dev/hidg0` appears
- [x] Windows enumerates "Beaglewing HID Keyboard" over USB-C
      (high speed, UDC state `configured`)
- [x] Manual key test: one key down/up, no stuck keys (verified in
      Notepad, 2026-08-31)
- [x] Pointer function enumerates alongside keyboard (address 22,
      `configured`, high speed)
- [x] Pointer waypoint sequence moves the Windows cursor deterministically
      (center + diamond verified on screen, 2026-08-31)
- [x] Click and wheel work (click collapsed a text selection, wheel
      scrolled); no stuck buttons; keyboard still works in the composite
      gadget
