# Beaglewing Bridge Protocol v1

TCP, default port **4870**. Ubuntu (`beaglebase`) is the client; the Pi
bridge daemon is the server. Latency-sensitive input only; clipboard and
file traffic ride their own channel (docs/clipboard.md), so a transfer
can never block a keystroke.

## Framing

Every message is:

```
[type: u8] [len: u8] [payload: len bytes]
```

`len` is validated strictly per type. An unknown type, a wrong length, or
`len > 32` is a protocol error: the server **releases all input state and
closes the connection**. Multi-byte integers are little-endian.

## Session rules

- The first message MUST be `HELLO` with a matching version, or the server
  closes the connection.
- One client at a time, handled serially. A half-dead connection is
  reclaimed by the heartbeat timeout (worst-case ~5 s before a
  reconnect is accepted).
- **Heartbeat:** if the server receives nothing for 5 s, it releases all
  input state and drops the client. Clients should send `KEEPALIVE` at
  least every 1-2 s when idle. Any message resets the deadline.
- On ANY disconnect path (clean, error, timeout, malformed, daemon
  shutdown) the server writes release-all reports to both HID devices:
  no stuck keys, modifiers, or buttons.

## Messages

| Type | Name | Dir | Len | Payload |
|------|------|-----|-----|---------|
| 0x01 | HELLO | C->S | 2 | `u16` protocol version (= 1) |
| 0x02 | HELLO_ACK | S->C | 2 | `u16` accepted version |
| 0x03 | KEEPALIVE | C->S | 0 | none (no reply) |
| 0x10 | KEY_DOWN | C->S | 1 | HID usage id |
| 0x11 | KEY_UP | C->S | 1 | HID usage id |
| 0x12 | KEY_STATE | C->S | 7 | modifier byte + 6 usage slots (authoritative) |
| 0x20 | POINTER_ABS | C->S | 4 | `u16` x, `u16` y, canonical 0..65535 |
| 0x21 | POINTER_BUTTON | C->S | 1 | absolute bitmask: bit0 L, bit1 R, bit2 M |
| 0x22 | POINTER_WHEEL | C->S | 1 | `i8` detents |
| 0x30 | RELEASE_ALL | C->S | 0 | clear all keyboard + button state |
| 0x40 | GET_STATUS | C->S | 0 | none |
| 0x41 | STATUS | S->C | 4 | modifiers, held-key count, buttons, hid_ok |

## Semantics

- `KEY_DOWN`/`KEY_UP` with usages 0xE0-0xE7 toggle modifier bits; other
  usages occupy one of the 6 boot-report slots. A 7th simultaneous
  non-modifier key is dropped and logged (boot keyboard limit).
- `POINTER_BUTTON` carries the **full** button state each time (absolute,
  like everything else in this protocol; no toggles to get out of sync).
- The pointer HID report always carries X/Y; buttons/wheel messages re-send
  the last known position (initialized to screen center 32768,32768 until
  the first `POINTER_ABS`).
- Canonical 0..65535 coordinates are scaled `>> 1` to the descriptor's
  0..32767 at the HID layer.
- `hid_ok` in `STATUS` is 0 if the last HID write failed (e.g. USB host
  disconnected); the daemon keeps running and recovers when writes succeed
  again.
