#!/usr/bin/env python3
"""Verify the bridge's safety properties (docs/protocol.md).

A: a malformed frame gets the connection closed.
B: a silent client holding a key is dropped by the heartbeat timeout and
   the held key is released (checked via STATUS on a fresh connection).
C: reconnecting immediately after works.

Uses only a modifier key (left shift) so a focused window sees no output.
"""

import socket
import struct
import sys
import time

HOST, PORT, VERSION = "beaglewing", 4870, 1
HELLO, HELLO_ACK = 0x01, 0x02
KEY_DOWN, GET_STATUS, STATUS = 0x10, 0x40, 0x41
LEFT_SHIFT = 0xE1


def frame(t, payload=b""):
    return bytes([t, len(payload)]) + payload


def connect():
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.sendall(frame(HELLO, struct.pack("<H", VERSION)))
    t, ln = s.recv(2)
    s.recv(ln)
    assert t == HELLO_ACK, f"no HELLO_ACK (got {t:#04x})"
    return s


def expect_closed(s, within, what):
    s.settimeout(within)
    try:
        data = s.recv(2)
    except socket.timeout:
        print(f"FAIL [{what}]: connection still open after {within}s")
        return False
    if data:
        print(f"FAIL [{what}]: expected close, got data {data.hex()}")
        return False
    print(f"ok   [{what}]: server closed the connection")
    return True


def status():
    with connect() as s:
        s.sendall(frame(GET_STATUS))
        t, ln = s.recv(2)
        p = s.recv(ln)
        assert t == STATUS and len(p) == 4, "bad STATUS reply"
        return tuple(p)  # modifiers, held_keys, buttons, hid_ok


def main() -> int:
    ok = True

    # A: malformed frame
    with connect() as s:
        s.sendall(bytes([0xEE, 0x00]))
        ok &= expect_closed(s, 3, "malformed frame")

    # B: heartbeat timeout with a held modifier
    with connect() as s:
        s.sendall(frame(KEY_DOWN, bytes([LEFT_SHIFT])))
        time.sleep(0.2)
        print("holding left shift, going silent (waiting out 5s heartbeat)...")
        ok &= expect_closed(s, 8, "heartbeat timeout")

    # C: immediate reconnect + state must be clean
    mods, held, buttons, hid_ok = status()
    if mods == held == buttons == 0 and hid_ok:
        print("ok   [reconnect+release]: fresh connection, state clean, HID ok")
    else:
        print(f"FAIL [reconnect+release]: mods={mods:#04x} held={held} "
              f"buttons={buttons:#04x} hid_ok={hid_ok}")
        ok = False

    print("ALL SAFETY TESTS PASSED" if ok else "SAFETY TESTS FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
