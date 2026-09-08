#!/usr/bin/env python3
"""Move the absolute pointer through a short, visible test sequence.

Manual use only. Never run automatically at boot or on connect.
Coordinates are given in the project's canonical 0..65535 range and scaled
deterministically (>> 1) to the descriptor's 0..32767 logical range.

Default behavior is movement only (harmless). A single click or wheel step
can be requested explicitly. A release-all report is always written in a
finally block, so no button can be left stuck on the host.

Report layout (6 bytes): buttons | X lo | X hi | Y lo | Y hi | wheel
"""

import argparse
import struct
import sys
import time

BUTTON_BITS = {"left": 0x01, "right": 0x02, "middle": 0x04}
CENTER = (32768, 32768)
# Canonical-range waypoints: center, then an inset diamond, back to center.
SEQUENCE = [CENTER, (16384, 32768), (32768, 16384), (49152, 32768),
            (32768, 49152), CENTER]


def report(x: int, y: int, buttons: int = 0, wheel: int = 0) -> bytes:
    x = max(0, min(65535, x)) >> 1
    y = max(0, min(65535, y)) >> 1
    return struct.pack("<BHHb", buttons, x, y, wheel)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--device", default="/dev/hidg1", help="pointer HID node")
    ap.add_argument("--click", choices=sorted(BUTTON_BITS),
                    help="send one click of this button at screen center")
    ap.add_argument("--wheel", type=int, default=0, metavar="N",
                    help="send one wheel report of N detents (-127..127)")
    ap.add_argument("--delay", type=float, default=0.4,
                    help="seconds between waypoints (default 0.4)")
    args = ap.parse_args()

    try:
        f = open(args.device, "wb", buffering=0)
    except OSError as e:
        print(f"ERROR: cannot open {args.device}: {e}\n"
              "Is the gadget set up (setup-gadget.sh) and the host connected?",
              file=sys.stderr)
        return 1

    last = CENTER
    try:
        with f:
            for x, y in SEQUENCE:
                f.write(report(x, y))
                last = (x, y)
                time.sleep(args.delay)
            if args.click:
                bit = BUTTON_BITS[args.click]
                f.write(report(*last, buttons=bit))
                time.sleep(0.05)
                f.write(report(*last))
            if args.wheel:
                f.write(report(*last, wheel=max(-127, min(127, args.wheel))))
    except OSError as e:
        print(f"ERROR: write failed: {e}\n"
              "The host may not have configured the device yet.",
              file=sys.stderr)
        return 1
    finally:
        # Always release all buttons, twice, even after a failed write.
        try:
            with open(args.device, "wb", buffering=0) as rf:
                rf.write(report(*last))
                rf.write(report(*last))
        except OSError:
            pass

    extras = (f", one {args.click} click" if args.click else "") + \
             (f", wheel {args.wheel}" if args.wheel else "")
    print(f"Pointer sequence complete ({len(SEQUENCE)} waypoints{extras}). "
          "All buttons released.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
