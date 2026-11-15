//! Golden-vector assertions. The vectors in `tests/golden_leaf.txt`
//! were generated from the C original (before its sources were retired)
//! and are checked in; this file re-computes every vector with the Rust
//! implementation and demands equality.
//!
//! Format (one vector per line, originally emitted by the C driver):
//! ```text
//! enc 00000041 -> 41 1
//! dec c3a9000000000000 -> 000000e9 2 1
//! dist "kitten" "sitting" -> 3
//! sugg "ecoh" -> "echo"
//! width 00000000 -> 0
//! ```
//!
//! `sugg` vectors need `exec::builtin_names()` — assertions light up in M3;
//! they are generated and checked in from M1 so regressions can't hide.

use std::fs;
use thesh::{spell, utf8};

fn golden() -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden_leaf.txt");
    fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("checked-in golden file missing ({e}) at {p:?}"))
}

fn hex_to_bytes(h: &str) -> Vec<u8> {
    assert!(h.len() % 2 == 0, "odd hex length: {h:?}");
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or_else(|_| panic!("bad hex {h:?}")))
        .collect()
}

/// Extract the n-th double-quoted string of a line.
fn quoted(line: &str, n: usize) -> &str {
    let mut rest = line;
    for i in 0..=n {
        rest = &rest[rest.find('"').map(|j| j + 1).expect("missing quote")..];
        let end = rest.find('"').expect("unterminated quote");
        if i == n {
            return &rest[..end];
        }
        rest = &rest[end + 1..];
    }
    unreachable!()
}

#[test]
fn golden_utf8_encode() {
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("enc ")) {
        // enc 00000041 -> 41 1
        let cp = u32::from_str_radix(&line[4..12], 16).expect("enc cp");
        let rhs = line.split("-> ").nth(1).expect("enc rhs");
        let (hex, n) = rhs.split_once(' ').expect("enc hex/n");
        let want = hex_to_bytes(hex);
        let want_n: usize = n.parse().expect("enc n");

        let mut buf = [0u8; 8];
        let got_n = utf8::encode(cp, &mut buf);
        assert_eq!(got_n, want_n, "enc {cp:08x}: length");
        assert_eq!(&buf[..got_n], &want[..], "enc {cp:08x}: bytes");
        checked += 1;
    }
    assert!(checked >= 13, "expected enc vectors, got {checked}");
}

#[test]
fn golden_utf8_decode() {
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("dec ")) {
        // dec <16 hex> -> <8 hex cp> <len> <ok>
        let input = hex_to_bytes(&line[4..20]);
        let rhs = line.split("-> ").nth(1).expect("dec rhs");
        let mut it = rhs.split_whitespace();
        let want_cp = u32::from_str_radix(it.next().expect("cp"), 16).expect("cp hex");
        let want_len: usize = it.next().expect("len").parse().expect("len");
        let want_ok: i32 = it.next().expect("ok").parse().expect("ok");

        let (cp, len, ok) = utf8::decode(&input);
        assert_eq!(
            (cp, len, ok as i32),
            (want_cp, want_len, want_ok),
            "dec {line}"
        );
        checked += 1;
    }
    assert!(checked >= 16, "expected dec vectors, got {checked}");
}

#[test]
fn golden_edit_distance() {
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("dist ")) {
        // dist "A" "B" -> N   (strings contain no quotes)
        let a = quoted(line, 0);
        let b = quoted(line, 1);
        let want: i32 = line.rsplit("-> ").next().expect("dist n").parse().expect("n");
        assert_eq!(spell::edit_distance(a, b), want, "{line}");
        checked += 1;
    }
    assert!(checked >= 10, "expected dist vectors, got {checked}");
}

#[test]
fn golden_cp_width_locale_c() {
    // The generator runs under LC_ALL=C; pin the same locale here.
    unsafe {
        libc::setlocale(libc::LC_ALL, b"C\0".as_ptr().cast());
    }
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("width ")) {
        // width 00000000 -> 0
        let cp = u32::from_str_radix(&line[6..14], 16).expect("width cp");
        let want: i32 = line.rsplit("-> ").next().expect("width w").parse().expect("w");
        assert_eq!(utf8::cp_width(cp), want, "{line}");
        checked += 1;
    }
    assert!(checked >= 10, "expected width vectors, got {checked}");
}

#[test]
fn golden_suggestions_use_builtins_only() {
    // The C generator runs with an *empty* dict (never refreshed), so
    // assert with builtins-only candidates — adding dict entries would
    // change "lst" → "ls" etc.
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("sugg ")) {
        // sugg "word" -> "best" | sugg "word" -> none
        let word = quoted(line, 0);
        let rhs = line.rsplit("-> ").next().expect("sugg rhs");
        let want = if rhs == "none" {
            None
        } else {
            Some(rhs.trim_matches('"').to_string())
        };
        let got = thesh::spell::best_suggestion(
            word,
            thesh::exec::builtin_names().iter().copied(),
        );
        assert_eq!(got, want.as_deref(), "{line}");
        checked += 1;
    }
    assert!(checked >= 9, "expected sugg vectors, got {checked}");
}

