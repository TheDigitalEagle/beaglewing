#!/usr/bin/env python3
"""End-to-end bridge test: drive the Pi daemon over TCP from Ubuntu.

Manual use only. Sequence: HELLO handshake, pointer diamond, optional
click, types a short marker string, RELEASE_ALL, then prints STATUS.
Protocol: docs/protocol.md (v1).
"""

import argparse
import socket
import string
import struct
import sys
import time

VERSION = 1
HELLO, HELLO_ACK, KEEPALIVE = 0x01, 0x02, 0x03
KEY_DOWN, KEY_UP = 0x10, 0x11
POINTER_ABS, POINTER_BUTTON, POINTER_WHEEL = 0x20, 0x21, 0x22
RELEASE_ALL, GET_STATUS, STATUS = 0x30, 0x40, 0x41

# HID usages: letters a-z, digits, and the few extras the marker needs.
USAGE = {c: 0x04 + i for i, c in enumerate(string.ascii_lowercase)}
USAGE.update({str(d): 0x1E + i for i, d in enumerate([1, 2, 3, 4, 5, 6, 7, 8, 9, 0])})
USAGE[" "] = 0x2C
USAGE["\n"] = 0x28
USAGE["-"] = 0x2D

DIAMOND = [(32768, 32768), (16384, 32768), (32768, 16384),
           (49152, 32768), (32768, 49152), (32768, 32768)]


def frame(t: int, payload: bytes = b"") -> bytes:
    return bytes([t, len(payload)]) + payload


def recv_frame(sock: socket.socket):
    hdr = sock.recv(2)
    if len(hdr) < 2:
        raise ConnectionError("connection closed by bridge")
    t, ln = hdr
    payload = sock.recv(ln) if ln else b""
    return t, payload


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="beaglewing")
    ap.add_argument("--port", type=int, default=4870)
    ap.add_argument("--text", default="bridge ok\n",
                    help="marker text to type (default 'bridge ok\\n')")
    ap.add_argument("--click", action="store_true",
                    help="left-click at screen center before typing")
    ap.add_argument("--delay", type=float, default=0.25)
    args = ap.parse_args()

    unsupported = [c for c in args.text if c not in USAGE]
    if unsupported:
        print(f"ERROR: no usage mapping for {unsupported!r}", file=sys.stderr)
        return 2

    with socket.create_connection((args.host, args.port), timeout=5) as s:
        s.sendall(frame(HELLO, struct.pack("<H", VERSION)))
        t, p = recv_frame(s)
        if t != HELLO_ACK:
            print(f"ERROR: expected HELLO_ACK, got {t:#04x}", file=sys.stderr)
            return 1
        print(f"handshake OK (bridge protocol v{struct.unpack('<H', p)[0]})")

        try:
            for x, y in DIAMOND:
                s.sendall(frame(POINTER_ABS, struct.pack("<HH", x, y)))
                time.sleep(args.delay)
            print("pointer diamond sent")

            if args.click:
                s.sendall(frame(POINTER_BUTTON, b"\x01"))
                time.sleep(0.05)
                s.sendall(frame(POINTER_BUTTON, b"\x00"))
                print("left click sent")

            for c in args.text:
                s.sendall(frame(KEY_DOWN, bytes([USAGE[c]])))
                time.sleep(0.02)
                s.sendall(frame(KEY_UP, bytes([USAGE[c]])))
                time.sleep(0.02)
            print(f"typed {args.text!r}")
        finally:
            s.sendall(frame(RELEASE_ALL))

        s.sendall(frame(GET_STATUS))
        t, p = recv_frame(s)
        if t == STATUS and len(p) == 4:
            mods, held, buttons, hid_ok = p
            print(f"STATUS: modifiers={mods:#04x} held_keys={held} "
                  f"buttons={buttons:#04x} hid_ok={bool(hid_ok)}")
            if mods or held or buttons:
                print("ERROR: state not clean after RELEASE_ALL", file=sys.stderr)
                return 1
            if not hid_ok:
                print("ERROR: bridge reports HID writes failing", file=sys.stderr)
                return 1
        else:
            print(f"ERROR: bad STATUS reply {t:#04x}", file=sys.stderr)
            return 1

    print("end-to-end test complete")
    return 0


if __name__ == "__main__":
    sys.exit(main())
