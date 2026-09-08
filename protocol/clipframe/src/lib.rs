//! Framing for the vendor raw HID clipboard channel (docs/clipboard.md).
//!
//! Every frame is exactly FRAME_LEN (1024) bytes:
//!
//! ```text
//! byte 0     frame type
//! byte 1     flags (reserved, 0)
//! bytes 2-3  sequence number (u16 LE, wraps)
//! bytes 4-5  payload length (u16 LE, max FRAME_LEN-6)
//! bytes 6..  payload
//! ```
//!
//! A transfer is START (kind + total length), DATA frames, then END
//! (echoes total length as a sanity check). One transfer at a time per
//! direction; either side can ABORT.

pub mod files;

pub const FRAME_LEN: usize = 1024;
pub const MAX_PAYLOAD: usize = FRAME_LEN - 6;

pub const T_PING: u8 = 0x01;
pub const T_PONG: u8 = 0x02;
pub const T_START: u8 = 0x10;
pub const T_DATA: u8 = 0x11;
pub const T_END: u8 = 0x12;
pub const T_ABORT: u8 = 0x13;
/// Flow control: receiver reports how many DATA frames of the current
/// transfer it has consumed (u32 LE). Windows drops the OLDEST queued
/// HID input reports when its ~512-report buffer overflows, so senders
/// keep at most ACK_WINDOW frames in flight.
pub const T_ACK: u8 = 0x14;

/// Lazy transfers: a file copy sends only an OFFER (what is available);
/// the bytes move when the other side PULLs, typically because the user
/// crossed over to it. Copies that never cross machines never transfer.
pub const T_OFFER: u8 = 0x20;
pub const T_PULL: u8 = 0x21;

pub const ACK_EVERY: u32 = 128;
pub const ACK_WINDOW: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    pub kind: u8,
    pub total: u32,
    pub items: u16,
    pub id: u32,
}

pub fn offer_frame(o: &Offer) -> [u8; FRAME_LEN] {
    let mut p = [0u8; 11];
    p[0] = o.kind;
    p[1..5].copy_from_slice(&o.total.to_le_bytes());
    p[5..7].copy_from_slice(&o.items.to_le_bytes());
    p[7..11].copy_from_slice(&o.id.to_le_bytes());
    frame(T_OFFER, 0, &p)
}

pub fn pull_frame(id: u32) -> [u8; FRAME_LEN] {
    frame(T_PULL, 0, &id.to_le_bytes())
}

pub fn ack_frame(count: u32) -> [u8; FRAME_LEN] {
    frame(T_ACK, 0, &count.to_le_bytes())
}

pub fn parse_ack(f: &[u8; FRAME_LEN]) -> Option<u32> {
    let p = parse(f)?;
    (p.t == T_ACK && p.payload.len() == 4).then(|| {
        u32::from_le_bytes([p.payload[0], p.payload[1], p.payload[2], p.payload[3]])
    })
}

pub const KIND_TEXT: u8 = 1;
pub const KIND_PNG: u8 = 2;
/// A tar stream carrying files and/or directories with relative paths.
pub const KIND_TAR: u8 = 3;

/// Refuse absurd transfers before allocating for them.
pub const MAX_TRANSFER: u32 = 64 * 1024 * 1024;

/// Measured channel rates (bytes/s) with 1024-byte reports, 2026-09-01.
/// Toward Windows the Pi relay streams at near endpoint rate; toward
/// Linux each report costs the Windows helper a synchronous WriteFile
/// round-trip (~1.3ms), which is the ceiling until writes are pipelined.
pub const RATE_TO_WINDOWS: u64 = 4_000_000;
pub const RATE_TO_LINUX: u64 = 800_000;

/// Rough transfer time for logs.
pub fn eta_secs(bytes: usize, rate: u64) -> u64 {
    (bytes as u64 / rate).max(1)
}