// ── config / prompt-render vectors (checked-in, C-generated) ───────────

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The golden generator and these tests run in separate processes from
/// the unit tests; only sibling tests here share state → private lock.
fn state_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Replicates the golden driver's config setup exactly (see golden.c).
fn setup_like_driver() {
    thesh::config::init();
    for l in [
        "textcolor = blue",
        "path-color = red",
        "path-opacity = 50",
        "cursorstyle = %",
        "seperatorstyle = :",
    ] {
        assert!(thesh::config::apply_line(l), "setup line: {l}");
    }
}

#[test]
fn golden_color_sgr_opacity() {
    let g = golden();
    let mut checked = 0;
    for line in g.lines().filter(|l| l.starts_with("sgr ")) {
        // sgr "31" 65 -> 33383b353b3838
        let sgr = quoted(line, 0);
        let (pre, hexv) = line.rsplit_once(" -> ").expect("sgr arrow");
        let op = pre.rsplit_once(' ').expect("sgr op").1;
        let want = hex_to_bytes(hexv);
        let got = thesh::config::color_sgr(Some(sgr), op.parse().expect("op"));
        assert_eq!(got.as_bytes(), &want[..], "{line}");
        checked += 1;
    }
    assert!(checked >= 28, "expected sgr vectors, got {checked}");
}

/// The template/ps1 string sits between the first and last quote of the
/// input half (a label like `default ` may precede it).
fn between_quotes(s: &str) -> &str {
    let a = s.find('"').expect("open quote") + 1;
    let b = s.rfind('"').expect("close quote");
    &s[a..b]
}

#[test]
fn golden_looks_and_ps1_render() {
    let _lk = state_lock();
    let g = golden();

    // The driver rendered `looks default` on a FRESH config, then set
    // its option block for the colored vectors — mirror that order.
    thesh::config::init();
    let mut options_applied = false;

    for line in g.lines() {
        if let Some(rest) = line.strip_prefix("looks ") {
            let (input, hexv) = rest.rsplit_once(" -> ").expect("looks rhs");
            let label = input.split('"').next().expect("label").trim().to_string();
            let tpl = between_quotes(input).to_string();
            if label == "default" {
                let got = thesh::config::render_looks_str(&tpl, "bob", "host", "/tmp");
                assert_eq!(hex(got.as_bytes()), hexv, "{line}");
            } else {
                if !options_applied {
                    setup_like_driver();
                    options_applied = true;
                }
                let got = thesh::config::render_looks_str(&tpl, "bob", "host", "/tmp");
                assert_eq!(hex(got.as_bytes()), hexv, "{line}");
            }
        } else if let Some(rest) = line.strip_prefix("ps1 ") {
            let (input, hexv) = rest.rsplit_once(" -> ").expect("ps1 rhs");
            // per-vector inputs, exactly as golden.c generated them
            let (tpl, dir) = if input == "root-basename" {
                ("\\W", "/")
            } else {
                (between_quotes(input), "/home/bob/proj")
            };
            // \! reads history count — the integration process starts
            // empty, matching the driver's fresh History
            let got = thesh::config::render_ps1(tpl, "bob", "host", dir);
            assert_eq!(hex(got.as_bytes()), hexv, "{line}");
        }
    }
}

#[test]
fn golden_config_dump_round_trip() {
    let _lk = state_lock();
    let g = golden();

    // expected block straight from the golden file
    let mut expect: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in g.lines() {
        if line == "cfgdump-begin" {
            inside = true;
            continue;
        }
        if line == "cfgdump-end" {
            break;
        }
        if inside {
            expect.push(line);
        }
    }
    assert!(!expect.is_empty(), "cfgdump vectors missing");

    // driver's exact option sequence (quirks included)
    thesh::config::init();
    for l in [
        "historylimit = 42",
        "typing = lowercase",
        "guesser = no",
        "corrector = consent",
        "animation = spinner",
        "movespeed = 7",
        "amplify = yes",
        "multiplexer-window-limit = 9",
        "hostname-color = white",
        "user-opacity = 55",
        "looks = x $CURSOR",
        "scrollspeed = banana", // unparsable → keep current (1)
        "historylimit = ",      // empty → 0! (C quirk)
        "animation = unknown",  // → none
    ] {
        let consumed = thesh::config::apply_line(l);
        assert!(consumed, "driver option not consumed: {l}");
    }

    let mut out: Vec<u8> = Vec::new();
    thesh::config::dump_current(&mut out).expect("dump");
    let got = String::from_utf8_lossy(&out);
    let got: Vec<&str> = got.lines().collect();
    assert_eq!(got, expect, "config dump round-trip");
}
