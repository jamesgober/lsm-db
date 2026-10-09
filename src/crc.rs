//! CRC32C (Castagnoli) for the on-disk format.
//!
//! Every data block, index block, and bloom sidecar carries a CRC32C, so the
//! checksum is part of the frozen format: every build of this crate must
//! produce bit-identical values.
//!
//! - **With `std`** (the default) this is the [`crc32c`](::crc32c) crate. It
//!   picks the SSE4.2 `crc32` instruction on x86_64 or the ARMv8 CRC extension
//!   on aarch64 at run time, with a table fallback.
//! - **Without `std`** there is no run-time CPU detection, so the choice is made
//!   at compile time. When the target enables the CRC instructions
//!   (`target_feature = "sse4.2"` on x86_64, `target_feature = "crc"` on
//!   aarch64) the [`hw`] path uses them, interleaving three independent streams
//!   to hide the instruction's latency, the same scheme the `crc32c` crate uses.
//!   Otherwise the [`sw`] path computes it with slicing-by-8 tables.
//!
//! The tables are built by `const fn` at compile time, so the `no_std` build
//! needs no build script and no allocation.

/// Computes the CRC32C of `data` (initial value `!0`, final XOR `!0`).
#[cfg(feature = "std")]
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    ::crc32c::crc32c(data)
}

/// Computes the CRC32C of `data` (initial value `!0`, final XOR `!0`).
#[cfg(all(
    not(feature = "std"),
    any(
        all(target_arch = "x86_64", target_feature = "sse4.2"),
        all(target_arch = "aarch64", target_feature = "crc")
    )
))]
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    hw::crc32c(data)
}

/// Computes the CRC32C of `data` (initial value `!0`, final XOR `!0`).
#[cfg(all(
    not(feature = "std"),
    not(any(
        all(target_arch = "x86_64", target_feature = "sse4.2"),
        all(target_arch = "aarch64", target_feature = "crc")
    ))
))]
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    sw::crc32c(data)
}

/// CRC32C polynomial `0x1EDC6F41` in reversed (LSB-first) bit order.
#[cfg(any(test, not(feature = "std"),))]
const POLY: u32 = 0x82F6_3B78;

/// Portable slicing-by-8 implementation. Compiled for `no_std` builds without
/// the CRC instructions, and always in tests so every other path can be checked
/// against it.
#[cfg(any(
    test,
    all(
        not(feature = "std"),
        not(any(
            all(target_arch = "x86_64", target_feature = "sse4.2"),
            all(target_arch = "aarch64", target_feature = "crc")
        ))
    )
))]
pub(crate) mod sw {
    use super::POLY;

    /// Slicing-by-8 tables: `TABLE[0]` is the classic byte-at-a-time table and
    /// `TABLE[k]` advances a byte through `k` further zero bytes.
    static TABLE: [[u32; 256]; 8] = build_table();

    const fn build_table() -> [[u32; 256]; 8] {
        let mut table = [[0u32; 256]; 8];
        let mut n = 0;
        while n < 256 {
            let mut crc = n as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ POLY
                };
                bit += 1;
            }
            table[0][n] = crc;
            n += 1;
        }
        let mut n = 0;
        while n < 256 {
            let mut crc = table[0][n];
            let mut k = 1;
            while k < 8 {
                crc = table[0][(crc & 0xFF) as usize] ^ (crc >> 8);
                table[k][n] = crc;
                k += 1;
            }
            n += 1;
        }
        table
    }

    /// CRC32C of `data`.
    pub(crate) fn crc32c(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        let mut words = data.chunks_exact(8);
        for w in &mut words {
            let lo = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ crc;
            let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
            crc = TABLE[7][(lo & 0xFF) as usize]
                ^ TABLE[6][((lo >> 8) & 0xFF) as usize]
                ^ TABLE[5][((lo >> 16) & 0xFF) as usize]
                ^ TABLE[4][(lo >> 24) as usize]
                ^ TABLE[3][(hi & 0xFF) as usize]
                ^ TABLE[2][((hi >> 8) & 0xFF) as usize]
                ^ TABLE[1][((hi >> 16) & 0xFF) as usize]
                ^ TABLE[0][(hi >> 24) as usize];
        }
        for &b in words.remainder() {
            crc = TABLE[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
        }
        !crc
    }
}

/// Hardware implementation for `no_std` builds whose target enables the CRC
/// instructions at compile time.
///
/// This is the only `unsafe` in the crate, and it is compiled only in this
/// configuration (the default `std` build stays `forbid(unsafe_code)`). Each
/// `unsafe` block calls one CRC intrinsic. The intrinsics' sole safety
/// requirement is that the CPU supports the instruction, which the
/// `target_feature` the module is compiled under guarantees: the same
/// guarantee every other function in a binary built for that target relies on.
#[cfg(all(
    not(feature = "std"),
    any(
        all(target_arch = "x86_64", target_feature = "sse4.2"),
        all(target_arch = "aarch64", target_feature = "crc")
    )
))]
#[allow(unsafe_code)]
pub(crate) mod hw {
    use super::POLY;

