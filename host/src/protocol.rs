//! Wire protocol between the host and the tablet app (all integers big-endian).
//!
//! Tablet -> host:
//!   handshake: b"TDSP", u8 version, u32 width, u32 height, u32 max_fps
//!              (screen size in px; max_fps = highest refresh rate the tablet can both
//!              show and decode at that size)
//!   control:   u8 kind, u8 value
//!              KIND_AUDIO value 0/1, KIND_ACK (one video frame taken off the wire),
//!              KIND_IDR (decoder was reset, send a keyframe)
//!              KIND_POINTER value = POINTER_* action, then u16 x, u16 y
//!                (position on the stream, 0..=65535 across the width / height)
//!              KIND_SCROLL value 0, then i16 dx, i16 dy (stream pixels, finger direction)
//!              KIND_SHOWN value 0, then u64 pts_us of a video frame the tablet just put on
//!                screen (the host's clock, so the host can measure end-to-end latency)
//! Host -> tablet, a stream of frames: u8 kind, u32 len, payload
//!   MSG_CONFIG: u32 width, u32 height, u32 fps, u32 audio_rate, u8 audio_channels
//!   MSG_VIDEO:  u64 pts_us (when the frame was captured, or handed to the encoder,
//!               on the host's session clock), H.264 Annex-B access unit
//!   MSG_AUDIO:  interleaved signed 16-bit little-endian PCM

use std::io::{self, Read};

pub const MAGIC: &[u8; 4] = b"TDSP";
pub const VERSION: u8 = 2;

pub const MSG_CONFIG: u8 = 1;
pub const MSG_VIDEO: u8 = 2;
pub const MSG_AUDIO: u8 = 3;

pub const KIND_AUDIO: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const KIND_IDR: u8 = 3;
pub const KIND_POINTER: u8 = 4;
pub const KIND_SCROLL: u8 = 5;
pub const KIND_SHOWN: u8 = 6;

pub const POINTER_MOVE: u8 = 0; // no button held (pen hover, cursor placement)
pub const POINTER_LEFT_DOWN: u8 = 1;
pub const POINTER_DRAG: u8 = 2; // move with the left button held
pub const POINTER_LEFT_UP: u8 = 3;
pub const POINTER_RIGHT_DOWN: u8 = 4;
pub const POINTER_RIGHT_UP: u8 = 5;

/// Bytes that follow the 2-byte header of a control message of this kind.
pub fn control_payload_len(kind: u8) -> usize {
    match kind {
        KIND_POINTER | KIND_SCROLL => 4,
        KIND_SHOWN => 8,
        _ => 0,
    }
}

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u8 = 2;

pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
}

pub fn read_hello(r: &mut impl Read) -> io::Result<Hello> {
    let mut buf = [0u8; 17];
    r.read_exact(&mut buf)?;
    if &buf[0..4] != MAGIC || buf[4] != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad handshake"));
    }
    Ok(Hello {
        width: u32::from_be_bytes(buf[5..9].try_into().unwrap()),
        height: u32::from_be_bytes(buf[9..13].try_into().unwrap()),
        max_fps: u32::from_be_bytes(buf[13..17].try_into().unwrap()),
    })
}

/// Builds a complete framed message ready to be written to the socket.
pub fn frame(kind: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(5 + len);
    out.push(kind);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

pub fn config_msg(width: u32, height: u32, fps: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(17);
    p.extend_from_slice(&width.to_be_bytes());
    p.extend_from_slice(&height.to_be_bytes());
    p.extend_from_slice(&fps.to_be_bytes());
    p.extend_from_slice(&AUDIO_RATE.to_be_bytes());
    p.push(AUDIO_CHANNELS);
    frame(MSG_CONFIG, &[&p])
}

pub fn video_msg(pts_us: u64, au: &[u8]) -> Vec<u8> {
    frame(MSG_VIDEO, &[&pts_us.to_be_bytes(), au])
}

pub fn audio_msg(pcm: &[u8]) -> Vec<u8> {
    frame(MSG_AUDIO, &[pcm])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.extend_from_slice(&2560u32.to_be_bytes());
        b.extend_from_slice(&1600u32.to_be_bytes());
        b.extend_from_slice(&120u32.to_be_bytes());
        let h = read_hello(&mut &b[..]).unwrap();
        assert_eq!((h.width, h.height, h.max_fps), (2560, 1600, 120));
    }

    #[test]
    fn frame_layout() {
        let m = video_msg(7, &[1, 2, 3]);
        assert_eq!(m[0], MSG_VIDEO);
        assert_eq!(u32::from_be_bytes(m[1..5].try_into().unwrap()), 11);
        assert_eq!(&m[13..], &[1, 2, 3]);
    }
}
