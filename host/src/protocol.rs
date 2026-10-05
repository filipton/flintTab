//! Wire protocol between the host and the tablet app (all integers big-endian).
//!
//! Tablet -> host:
//!   handshake: b"TDSP", u8 version, u32 width, u32 height, u32 max_fps, u8 features
//!              (screen size in px; max_fps = highest refresh rate the tablet can both
//!              show and decode at that size; features bit 0: it can draw MSG_TILE)
//!   control:   u8 kind, u8 value
//!              KIND_AUDIO value 0/1, KIND_ACK (one video frame taken off the wire),
//!              KIND_IDR (decoder was reset, send a keyframe)
//!              KIND_POINTER value = POINTER_* action, then u16 x, u16 y
//!                (position on the stream, 0..=65535 across the width / height)
//!              KIND_SCROLL value 0, then i16 dx, i16 dy (stream pixels, finger direction)
//!              KIND_TIMING value 0, then u64 pts_us of a video frame the tablet showed, and
//!                its u64 recv_start, recv_end, queued, decoded, shown times (tablet clock, µs)
//!              KIND_PONG value 0, then u64 the ping's time (host clock), u64 when the tablet
//!                read it (tablet clock), so the host can map tablet times onto its own clock
//! Host -> tablet, a stream of frames: u8 kind, u32 len, payload
//!   MSG_CONFIG: u32 width, u32 height, u32 fps, u32 audio_rate, u8 audio_channels
//!   MSG_VIDEO:  u64 pts_us (when the frame was captured, or handed to the encoder,
//!               on the host's session clock), u16 x0, y0, x1, y1 (the area that changed
//!               since the previous video frame, 0..=65535 across the frame; the tablet only
//!               needs to redraw that), H.264 Annex-B access unit
//!   MSG_AUDIO:  interleaved signed 16-bit little-endian PCM
//!   MSG_CURSOR: u16 x, u16 y (hotspot position, 0..=65535 across the display), u8 visible.
//!               The host's own mouse, drawn by the tablet on top of the video so it moves
//!               without waiting for capture, encode and decode.
//!   MSG_CURSOR_IMAGE: u16 display_width_pt, u16 w_pt, u16 h_pt, u16 hot_x_pt, u16 hot_y_pt,
//!               then a PNG (any resolution; drawn at w_pt x h_pt display points)
//!   MSG_PING:   u64 host time (µs); the tablet answers at once with KIND_PONG
//!   MSG_TILE:   a screen update as pixels instead of video: u64 pts_us, u16 x, y, w, h
//!               (pixels, even), u32 n, then n bytes of LZ4 block-compressed Y plane (w*h
//!               bytes) and the rest LZ4-compressed interleaved CbCr (w*h/2 bytes); BT.709
//!               video range like the video. Drawn in arrival order with the video frames,
//!               acknowledged like them (KIND_ACK) and timed like them (KIND_TIMING).

use std::io::{self, Read};

pub const MAGIC: &[u8; 4] = b"TDSP";
pub const VERSION: u8 = 3;

pub const MSG_CONFIG: u8 = 1;
pub const MSG_VIDEO: u8 = 2;
pub const MSG_AUDIO: u8 = 3;
// The cursor is sent separately only on macOS; Linux keeps it in the video.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MSG_CURSOR: u8 = 4;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MSG_CURSOR_IMAGE: u8 = 5;
pub const MSG_PING: u8 = 6;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MSG_TILE: u8 = 7;

pub const KIND_AUDIO: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const KIND_IDR: u8 = 3;
pub const KIND_POINTER: u8 = 4;
pub const KIND_SCROLL: u8 = 5;
pub const KIND_TIMING: u8 = 6;
pub const KIND_PONG: u8 = 7;

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
        KIND_TIMING => 48,
        KIND_PONG => 16,
        _ => 0,
    }
}

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u8 = 2;

pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    /// The tablet draws MSG_TILE updates.
    pub tiles: bool,
}

pub const FEATURE_TILES: u8 = 1;

pub fn read_hello(r: &mut impl Read) -> io::Result<Hello> {
    let mut buf = [0u8; 18];
    r.read_exact(&mut buf)?;
    if &buf[0..4] != MAGIC || buf[4] != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad handshake"));
    }
    Ok(Hello {
        width: u32::from_be_bytes(buf[5..9].try_into().unwrap()),
        height: u32::from_be_bytes(buf[9..13].try_into().unwrap()),
        max_fps: u32::from_be_bytes(buf[13..17].try_into().unwrap()),
        tiles: buf[17] & FEATURE_TILES != 0,
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

/// The whole frame changed.
pub const ALL: [u16; 4] = [0, 0, 65535, 65535];

pub fn video_msg(pts_us: u64, changed: [u16; 4], au: &[u8]) -> Vec<u8> {
    let mut c = [0u8; 8];
    for (i, v) in changed.iter().enumerate() {
        c[2 * i..2 * i + 2].copy_from_slice(&v.to_be_bytes());
    }
    frame(MSG_VIDEO, &[&pts_us.to_be_bytes(), &c, au])
}

/// Offset of the access unit in a MSG_VIDEO message.
pub const VIDEO_HEADER: usize = 5 + 8 + 8;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn cursor_msg(x: f64, y: f64, visible: bool) -> Vec<u8> {
    let n = |v: f64| ((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_be_bytes();
    frame(MSG_CURSOR, &[&n(x), &n(y), &[visible as u8]])
}

/// Sizes are in display points; `png` is the cursor picture at any resolution.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn cursor_image_msg(display_width_pt: u16, size: (u16, u16), hotspot: (u16, u16), png: &[u8]) -> Vec<u8> {
    let mut head = Vec::with_capacity(10);
    for v in [display_width_pt, size.0, size.1, hotspot.0, hotspot.1] {
        head.extend_from_slice(&v.to_be_bytes());
    }
    frame(MSG_CURSOR_IMAGE, &[&head, png])
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn tile_msg(pts_us: u64, rect: [u16; 4], y: &[u8], uv: &[u8]) -> Vec<u8> {
    let mut head = Vec::with_capacity(20);
    head.extend_from_slice(&pts_us.to_be_bytes());
    for v in rect {
        head.extend_from_slice(&v.to_be_bytes());
    }
    head.extend_from_slice(&(y.len() as u32).to_be_bytes());
    frame(MSG_TILE, &[&head, y, uv])
}

pub fn ping_msg(host_us: u64) -> Vec<u8> {
    frame(MSG_PING, &[&host_us.to_be_bytes()])
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
        b.push(FEATURE_TILES);
        let h = read_hello(&mut &b[..]).unwrap();
        assert_eq!((h.width, h.height, h.max_fps, h.tiles), (2560, 1600, 120, true));
    }

    #[test]
    fn frame_layout() {
        let m = video_msg(7, [1, 2, 3, 4], &[1, 2, 3]);
        assert_eq!(m[0], MSG_VIDEO);
        assert_eq!(u32::from_be_bytes(m[1..5].try_into().unwrap()), 19);
        assert_eq!(&m[13..21], &[0, 1, 0, 2, 0, 3, 0, 4]);
        assert_eq!(&m[VIDEO_HEADER..], &[1, 2, 3]);
    }
}