pub fn frame(t: u8, seq: u16, payload: &[u8]) -> [u8; FRAME_LEN] {
    assert!(payload.len() <= MAX_PAYLOAD);
    let mut f = [0u8; FRAME_LEN];
    f[0] = t;
    f[2..4].copy_from_slice(&seq.to_le_bytes());
    f[4..6].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    f[6..6 + payload.len()].copy_from_slice(payload);
    f
}

pub struct Parsed<'a> {
    pub t: u8,
    pub seq: u16,
    pub payload: &'a [u8],
}

pub fn parse(f: &[u8; FRAME_LEN]) -> Option<Parsed<'_>> {
    let len = u16::from_le_bytes([f[4], f[5]]) as usize;
    if len > MAX_PAYLOAD {
        return None;
    }
    Some(Parsed {
        t: f[0],
        seq: u16::from_le_bytes([f[2], f[3]]),
        payload: &f[6..6 + len],
    })
}

/// Encode one complete transfer as a frame sequence.
pub fn encode_transfer(kind: u8, data: &[u8]) -> Vec<[u8; FRAME_LEN]> {
    let mut out = Vec::with_capacity(2 + data.len() / MAX_PAYLOAD);
    let mut seq: u16 = 0;
    let mut start = [0u8; 5];
    start[0] = kind;
    start[1..5].copy_from_slice(&(data.len() as u32).to_le_bytes());
    out.push(frame(T_START, seq, &start));
    for chunk in data.chunks(MAX_PAYLOAD) {
        seq = seq.wrapping_add(1);
        out.push(frame(T_DATA, seq, chunk));
    }
    seq = seq.wrapping_add(1);
    out.push(frame(T_END, seq, &(data.len() as u32).to_le_bytes()));
    out
}

/// Receiving state machine. Feed frames; a completed transfer comes back
/// as (kind, data). Sequence gaps or length mismatches drop the transfer
/// (the channel is reliable, so a gap means a peer bug, not line noise).
#[derive(Default)]
pub struct Reassembler {
    active: Option<(u8, u32, u16, Vec<u8>)>, // kind, total, last_seq, data
    data_frames: u32,
}

impl Reassembler {
    /// (bytes received, total bytes) of the transfer in progress, if any.
    pub fn status(&self) -> Option<(usize, u32)> {
        self.active
            .as_ref()
            .map(|(_, total, _, data)| (data.len(), *total))
    }

