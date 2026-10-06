//! Content hashes as the providers report them, so a transferred file can
//! be compared with the provider's own figure.

use base64::Engine;
use md5::{Digest, Md5};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    /// OneDrive's QuickXorHash, base64.
    QuickXor,
    /// Google Drive's MD5, lowercase hex.
    Md5,
}

pub enum Hasher {
    QuickXor(QuickXor),
    Md5(Md5),
}

impl Hasher {
    pub fn new(kind: HashKind) -> Self {
        match kind {
            HashKind::QuickXor => Self::QuickXor(QuickXor::default()),
            HashKind::Md5 => Self::Md5(Md5::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::QuickXor(hasher) => hasher.update(data),
            Self::Md5(hasher) => hasher.update(data),
        }
    }

    /// The hash in the same text form the provider uses.
    pub fn finish(self) -> String {
        match self {
            Self::QuickXor(hasher) => {
                base64::engine::general_purpose::STANDARD.encode(hasher.finish())
            }
            Self::Md5(hasher) => hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }
}

const WIDTH_BITS: usize = 160;
const SHIFT_BITS: usize = 11;

/// QuickXorHash: every input byte is XORed into a 160-bit ring, each one 11
/// bits further along than the last, and the input length is XORed into the
/// final 8 bytes.
#[derive(Default)]
pub struct QuickXor {
    ring: [u8; WIDTH_BITS / 8],
    /// Bit position for the next byte.
    position: usize,
    length: u64,
}

impl QuickXor {
    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let (index, bit) = (self.position / 8, self.position % 8);
            self.ring[index] ^= byte << bit;
            if bit > 0 {
                // The high bits spill into the next byte, wrapping at the end.
                self.ring[(index + 1) % self.ring.len()] ^= byte >> (8 - bit);
            }
            self.position = (self.position + SHIFT_BITS) % WIDTH_BITS;
        }
        self.length += data.len() as u64;
    }

    pub fn finish(mut self) -> [u8; WIDTH_BITS / 8] {
        let tail = self.ring.len() - 8;
        for (slot, byte) in self.ring[tail..].iter_mut().zip(self.length.to_le_bytes()) {
            *slot ^= byte;
        }
        self.ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_xor(data: &[u8]) -> String {
        let mut hasher = Hasher::new(HashKind::QuickXor);
        hasher.update(data);
        hasher.finish()
    }

    #[test]
    fn quick_xor_of_nothing_is_all_zero() {
        assert_eq!(quick_xor(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn quick_xor_places_first_byte_at_bit_zero_and_mixes_in_the_length() {
        let mut hasher = QuickXor::default();
        hasher.update(&[0x01]);
        let hash = hasher.finish();
        assert_eq!(hash[0], 0x01);
        assert_eq!(
            hash[12], 0x01,
            "length 1 lands in the first of the last 8 bytes"
        );
    }

    #[test]
    fn quick_xor_second_byte_lands_eleven_bits_along() {
        let mut hasher = QuickXor::default();
        hasher.update(&[0x00, 0xff]);
        let hash = hasher.finish();
        // Bits 11..19: the top 5 bits of byte 1 and the low 3 bits of byte 2.
        assert_eq!((hash[1], hash[2]), (0xf8, 0x07));
    }

    #[test]
    fn quick_xor_does_not_depend_on_how_input_is_chunked() {
        let data: Vec<u8> = (0..5000u32).map(|n| (n * 31 % 251) as u8).collect();
        let mut chunked = Hasher::new(HashKind::QuickXor);
        for chunk in data.chunks(7) {
            chunked.update(chunk);
        }
        assert_eq!(chunked.finish(), quick_xor(&data));
    }

    #[test]
    fn md5_matches_the_rfc_1321_vector() {
        let mut hasher = Hasher::new(HashKind::Md5);
        hasher.update(b"abc");
        assert_eq!(hasher.finish(), "900150983cd24fb0d6963f7d28e17f72");
    }
}
