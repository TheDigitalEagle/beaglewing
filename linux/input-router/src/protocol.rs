//! Bridge protocol v1 client (docs/protocol.md).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const VERSION: u16 = 1;

pub const MSG_HELLO: u8 = 0x01;
pub const MSG_HELLO_ACK: u8 = 0x02;
pub const MSG_KEEPALIVE: u8 = 0x03;
pub const MSG_KEY_DOWN: u8 = 0x10;
pub const MSG_KEY_UP: u8 = 0x11;
pub const MSG_POINTER_ABS: u8 = 0x20;
pub const MSG_POINTER_BUTTON: u8 = 0x21;
pub const MSG_POINTER_WHEEL: u8 = 0x22;
pub const MSG_RELEASE_ALL: u8 = 0x30;
pub const MSG_GET_STATUS: u8 = 0x40;
pub const MSG_STATUS: u8 = 0x41;

pub fn frame(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= 32);
    let mut f = Vec::with_capacity(2 + payload.len());
    f.push(msg_type);
    f.push(payload.len() as u8);
    f.extend_from_slice(payload);
    f
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub modifiers: u8,
    pub held_keys: u8,
    pub buttons: u8,
    pub hid_ok: bool,
}

pub struct BridgeClient {
    stream: TcpStream,
}

impl BridgeClient {
    /// Connect and complete the HELLO handshake.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut c = BridgeClient { stream };
        c.send(MSG_HELLO, &VERSION.to_le_bytes())?;
        let (t, p) = c.recv()?;
        if t != MSG_HELLO_ACK || p != VERSION.to_le_bytes() {
            return Err(std::io::Error::other("bridge HELLO_ACK mismatch"));
        }
        Ok(c)
    }

    fn send(&mut self, msg_type: u8, payload: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(&frame(msg_type, payload))
    }

    fn recv(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut hdr = [0u8; 2];
        self.stream.read_exact(&mut hdr)?;
        let mut payload = vec![0u8; hdr[1] as usize];
        self.stream.read_exact(&mut payload)?;
        Ok((hdr[0], payload))
    }

    pub fn keepalive(&mut self) -> std::io::Result<()> {
        self.send(MSG_KEEPALIVE, &[])
    }

    pub fn key_down(&mut self, usage: u8) -> std::io::Result<()> {
        self.send(MSG_KEY_DOWN, &[usage])
    }

    pub fn key_up(&mut self, usage: u8) -> std::io::Result<()> {
        self.send(MSG_KEY_UP, &[usage])
    }

    /// Canonical coordinates, 0..65535.
    pub fn pointer_abs(&mut self, x: u16, y: u16) -> std::io::Result<()> {
        let mut p = [0u8; 4];
        p[..2].copy_from_slice(&x.to_le_bytes());
        p[2..].copy_from_slice(&y.to_le_bytes());
        self.send(MSG_POINTER_ABS, &p)
    }

    /// Full absolute button state: bit0 left, bit1 right, bit2 middle.
    pub fn pointer_button(&mut self, buttons: u8) -> std::io::Result<()> {
        self.send(MSG_POINTER_BUTTON, &[buttons])
    }

    pub fn pointer_wheel(&mut self, detents: i8) -> std::io::Result<()> {
        self.send(MSG_POINTER_WHEEL, &[detents as u8])
    }

    pub fn release_all(&mut self) -> std::io::Result<()> {
        self.send(MSG_RELEASE_ALL, &[])
    }

    pub fn status(&mut self) -> std::io::Result<Status> {
        self.send(MSG_GET_STATUS, &[])?;
        let (t, p) = self.recv()?;
        if t != MSG_STATUS || p.len() != 4 {
            return Err(std::io::Error::other("bad STATUS reply"));
        }
        Ok(Status {
            modifiers: p[0],
            held_keys: p[1],
            buttons: p[2],
            hid_ok: p[3] != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout() {
        assert_eq!(frame(MSG_KEEPALIVE, &[]), vec![0x03, 0x00]);
        assert_eq!(frame(MSG_KEY_DOWN, &[0x04]), vec![0x10, 0x01, 0x04]);
        assert_eq!(
            frame(MSG_POINTER_ABS, &[0xff, 0xff, 0x00, 0x80]),
            vec![0x20, 0x04, 0xff, 0xff, 0x00, 0x80]
        );
    }
}
