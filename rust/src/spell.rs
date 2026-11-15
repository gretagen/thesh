//! Port of `src/spell.c` — Levenshtein typo detection.
//!
//! `best_suggestion` is generic over the candidate list because the C
//! version iterates `builtin_names()` (exec.c, M3) then the command dict;
//! callers pass that chain so the C tie-breaking order is preserved.

/// Port of `edit_distance()`: Levenshtein distance over **bytes**
/// (like the C original — UTF-8 words compare byte-wise).
///
/// Quirks preserved: >256-byte inputs short-circuit to 999, and the
/// result is capped at 4096 (unreachable for ≤256-byte inputs).
pub fn edit_distance_bytes(a: &[u8], b: &[u8]) -> i32 {
    let la = a.len();
    let lb = b.len();
    if la > 256 || lb > 256 {
        return 999;
    }

    let w = lb + 1;
    let mut prev: Vec<usize> = (0..w).collect();
    let mut cur: Vec<usize> = vec![0; w];

    for i in 1..=la {
        cur[0] = i;
        for j in 1..w {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = prev[j] + 1; // delete
            let ins = cur[j - 1] + 1; // insert
            let sub = prev[j - 1] + cost; // subs
            if ins < best {
                best = ins;
            }
            if sub < best {
                best = sub;
            }
            cur[j] = best;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let r = prev[lb];
    if r < 4096 {
        r as i32
    } else {
        4096
    }
}

/// `edit_distance()` for string arguments (byte semantics, as in C).
pub fn edit_distance(a: &str, b: &str) -> i32 {
    edit_distance_bytes(a.as_bytes(), b.as_bytes())
}

/// Port of `best_suggestion()` over an explicit candidate list — pass
/// `builtin_names()` first, then the command dictionary (C's order).
///
/// C tie-breaking rules preserved exactly:
/// * exact matches are skipped (the word "works", it's not a typo),
/// * candidates whose **first byte** differs are never tried,
/// * distance cap: 2 for words < 8 bytes, 3 for ≥ 8,
/// * on a distance tie the **shorter** candidate wins; equal lengths keep
///   the earlier candidate (builtins shadow the dict).
pub fn best_suggestion<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let wb = word.as_bytes();
    if wb.is_empty() {
        return None;
    }

    let mut best_d = i32::MAX;
    let mut best: Option<&str> = None;
    let maxd: i32 = if wb.len() >= 8 { 3 } else { 2 };

    for c in candidates {
        if c == word {
            continue;
        }
        let Some(&cb0) = c.as_bytes().first() else { continue };
        if cb0 != wb[0] {
            continue;
        }
        let d = edit_distance(word, c);
        if d <= maxd
            && (d < best_d || (d == best_d && best.map_or(true, |b| c.len() < b.len())))
        {
            best_d = d;
            best = Some(c);
        }
    }
    best
}

/// Port of `print_suggestion()` over an explicit candidate list.
/// (The C one is wired to builtins+dict; that wrapper arrives with exec.)
pub fn print_suggestion_from<'a>(
    word: &str,
    candidates: impl Iterator<Item = &'a str>,
) {
    const NAME: &str = "thesh";
    if word.is_empty() {
        eprintln!("{NAME}: no command given");
        return;
    }
    match best_suggestion(word, candidates) {
        Some(best) => eprintln!("{NAME}: no such command: '{word}'. Did you mean '{best}'?"),
        None => eprintln!("{NAME}: no such command: '{word}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_known_pairs() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("same", "same"), 0);
        assert_eq!(edit_distance("a", "b"), 1);
        assert_eq!(edit_distance("ab", "ba"), 2);
    }

    #[test]
    fn distance_is_byte_based() {
        // C compares bytes, not codepoints: é (2 bytes) vs e = 2
        assert_eq!(edit_distance("café", "cafe"), 2);
        assert_eq!(edit_distance_bytes("café".as_bytes(), "cafe".as_bytes()), 2);
    }

    #[test]
    fn distance_over_256_short_circuits_to_999() {
        let long = "a".repeat(257);
        assert_eq!(edit_distance(&long, ""), 999);
        assert_eq!(edit_distance(&long, &long), 999);
        let exact256 = "a".repeat(256);
        assert_eq!(edit_distance(&exact256, ""), 256); // boundary: computed, not 999
    }

    #[test]
    fn suggestion_skips_exact_and_other_first_bytes() {
        let cands = ["echo", "exit", "cd"];
        assert_eq!(best_suggestion("ecoh", cands.into_iter()), Some("echo"));
        assert_eq!(best_suggestion("echo", cands.into_iter()), None); // exact → skip
        assert_eq!(best_suggestion("", cands.into_iter()), None);
        assert_eq!(best_suggestion("zx", cands.into_iter()), None); // first byte
    }

    #[test]
    fn suggestion_distance_cap() {
        let cands = ["echo"];
        // "echo" (4 bytes) vs "qqqq" → distance 4 > maxd 2 → rejected
        assert_eq!(best_suggestion("qqqq", cands.into_iter()), None);
        // word ≥ 8 bytes gets maxd 3: "echotset" vs "echotest" = 2 ≤ 3 →
        // picked, while "echo" (distance 4) is rejected by the cap
        assert_eq!(
            best_suggestion("echotset", ["echo", "echotest"].into_iter()),
            Some("echotest")
        );
    }

    #[test]
    fn suggestion_tie_prefers_shorter_then_earlier() {
        // equal distance, different length → shorter wins even when listed
        // later: "abxd" (d=1) first, then "abc" (d=1, shorter) replaces it
        assert_eq!(
            best_suggestion("abcd", ["abxd", "abc"].into_iter()),
            Some("abc")
        );
        // equal distance, equal length → earlier candidate wins
        assert_eq!(
            best_suggestion("abcd", ["abxd", "abce"].into_iter()),
            Some("abxd")
        );
    }

    #[test]
    fn print_suggestion_messages() {
        // smoke: formats without panicking (output goes to stderr)
        print_suggestion_from("", ["echo"].into_iter());
        print_suggestion_from("zzzzz", ["echo"].into_iter());
        print_suggestion_from("ecoh", ["echo"].into_iter());
    }
}
