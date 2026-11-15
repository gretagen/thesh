//! Port of `src/utf8.c` — deliberately *non-validating* UTF-8, byte-for-byte
//! compatible with the C original (the shell must tolerate whatever bytes
//! arrive from a terminal or a child process).
//!
//! Cross-checked against the C implementation by `tests/golden.rs` using
//! checked-in vectors generated from the C original before it was retired.

/// Port of `utf8_decode()`: decode the first codepoint of `s`.
///
/// Returns `(cp, bytes_consumed, ok)`. On a malformed sequence the C code
/// reports the *raw first byte* as the codepoint with length 1 and
/// `ok = false`; this mirrors that exactly — including acceptance of
/// overlong forms and out-of-range values (there is no validation at all).
///
/// Deviation: for an empty slice the C function reads the NUL terminator
/// and reports `(0, 1, true)`; Rust cannot index an empty slice, so this
/// returns `(0, 0, false)`. No C caller ever passes an empty buffer.
pub fn decode(s: &[u8]) -> (u32, usize, bool) {
    let Some(&c) = s.first() else { return (0, 0, false) };
    if c < 0x80 {
        return (c as u32, 1, true);
    }
    let (n, mut code): (usize, u32) = if c & 0xE0 == 0xC0 {
        (2, (c & 0x1F) as u32)
    } else if c & 0xF0 == 0xE0 {
        (3, (c & 0x0F) as u32)
    } else if c & 0xF8 == 0xF0 {
        (4, (c & 0x07) as u32)
    } else {
        return (c as u32, 1, false);
    };
    for i in 1..n {
        // C reads s[i] here; hitting the NUL terminator fails the
        // (b & 0xC0) != 0x80 check identically to our `None`.
        let Some(&b) = s.get(i) else { return (c as u32, 1, false) };
        if b & 0xC0 != 0x80 {
            return (c as u32, 1, false);
        }
        code = (code << 6) | (b & 0x3F) as u32;
    }
    (code, n, true)
}

/// Port of `utf8_encode()`: write the UTF-8 form of `cp` into `out` and
/// return the byte count. `out` must have room for 4 bytes.
///
/// Like the C version there is no range check: `cp > 0x10FFFF` still
/// produces a 4-byte (possibly invalid) sequence — the C arithmetic
/// truncates to bytes the same way our `as u8` casts do.
pub fn encode(cp: u32, out: &mut [u8]) -> usize {
    if cp < 0x80 {
        out[0] = cp as u8;
        1
    } else if cp < 0x800 {
        out[0] = (0xC0 | (cp >> 6)) as u8;
        out[1] = (0x80 | (cp & 0x3F)) as u8;
        2
    } else if cp < 0x10000 {
        out[0] = (0xE0 | (cp >> 12)) as u8;
        out[1] = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        out[2] = (0x80 | (cp & 0x3F)) as u8;
        3
    } else {
        out[0] = (0xF0 | (cp >> 18)) as u8;
        out[1] = (0x80 | ((cp >> 12) & 0x3F)) as u8;
        out[2] = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        out[3] = (0x80 | (cp & 0x3F)) as u8;
        4
    }
}

/// Port of `cp_width()`: terminal cell width of a codepoint via `wcwidth(3)`,
/// treating anything the locale doesn't know (−1) as width 1.
///
/// Locale-dependent exactly like the C original — `main()` must call
/// `setlocale(LC_ALL, "")` first (wired in M6).
///
/// `wcwidth` is XSI but the libc crate doesn't bind it — declare it here.
mod wcwidth_ffi {
    extern "C" {
        pub fn wcwidth(c: libc::wchar_t) -> libc::c_int;
    }
}

