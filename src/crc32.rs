//! IEEE 802.3 CRC-32 (reflected polynomial `0xEDB88320`).
//!
//! This is the checksum zlib/PNG/zip use. Every v2 structure (base header,
//! directory section, index section, WAL header, WAL frame) is protected by it,
//! so a single bit flip anywhere in a committed byte range is detected instead
//! of being parsed as data.

/// Reflected CRC-32 polynomial (IEEE 802.3).
const POLY: u32 = 0xEDB8_8320;

/// Byte-at-a-time lookup table, computed at compile time.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC-32 of a complete byte slice.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = Crc32Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Incremental CRC-32 over a sequence of chunks.
///
/// Committing a CRC over disjoint byte ranges (a frame's length prefix plus its
/// body) must not require them to be contiguous in memory; feeding the ranges
/// to one hasher gives the same value as hashing the concatenation.
#[derive(Debug, Clone)]
pub struct Crc32Hasher {
    state: u32,
}

impl Crc32Hasher {
    /// A hasher primed for a fresh CRC.
    pub fn new() -> Self {
        Crc32Hasher { state: 0xFFFF_FFFF }
    }

    /// Fold another chunk into the running CRC.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &byte in bytes {
            let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
            crc = (crc >> 8) ^ TABLE[index];
        }
        self.state = crc;
    }

    /// Finish the CRC. Consumes the hasher: a finalized CRC cannot be extended.
    pub fn finalize(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32Hasher {
    fn default() -> Self {
        Crc32Hasher::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(Crc32Hasher::new().finalize(), 0);
    }

    #[test]
    fn well_known_vectors() {
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
        assert_eq!(crc32(&[0u8; 32]), 0x190A_55AD);
    }

    #[test]
    fn table_matches_known_prefix() {
        // First entries of the canonical reflected CRC-32 table.
        assert_eq!(TABLE[0], 0x0000_0000);
        assert_eq!(TABLE[1], 0x7707_3096);
        assert_eq!(TABLE[2], 0xEE0E_612C);
        assert_eq!(TABLE[255], 0x2D02_EF8D);
    }

    #[test]
    fn incremental_equals_one_shot_for_every_split() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let expected = crc32(&data);
        for split in [0usize, 1, 2, 7, 63, 64, 255, 256, 500, 999, 1000] {
            let mut hasher = Crc32Hasher::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finalize(), expected, "split at {split}");
        }
    }

    #[test]
    fn incremental_byte_by_byte_equals_one_shot() {
        let data = b"ndb v2 binary storage blocks";
        let expected = crc32(data);
        let mut hasher = Crc32Hasher::new();
        for byte in data {
            hasher.update(std::slice::from_ref(byte));
        }
        assert_eq!(hasher.finalize(), expected);
    }

    #[test]
    fn empty_chunks_do_not_change_the_result() {
        let mut hasher = Crc32Hasher::new();
        hasher.update(b"abc");
        hasher.update(b"");
        hasher.update(b"");
        hasher.update(b"def");
        assert_eq!(hasher.finalize(), crc32(b"abcdef"));
    }

    #[test]
    fn default_matches_new() {
        let mut hasher = Crc32Hasher::default();
        hasher.update(b"123456789");
        assert_eq!(hasher.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn single_bit_flip_changes_the_crc() {
        let data = b"the quick brown fox".to_vec();
        let base = crc32(&data);
        for bit in 0..8 {
            let mut corrupted = data.clone();
            corrupted[5] ^= 1 << bit;
            assert_ne!(crc32(&corrupted), base, "bit {bit}");
        }
    }
}