    /// Bytes per stream in the long three-way loop (power of two).
    const LONG: usize = 8192;
    /// Bytes per stream in the short three-way loop (power of two).
    const SHORT: usize = 256;

    /// Shifts a CRC register over `LONG` zero bytes.
    static LONG_SHIFT: [[u32; 256]; 4] = shift_table(LONG);
    /// Shifts a CRC register over `SHORT` zero bytes.
    static SHORT_SHIFT: [[u32; 256]; 4] = shift_table(SHORT);

    /// One CRC32C step over 8 little-endian bytes.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn step_u64(crc: u32, word: u64) -> u32 {
        // SAFETY: compiled only with `target_feature = "sse4.2"` enabled, the
        // single requirement of `_mm_crc32_u64`.
        let r = unsafe { core::arch::x86_64::_mm_crc32_u64(u64::from(crc), word) };
        r as u32
    }

    /// One CRC32C step over a single byte.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn step_u8(crc: u32, byte: u8) -> u32 {
        // SAFETY: compiled only with `target_feature = "sse4.2"` enabled, the
        // single requirement of `_mm_crc32_u8`.
        unsafe { core::arch::x86_64::_mm_crc32_u8(crc, byte) }
    }

    /// One CRC32C step over 8 little-endian bytes.
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    fn step_u64(crc: u32, word: u64) -> u32 {
        // SAFETY: compiled only with `target_feature = "crc"` enabled, the
        // single requirement of `__crc32cd`.
        unsafe { core::arch::aarch64::__crc32cd(crc, word) }
    }

    /// One CRC32C step over a single byte.
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    fn step_u8(crc: u32, byte: u8) -> u32 {
        // SAFETY: compiled only with `target_feature = "crc"` enabled, the
        // single requirement of `__crc32cb`.
        unsafe { core::arch::aarch64::__crc32cb(crc, byte) }
    }

    #[inline(always)]
    fn le_u64(bytes: &[u8]) -> u64 {
        let mut word = [0u8; 8];
        word.copy_from_slice(bytes);
        u64::from_le_bytes(word)
    }

    /// CRC32C of `data`.
    pub(crate) fn crc32c(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        let mut rest = data;
        while rest.len() >= 3 * LONG {
            let (chunk, tail) = rest.split_at(3 * LONG);
            crc = parallel3(crc, chunk, LONG, &LONG_SHIFT);
            rest = tail;
        }
        while rest.len() >= 3 * SHORT {
            let (chunk, tail) = rest.split_at(3 * SHORT);
            crc = parallel3(crc, chunk, SHORT, &SHORT_SHIFT);
            rest = tail;
        }
        let mut words = rest.chunks_exact(8);
        for w in &mut words {
            crc = step_u64(crc, le_u64(w));
        }
        for &b in words.remainder() {
            crc = step_u8(crc, b);
        }
        !crc
    }

    /// Runs three independent CRC streams over the thirds of `chunk` (each
    /// `block` bytes) and folds them together. The instruction has a latency of
    /// about three cycles, so three streams keep it busy every cycle.
    #[inline(always)]
    fn parallel3(crc: u32, chunk: &[u8], block: usize, shift: &[[u32; 256]; 4]) -> u32 {
        let (a, bc) = chunk.split_at(block);
        let (b, c) = bc.split_at(block);
        let (mut c0, mut c1, mut c2) = (crc, 0u32, 0u32);
        for ((x, y), z) in a
            .chunks_exact(8)
            .zip(b.chunks_exact(8))
            .zip(c.chunks_exact(8))
        {
            c0 = step_u64(c0, le_u64(x));
            c1 = step_u64(c1, le_u64(y));
            c2 = step_u64(c2, le_u64(z));
        }
        // The register is linear: crc(A ‖ B) = shift_|B|(crc(A)) ^ crc₀(B),
        // where crc₀ starts from zero.
        c0 = apply(shift, c0) ^ c1;
        apply(shift, c0) ^ c2
    }

    #[inline(always)]
    fn apply(table: &[[u32; 256]; 4], crc: u32) -> u32 {
        table[0][(crc & 0xFF) as usize]
            ^ table[1][((crc >> 8) & 0xFF) as usize]
            ^ table[2][((crc >> 16) & 0xFF) as usize]
            ^ table[3][(crc >> 24) as usize]
    }

    /// Multiplies the GF(2) 32x32 matrix `mat` by the vector `vec`.
    const fn gf2_times(mat: &[u32; 32], mut vec: u32) -> u32 {
        let mut sum = 0;
        let mut i = 0;
        while vec != 0 {
            if vec & 1 != 0 {
                sum ^= mat[i];
            }
            vec >>= 1;
            i += 1;
        }
        sum
    }

    const fn gf2_square(mat: &[u32; 32]) -> [u32; 32] {
        let mut sq = [0u32; 32];
        let mut i = 0;
        while i < 32 {
            sq[i] = gf2_times(mat, mat[i]);
            i += 1;
        }
        sq
    }

    /// The operator that advances a CRC register over `len` zero bytes, for
    /// `len` a power of two (the zlib `crc32_combine` construction).
    const fn zeros_operator(mut len: usize) -> [u32; 32] {
        // Operator for one zero bit.
        let mut odd = [0u32; 32];
        odd[0] = POLY;
        let mut i = 1;
        while i < 32 {
            odd[i] = 1 << (i - 1);
            i += 1;
        }
        let mut even = gf2_square(&odd); // two zero bits
        odd = gf2_square(&even); // four zero bits
        loop {
            even = gf2_square(&odd);
            len >>= 1;
            if len == 0 {
                return even;
            }
            odd = gf2_square(&even);
            len >>= 1;
            if len == 0 {
                return odd;
            }
        }
    }

    /// Byte-wise lookup tables for [`zeros_operator`]`(len)`.
    const fn shift_table(len: usize) -> [[u32; 256]; 4] {
        let op = zeros_operator(len);
        let mut table = [[0u32; 256]; 4];
        let mut n = 0;
        while n < 256 {
            let mut i = 0;
            while i < 4 {
                table[i][n] = gf2_times(&op, (n as u32) << (i * 8));
                i += 1;
            }
            n += 1;
        }
        table
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Published CRC32C check values: the CRC catalogue check string and the
    /// RFC 3720 (iSCSI) appendix B.4 vectors.
    fn known_vectors() -> Vec<(Vec<u8>, u32)> {
        let inc: Vec<u8> = (0u8..32).collect();
        let dec: Vec<u8> = (0u8..32).rev().collect();
        alloc::vec![
            (Vec::new(), 0x0000_0000),
            (b"a".to_vec(), 0xC1D0_4330),
            (b"123456789".to_vec(), 0xE306_9283),
            (b"Hello world!".to_vec(), 0x7B98_E751),
            (alloc::vec![0x00; 32], 0x8A91_36AA),
            (alloc::vec![0xFF; 32], 0x62A8_AB43),
            (inc, 0x46DD_794E),
            (dec, 0x113F_DB5C),
        ]
    }

    /// Deterministic pseudo-random bytes (xorshift64*).
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            let v = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Lengths that cross every boundary the hardware path has: the byte tail,
    /// the 8-byte words, and the short and long three-way chunks.
    fn lengths() -> Vec<usize> {
        let mut lens: Vec<usize> = (0..=80).collect();
        for base in [767, 768, 769, 1024, 4096, 4099, 24_575, 24_576, 24_577] {
            lens.push(base);
        }
        lens.push(3 * 8192 * 2 + 3 * 256 + 13);
        lens
    }

    #[test]
    fn test_known_vectors_active_path() {
        for (data, want) in known_vectors() {
            assert_eq!(crc32c(&data), want, "len {}", data.len());
        }
    }

    #[test]
    fn test_known_vectors_software_path() {
        for (data, want) in known_vectors() {
            assert_eq!(sw::crc32c(&data), want, "len {}", data.len());
        }
    }

    /// The `crc32c` crate is the reference the 1.0 format was written with.
    /// Every path must agree with it on every length and alignment.
    #[test]
    fn test_matches_reference_crate() {
        let buf = noise(3 * 8192 * 2 + 3 * 256 + 64, 0x9E37_79B9_7F4A_7C15);
        for len in lengths() {
            for offset in 0..8 {
                if offset + len > buf.len() {
                    continue;
                }
                let data = &buf[offset..offset + len];
                let want = ::crc32c::crc32c(data);
                assert_eq!(crc32c(data), want, "active path, len {len} offset {offset}");
                assert_eq!(
                    sw::crc32c(data),
                    want,
                    "software path, len {len} offset {offset}"
                );
            }
        }
    }

    #[cfg(all(
        not(feature = "std"),
        any(
            all(target_arch = "x86_64", target_feature = "sse4.2"),
            all(target_arch = "aarch64", target_feature = "crc")
        )
    ))]
    #[test]
    fn test_hardware_matches_software() {
        let buf = noise(3 * 8192 * 2 + 3 * 256 + 64, 7);
        for len in lengths() {
            for offset in 0..8 {
                if offset + len > buf.len() {
                    continue;
                }
                let data = &buf[offset..offset + len];
                assert_eq!(
                    hw::crc32c(data),
                    sw::crc32c(data),
                    "len {len} offset {offset}"
                );
            }
        }
    }

    #[test]
    fn test_single_bit_flips_change_the_checksum() {
        let data = noise(4096, 42);
        let base = crc32c(&data);
        for bit in (0..data.len() * 8).step_by(97) {
            let mut flipped = data.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert_ne!(crc32c(&flipped), base, "bit {bit}");
        }
    }
}