pub fn cp_width(cp: u32) -> i32 {
    let w = unsafe { wcwidth_ffi::wcwidth(cp as libc::wchar_t) };
    if w >= 0 {
        w
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(cp: u32) -> (Vec<u8>, usize) {
        let mut buf = [0u8; 8];
        let n = encode(cp, &mut buf);
        (buf[..n].to_vec(), n)
    }

    #[test]
    fn encode_boundaries() {
        assert_eq!(enc(0x00), (vec![0x00], 1));
        assert_eq!(enc(0x41), (vec![0x41], 1));
        assert_eq!(enc(0x7f), (vec![0x7f], 1));
        assert_eq!(enc(0x80), (vec![0xc2, 0x80], 2));
        assert_eq!(enc(0x7ff), (vec![0xdf, 0xbf], 2));
        assert_eq!(enc(0x800), (vec![0xe0, 0xa0, 0x80], 3));
        assert_eq!(enc(0xffff), (vec![0xef, 0xbf, 0xbf], 3));
        assert_eq!(enc(0x10000), (vec![0xf0, 0x90, 0x80, 0x80], 4));
        assert_eq!(enc(0x10ffff), (vec![0xf4, 0x8f, 0xbf, 0xbf], 4));
    }

    #[test]
    fn encode_no_range_check_like_c() {
        // C emits a 4-byte form for out-of-range cps; same truncation.
        let (v, n) = enc(0x1fffff);
        assert_eq!((v.len(), n), (4, 4));
        let (v, n) = enc(u32::MAX);
        assert_eq!((v.len(), n), (4, 4));
        assert_eq!(v, vec![0xff, 0xbf, 0xbf, 0xbf]);
    }

    #[test]
    fn decode_valid() {
        assert_eq!(decode(b"A"), (0x41, 1, true));
        assert_eq!(decode("é".as_bytes()), (0xe9, 2, true));
        assert_eq!(decode("中".as_bytes()), (0x4e2d, 3, true));
        assert_eq!(decode("😀".as_bytes()), (0x1f600, 4, true));
    }

    #[test]
    fn decode_malformed_reports_raw_byte() {
        // bad continuation → (first byte, 1, false), like C
        assert_eq!(decode(&[0xe4, 0x41, 0x2e]), (0xe4, 1, false));
        // truncated at the NUL C would read
        assert_eq!(decode(&[0xe4, 0xb8]), (0xe4, 1, false));
        assert_eq!(decode(&[0xc2]), (0xc2, 1, false));
        // lone continuation byte / non-lead bytes
        assert_eq!(decode(&[0x80]), (0x80, 1, false));
        assert_eq!(decode(&[0xf8]), (0xf8, 1, false));
        assert_eq!(decode(&[0xff]), (0xff, 1, false));
    }

    #[test]
    fn decode_accepts_overlong_and_surrogates_like_c() {
        // no validation whatsoever — C accepts these, so do we
        assert_eq!(decode(&[0xc0, 0x80]), (0, 2, true));
        assert_eq!(decode(&[0xe0, 0x80, 0x80]), (0, 3, true));
        assert_eq!(decode(&[0xed, 0xa0, 0x80]), (0xd800, 3, true));
        // 5-byte lead pattern f5: (c & 0xF8) == 0xF0 → treated as 4-byte
        assert_eq!(decode(&[0xf5, 0x80, 0x80, 0x80]), (0x140000, 4, true));
    }

    #[test]
    fn decode_empty_slice_documented_deviation() {
        // C reads the NUL terminator: (0, 1, true). We cannot; documented
        // deviation in the fn docs — no C caller passes an empty buffer.
        assert_eq!(decode(b""), (0, 0, false));
    }

    #[test]
    fn roundtrip_ascii_and_multibyte() {
        for cp in [0x41u32, 0x7f, 0xe9, 0x4e2d, 0x1f600, 0x10ffff] {
            let mut buf = [0u8; 8];
            let n = encode(cp, &mut buf);
            assert_eq!(decode(&buf[..n]), (cp, n, true), "cp U+{cp:04X}");
        }
    }

    #[test]
    fn width_ascii_is_one_and_nul_is_zero() {
        // locale-independent assertions only (full width parity is
        // locale-dependent and checked via golden vectors under LC_ALL=C)
        assert_eq!(cp_width(0x41), 1);
        assert_eq!(cp_width(0x00), 0); // wcwidth(NUL) == 0 → returned as-is
        assert!(cp_width(0x4e2d) >= 1); // -1 fallback → 1, or real width
    }
}
