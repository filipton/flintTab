//! Splits an H.264 Annex-B byte stream into access units.
//!
//! ffmpeg is told to insert an Access Unit Delimiter (NAL type 9) in front of every
//! frame, so an AU ends where the next AUD starts. The last AU of a burst has no
//! following AUD yet, so `take_pending` lets the caller flush it once the stream
//! goes idle instead of delaying the frame by a whole frame interval.

#[derive(Default)]
pub struct AuSplitter {
    buf: Vec<u8>,
}

fn nal_type_at(buf: &[u8], i: usize) -> Option<u8> {
    if i + 3 < buf.len() && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
        Some(buf[i + 3] & 0x1f)
    } else {
        None
    }
}

impl AuSplitter {
    /// Feeds data, returns every access unit that is now known to be complete.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut start = 0usize; // start of current AU inside buf
        let mut i = 1usize;
        while i + 3 < self.buf.len() {
            if nal_type_at(&self.buf, i) == Some(9) {
                let cut = if self.buf[i - 1] == 0 { i - 1 } else { i };
                if cut > start {
                    out.push(self.buf[start..cut].to_vec());
                    start = cut;
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        self.buf.drain(..start);
        out
    }

    /// Returns the buffered partial AU if it already contains slice data.
    pub fn take_pending(&mut self) -> Option<Vec<u8>> {
        let has_slice = (0..self.buf.len())
            .any(|i| matches!(nal_type_at(&self.buf, i), Some(1) | Some(5)));
        if has_slice {
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 9, 0xf0, 0, 0, 1, kind];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn splits_on_aud_across_chunks() {
        let a = au(0x65, &[1, 2, 3]);
        let b = au(0x41, &[4, 5]);
        let c = au(0x41, &[6]);
        let all: Vec<u8> = [a.clone(), b.clone(), c.clone()].concat();

        let mut s = AuSplitter::default();
        let mut got = Vec::new();
        for chunk in all.chunks(3) {
            got.extend(s.push(chunk));
        }
        assert_eq!(got, vec![a, b]);
        assert_eq!(s.take_pending(), Some(c));
        assert_eq!(s.take_pending(), None);
    }

    #[test]
    fn three_byte_start_code_aud() {
        let mut s = AuSplitter::default();
        let first = vec![0, 0, 1, 9, 0xf0, 0, 0, 1, 0x41, 9];
        let second = vec![0, 0, 1, 9, 0xf0, 0, 0, 1, 0x41, 8];
        let mut got = s.push(&first);
        got.extend(s.push(&second));
        assert_eq!(got, vec![first]);
    }
}