    pub fn feed(&mut self, raw: &[u8; FRAME_LEN]) -> FeedResult {
        let Some(p) = parse(raw) else {
            return FeedResult::Error("unparseable frame");
        };
        match p.t {
            T_PING => FeedResult::Ping,
            T_PONG => FeedResult::None,
            T_ABORT => {
                self.active = None;
                FeedResult::None
            }
            T_ACK => FeedResult::None, // flow control, handled by senders
            T_OFFER => {
                if p.payload.len() != 11 {
                    return FeedResult::Error("bad OFFER payload");
                }
                let b = p.payload;
                FeedResult::Offer(Offer {
                    kind: b[0],
                    total: u32::from_le_bytes([b[1], b[2], b[3], b[4]]),
                    items: u16::from_le_bytes([b[5], b[6]]),
                    id: u32::from_le_bytes([b[7], b[8], b[9], b[10]]),
                })
            }
            T_PULL => {
                if p.payload.len() != 4 {
                    return FeedResult::Error("bad PULL payload");
                }
                let b = p.payload;
                FeedResult::Pull(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            }
            T_START => {
                if p.payload.len() != 5 {
                    return FeedResult::Error("bad START payload");
                }
                let kind = p.payload[0];
                let total =
                    u32::from_le_bytes([p.payload[1], p.payload[2], p.payload[3], p.payload[4]]);
                if total > MAX_TRANSFER {
                    self.active = None;
                    return FeedResult::Error("transfer too large");
                }
                self.active = Some((kind, total, p.seq, Vec::with_capacity(total as usize)));
                self.data_frames = 0;
                FeedResult::None
            }
            T_DATA => {
                let Some((_, total, last_seq, data)) = self.active.as_mut() else {
                    return FeedResult::Error("DATA without START");
                };
                if p.seq != last_seq.wrapping_add(1) {
                    self.active = None;
                    return FeedResult::Error("sequence gap");
                }
                *last_seq = p.seq;
                data.extend_from_slice(p.payload);
                if data.len() as u32 > *total {
                    self.active = None;
                    return FeedResult::Error("overrun");
                }
                self.data_frames += 1;
                FeedResult::Progress(self.data_frames)
            }
            T_END => {
                let Some((kind, total, last_seq, data)) = self.active.take() else {
                    return FeedResult::Error("END without START");
                };
                if p.seq != last_seq.wrapping_add(1) {
                    return FeedResult::Error("sequence gap at END");
                }
                if p.payload.len() != 4
                    || u32::from_le_bytes([p.payload[0], p.payload[1], p.payload[2], p.payload[3]])
                        != total
                    || data.len() as u32 != total
                {
                    return FeedResult::Error("length mismatch at END");
                }
                FeedResult::Complete(kind, data)
            }
            _ => FeedResult::Error("unknown frame type"),
        }
    }
}

pub enum FeedResult {
    None,
    Ping,
    /// A DATA frame landed; the count is DATA frames so far in this
    /// transfer. Receivers send an ACK every ACK_EVERY frames.
    Progress(u32),
    Complete(u8, Vec<u8>),
    /// The peer has something available for lazy transfer.
    Offer(Offer),
    /// The peer wants the offered batch with this id sent now.
    Pull(u32),
    Error(&'static str),
}

/// Cheap change signature for a file batch: relative names and sizes.
/// Used only locally for echo suppression (never compared across
/// machines), so filesystem-dependent details like mtimes stay out.
pub fn list_signature(items: impl Iterator<Item = (String, u64)>) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for (name, size) in items {
        for &b in name.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= size;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// FNV-1a, used by both endpoints for local echo suppression only.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_and_large() {
        for size in [0usize, 1, 57, 58, 59, 1000, 100_000] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let frames = encode_transfer(KIND_TEXT, &data);
            let mut r = Reassembler::default();
            let mut done = None;
            for f in &frames {
                match r.feed(f) {
                    FeedResult::Complete(kind, d) => done = Some((kind, d)),
                    FeedResult::Error(e) => panic!("size {size}: {e}"),
                    _ => {}
                }
            }
            let (kind, d) = done.expect("transfer completed");
            assert_eq!(kind, KIND_TEXT);
            assert_eq!(d, data, "size {size}");
        }
    }

    #[test]
    fn offer_and_pull_roundtrip() {
        let o = Offer { kind: KIND_TAR, total: 20_000_000, items: 2, id: 0xdeadbeef };
        let mut r = Reassembler::default();
        match r.feed(&offer_frame(&o)) {
            FeedResult::Offer(got) => assert_eq!(got, o),
            _ => panic!("offer not parsed"),
        }
        match r.feed(&pull_frame(0xdeadbeef)) {
            FeedResult::Pull(id) => assert_eq!(id, 0xdeadbeef),
            _ => panic!("pull not parsed"),
        }
    }

    #[test]
    fn sequence_gap_detected() {
        let data = vec![7u8; MAX_PAYLOAD * 5];
        let frames = encode_transfer(KIND_TEXT, &data);
        let mut r = Reassembler::default();
        r.feed(&frames[0]);
        r.feed(&frames[1]);
        // skip frames[2]
        match r.feed(&frames[3]) {
            FeedResult::Error("sequence gap") => {}
            _ => panic!("gap not detected"),
        }
    }

    #[test]
    fn oversize_start_rejected() {
        let mut start = [0u8; 5];
        start[0] = KIND_TEXT;
        start[1..5].copy_from_slice(&(MAX_TRANSFER + 1).to_le_bytes());
        let f = frame(T_START, 0, &start);
        let mut r = Reassembler::default();
        match r.feed(&f) {
            FeedResult::Error("transfer too large") => {}
            _ => panic!("oversize not rejected"),
        }
    }
}
