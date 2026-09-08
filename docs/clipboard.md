# Clipboard Sync (Phase 5)

## Transport decision: vendor raw HID data channel

The Windows clipboard can only be touched by code running in the Windows
session, so a small userspace helper on the laptop is required no matter
what. The design question is what pipe the helper uses. Decision: a third
HID interface on the existing composite gadget, vendor usage page 0xFF60
(the same mechanism QMK uses for raw HID). Reasons:

- Windows binds its generic HID driver automatically; no driver install.
- Userspace can open a vendor HID interface with HidD_* APIs without
  admin rights, so the helper is an ordinary unprivileged exe.
- No new device classes on a managed laptop: no network adapter, no
  storage device. Just one more interface on the input bridge that is
  already plugged in.
- Rejected alternatives: LAN/TCP (network activity on the target
  machine, against the security posture), USB NCM network gadget (fast and
  driver-free but a whole new NIC appearing in endpoint management).

Bandwidth, measured 2026-09-01 with 1024-byte reports (the high-speed
interrupt maximum; the Pi's dwc2 and Windows both accepted it): about
4 MB/s toward Windows (an 8MB batch in 2.2s) and about 0.8 MB/s toward
Linux, where each report costs the Windows helper a synchronous
WriteFile round-trip of ~1.3ms. Pipelining those writes (overlapped I/O
or parallel handles with a reordering receiver) is the known next step
for that direction. With 64-byte reports the same paths ran at 0.5 MB/s
and, before the helper's read/write handles were split, 49 KB/s.

## Data flow

```
Ubuntu clipboard (wl-clipboard / Clipboard portal)
      |
beaglebase clipboard agent (part of router or sibling process)
      |            TCP (separate port from input; never blocks input)
      v
Pi bridge data daemon <-> /dev/hidg2 <-> USB <-> Windows helper (HidD_*)
      |                                              |
      +---- chunked framed transfer protocol --------+
                                              Windows clipboard APIs
```

Standing rules (docs/engineering-rules.md): input and clipboard traffic
stay on separate connections; large transfers must never block HID
input; never log clipboard contents.

## Progression (in order, no skipping)

1. Text (UTF-8), both directions.
2. PNG/bitmap images (Windows DIB <-> PNG conversion at the edges).
3. Files: transfer contents, stage locally on the destination, publish a
   destination-local path (CF_HDROP on Windows, file:// URI list on
   Linux). A copied path is never pasted raw across machines.

## HID channel details

- Function hid.data, created third, so /dev/hidg2. Keyboard and pointer
  nodes keep their numbers.
- Report descriptor: vendor usage page 0xFF60, usage 0x61, one 1024-byte
  input report and one 1024-byte output report, no report IDs.
- bcdDevice bumped on each descriptor change (0x0103 now) so Windows
  re-reads descriptors.

## Framing on the 1024-byte reports

Implemented in `protocol/clipframe` (the shared crate both endpoints
build against; see its tests for the authoritative behavior):

```
byte 0    frame type
byte 1    flags (reserved, 0)
bytes 2-3 sequence (u16 LE)
bytes 4-5 payload length in this report (u16 LE, max 1018)
bytes 6..  payload
```

A transfer is START (content kind + total length), DATA frames with
strictly sequential numbering, then END echoing the total length as a
sanity check. Sequence gaps or length mismatches drop the transfer (the
channel is reliable, so a gap means a peer bug). Either side can ABORT.
Transfers above 64MB are refused up front (the channel moves about
0.5MB/s, so the cap is about patience as much as memory).

## Status

- [x] hid.data interface enumerates on Windows alongside keyboard/pointer
- [x] Ubuntu clipboard read/write proven (xclip via XWayland; wl-paste
      polling visibly churns the GNOME desktop and was dropped)
- [x] Windows helper opens the vendor HID device (no driver, no admin)
- [x] Text sync both directions (verified live 2026-08-31; ~instant for
      ordinary text)
- [x] Images both directions (verified 2026-09-01: Ubuntu screenshot ->
      Teams paste, Windows snip -> Ubuntu). Wire format is PNG; the
      Windows edge publishes both the registered PNG format (Chromium
      apps) and CF_DIB (classic apps), and prefers reading PNG verbatim.
- [x] Files and directories both directions (verified 2026-09-01:
      nested structures intact). Tar on the wire, staged locally,
      destination-local CF_HDROP / gnome-copied-files published; 64MB
      cap; 3-day staging cleanup. Scale-checked with a 20MB two-file batch
      Windows -> Linux: byte-perfect, about 40s, progress logged both ends.
- [x] Flow control: Windows HIDClass holds ~512 input reports and drops
      the oldest on overflow, which ate the START frame of the first
      multi-thousand-frame burst. Receivers ACK every 128 DATA frames,
      senders cap 256 in flight and abort after 5s of ACK silence.

Known quirks: xclip serves one target, so received batches paste in
Nautilus but not into browser upload dialogs (multi-target owner is a
possible refinement); if an xclip owner dies, mutter's clipboard manager
degrades the retained content to plain text (mostly a test-harness
concern, real file managers stay alive as owners).

- [x] Lazy files: a copy sends an OFFER; the batch moves when the user
      crosses to the other machine (router -> agent datagram socket) or
      runs `beaglewing-router pull`. Local-only copies never transfer.
      Offers and arrivals raise desktop notifications on Linux.
- [x] Relay robustness: nonblocking HID I/O with poll(); a host that
      stops draining gets 10s of grace, then frames drop instead of the
      pump wedging. Progress and ETA logged on both ends.

Deployment: beaglewing-data (Pi, boot-enabled), beaglewing-clipboard
(Ubuntu user service, login-enabled), beaglewing-clip.exe run manually
on Windows for now.
