#!/usr/bin/env python3
"""Send exactly one harmless key-down/key-up pair through the HID gadget.

Manual use only. Never run automatically at boot or on connect.
Always releases: the release report is written in a finally block, twice,
so a failure between down and up cannot leave a key stuck on the host.
"""

import argparse
import string
import sys
import time

# HID usage IDs for a-z (0x04..0x1d); 'a' is the harmless default.
LETTER_USAGE = {c: 0x04 + i for i, c in enumerate(string.ascii_lowercase)}

RELEASE = bytes(8)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--device", default="/dev/hidg0", help="HID gadget node")
    ap.add_argument("--key", default="a", choices=sorted(LETTER_USAGE),
                    help="single unmodified letter to send (default: a)")
    args = ap.parse_args()

    usage = LETTER_USAGE[args.key]
    down = bytes([0, 0, usage, 0, 0, 0, 0, 0])

    try:
        f = open(args.device, "wb", buffering=0)
    except OSError as e:
        print(f"ERROR: cannot open {args.device}: {e}\n"
              "Is the gadget set up (setup-gadget.sh) and the host connected?",
              file=sys.stderr)
        return 1

    try:
        with f:
            f.write(down)
            time.sleep(0.05)
    except OSError as e:
        print(f"ERROR: write failed: {e}\n"
              "The host may not have configured the device yet "
              "(check cable and Windows-side enumeration).", file=sys.stderr)
        return 1
    finally:
        # Belt and braces: always try to release, even after a failed write.
        try:
            with open(args.device, "wb", buffering=0) as rf:
                rf.write(RELEASE)
                rf.write(RELEASE)
        except OSError:
            pass

    print(f"Sent key '{args.key}': down, up. All keys released.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
