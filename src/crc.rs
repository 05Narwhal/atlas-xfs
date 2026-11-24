// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! CRC32C (Castagnoli), the checksum of XFS version 5 metadata.

const POLY: u32 = 0x82F6_3B78;

// Built at compile time: every index is below 256 and the cast is of a loop
// counter below 256.
#[allow(clippy::indexing_slicing, clippy::cast_possible_truncation)]
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        let index = usize::from(u8::try_from(crc & 0xFF).unwrap_or(0) ^ b);
        // The table has 256 entries and `index` is a byte.
        crc = TABLE.get(index).copied().unwrap_or(0) ^ (crc >> 8);
    }
    crc
}

/// The CRC32C of `buf` with the four bytes at `crc_offset` counted as zero:
/// how XFS checksums a block that holds its own checksum. `None` when the
/// four bytes are not inside `buf`.
pub(super) fn crc32c_zeroed(buf: &[u8], crc_offset: usize) -> Option<u32> {
    let end = crc_offset.checked_add(4)?;
    let head = buf.get(..crc_offset)?;
    let tail = buf.get(end..)?;
    let mut crc = update(!0, head);
    crc = update(crc, &[0; 4]);
    crc = update(crc, tail);
    Some(!crc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // The standard check value of CRC32C.
        assert_eq!(!update(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn zeroed_field_is_skipped() {
        let mut a = *b"abcd1234wxyz";
        let x = crc32c_zeroed(&a, 4).unwrap();
        a[4..8].copy_from_slice(b"WXYZ");
        assert_eq!(crc32c_zeroed(&a, 4), Some(x));
        assert_eq!(crc32c_zeroed(&a, 10), None);
    }
}
