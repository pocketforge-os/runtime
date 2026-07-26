//! Frame sync + parse — the pure wire layer.
//!
//! Each MCU streams fixed 8-byte frames continuously (~48 fps, NOT push-on-change) over an
//! RX-only UART at 19200 8N1:
//!
//! ```text
//!   byte0  byte1  byte2      byte3 byte4   byte5 byte6   byte7
//!   0xFF   0x01   <BTNmask>  Xhi   Xlo     Yhi   Ylo     0xFE
//! ```
//!
//! The stick sample is **12-bit**: `X = (Xhi << 8) | Xlo`, `Y = (Yhi << 8) | Ylo` (ground truth
//! `tsp-ozbp.2`). [`FrameScanner`] is a resynchronising byte-stream framer: feed it whatever a
//! `read(2)` returned and it yields whole, validated frames, holding any partial tail across
//! calls. It never enters the kernel — it is exhaustively unit-testable against synthetic bytes.

use crate::codes;

/// The `0xFF` start-of-frame marker.
const SOF: u8 = 0xFF;
/// The `0x01` fixed second byte.
const HDR1: u8 = 0x01;
/// The `0xFE` end-of-frame marker.
const EOF: u8 = 0xFE;
/// Total frame length in bytes.
pub const FRAME_LEN: usize = 8;

/// One decoded gamepad frame from a single UART (which controls it carries depends on the UART —
/// see [`crate::decode::Side`]). `x`/`y` are already masked to the 12-bit sample range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The `byte2` button bitmask (meaning is per-UART; see [`crate::decode`]).
    pub buttons: u8,
    /// 12-bit stick X sample (`0..=4095`).
    pub x: u16,
    /// 12-bit stick Y sample (`0..=4095`).
    pub y: u16,
}

impl Frame {
    /// Parse an exactly-`FRAME_LEN` slice that has already been validated to start with `FF 01`
    /// and end with `FE`. `x`/`y` are masked to 12 bits so a stray high nibble can never inflate
    /// an axis past its declared range.
    fn from_bytes(b: &[u8]) -> Frame {
        debug_assert_eq!(b.len(), FRAME_LEN);
        let x = (((b[3] as u16) << 8) | b[4] as u16) & (codes::STICK_MAX as u16);
        let y = (((b[5] as u16) << 8) | b[6] as u16) & (codes::STICK_MAX as u16);
        Frame { buttons: b[2], x, y }
    }
}

/// A resynchronising framer. Holds bytes not yet consumed into a whole frame; [`push`] appends a
/// freshly-read chunk and returns every complete, validated frame it can now form.
///
/// Resync rule: a candidate frame is valid only when `buf[i]==0xFF && buf[i+1]==0x01 &&
/// buf[i+7]==0xFE`. On a mismatch we drop the single leading `0xFF` and rescan from the next byte,
/// so line noise or a mid-stream attach never desynchronises us permanently.
///
/// [`push`]: FrameScanner::push
#[derive(Default)]
pub struct FrameScanner {
    buf: Vec<u8>,
}

impl FrameScanner {
    /// A fresh scanner with an empty carry buffer.
    pub fn new() -> FrameScanner {
        FrameScanner { buf: Vec::with_capacity(FRAME_LEN * 4) }
    }

    /// Append `chunk` and return every whole frame now available (in stream order).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0usize; // index of the first unconsumed byte

        loop {
            // Skip to the next 0xFF start marker; drop any leading garbage before it.
            match self.buf[start..].iter().position(|&b| b == SOF) {
                Some(off) => start += off,
                None => {
                    // No start marker in the remainder — it is all garbage; discard it.
                    start = self.buf.len();
                    break;
                }
            }
            // Not enough bytes yet for a full frame from here: keep them and wait for more.
            if self.buf.len() - start < FRAME_LEN {
                break;
            }
            let cand = &self.buf[start..start + FRAME_LEN];
            if cand[1] == HDR1 && cand[FRAME_LEN - 1] == EOF {
                out.push(Frame::from_bytes(cand));
                start += FRAME_LEN;
            } else {
                // A 0xFF that is not a real frame header — drop just it and rescan from the next.
                start += 1;
            }
        }

        // Retain only the unconsumed tail, keeping the buffer bounded.
        if start > 0 {
            self.buf.drain(0..start);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(buttons: u8, x: u16, y: u16) -> [u8; 8] {
        [SOF, HDR1, buttons, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8, EOF]
    }

    #[test]
    fn parses_a_clean_frame() {
        let mut s = FrameScanner::new();
        // X = 0x0801 = 2049, Y = 0x07FF = 2047 — both inside the 12-bit range.
        let f = s.push(&frame(0x14, 0x0801, 0x07FF));
        assert_eq!(f, vec![Frame { buttons: 0x14, x: 0x0801, y: 0x07FF }]);
    }

    #[test]
    fn twelve_bit_mask_clamps_a_stray_high_nibble() {
        let mut s = FrameScanner::new();
        // Xhi carries a stray 0xF0 in its high nibble; masking must keep only the low 12 bits.
        let raw = [SOF, HDR1, 0x00, 0xF8, 0x34, 0x0A, 0xBC, EOF];
        let f = s.push(&raw);
        assert_eq!(f[0].x, 0x0834, "x masked to 12 bits");
        assert_eq!(f[0].y, 0x0ABC, "y masked to 12 bits");
    }

    #[test]
    fn frames_split_across_reads_are_reassembled() {
        let mut s = FrameScanner::new();
        let f = frame(0x22, 0x0100, 0x0200);
        assert!(s.push(&f[0..3]).is_empty(), "partial frame yields nothing yet");
        assert!(s.push(&f[3..6]).is_empty(), "still partial");
        let out = s.push(&f[6..8]);
        assert_eq!(out, vec![Frame { buttons: 0x22, x: 0x0100, y: 0x0200 }]);
    }

    #[test]
    fn back_to_back_frames_in_one_read() {
        let mut s = FrameScanner::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&frame(0x01, 0x0010, 0x0020));
        bytes.extend_from_slice(&frame(0x02, 0x0030, 0x0040));
        let out = s.push(&bytes);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].buttons, 0x01);
        assert_eq!(out[1].buttons, 0x02);
    }

    #[test]
    fn resyncs_after_leading_garbage_and_a_false_marker() {
        let mut s = FrameScanner::new();
        let mut bytes = vec![0x00, 0x13, 0x37]; // pure garbage, no marker
        // A false 0xFF whose second byte is NOT 0x01 → must be dropped, then the real frame found.
        bytes.push(SOF);
        bytes.push(0x99);
        bytes.extend_from_slice(&frame(0x40, 0x0AAA, 0x0555));
        let out = s.push(&bytes);
        assert_eq!(out, vec![Frame { buttons: 0x40, x: 0x0AAA, y: 0x0555 }]);
    }

    #[test]
    fn a_dangling_start_marker_is_held_not_dropped() {
        let mut s = FrameScanner::new();
        // A lone trailing 0xFF must be kept as a possible SOF for the next read.
        assert!(s.push(&[0x00, SOF]).is_empty());
        let rest = [HDR1, 0x55, 0x01, 0x02, 0x03, 0x04, EOF];
        let out = s.push(&rest);
        assert_eq!(out, vec![Frame { buttons: 0x55, x: 0x0102, y: 0x0304 }]);
    }
}
