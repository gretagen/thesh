//! Port of `src/bind.c` — user key bindings from the config.
//!
//! Config syntax (wm-style prefix form — canonical):
//!     bind = ALT  + T = exec('top')
//!     bind = CTRL + ALT + V = paste
//!     bind = SHIFT + LEFT = exec('select-mode')
//!     bind = F11 = exec('something')
//! The bare form (`ALT + T = exec('top')`) stays fully supported.
//!
//! Locking: the bind table is a private `Mutex<Vec<Bind>>`. This module
//! must NEVER call `crate::config::*` — `config::apply_line` invokes
//! [`parse_line`] from its own critical section (lock order is always
//! config → bind, never the reverse).
//!
//! Byte fidelity: C works on raw bytes (`char buf[512]`, `cmd[256]`,
//! …). Parsing below runs on `&[u8]` so every truncation point and
//! terminator check matches `snprintf`/`strtol`/`strtok_r` exactly.
//! DEVIATION: a 511-byte cut can split a multi-byte char; such tokens
//! are rendered lossily (U+FFFD) in the stderr diagnostics where C
//! would print the raw broken bytes — stored fields go through
//! `truncate_bytes` instead (see there).

use crate::keys::*;
use std::io::Write;

pub const BIND_CMD_MAX: usize = 256;
pub const BIND_ASK_MAX: usize = 64;
/// `BIND_MAX` from thesh.h — hard cap on stored bindings.
pub const BIND_MAX: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindKind {
    Close,
    Copy,
    Paste,
    Exec,
    Verb,
}

#[derive(Clone, Debug)]
pub struct Bind {
    pub mods: i32, // MOD_* bitmask (crate::keys)
    pub key: i32,  // char code or K_* for named keys
    pub kind: BindKind,
    pub cmd: String, // exec target
    pub ask: String, // ask() prompt label ("" = none)
}

const THESH_NAME: &str = "thesh";
/// C: `char buf[512]` in `bind_parse_line` (511 usable bytes + NUL).
const LINE_BUF: usize = 512;

static BINDS: std::sync::Mutex<Vec<Bind>> = std::sync::Mutex::new(Vec::new());

fn lock() -> std::sync::MutexGuard<'static, Vec<Bind>> {
    BINDS.lock().unwrap_or_else(|p| p.into_inner())
}

// ── byte helpers (C <ctype.h>/<string.h> on unsigned char) ──────────────

/// C's `isspace()` in the C locale: space, tab, LF, VT, FF, CR.
fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C's `strcasecmp() == 0` (byte-wise ASCII case folding).
fn ieq(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// C's `svtrim()`: ASCII-space-trim, return the interior slice.
fn svtrim(s: &[u8]) -> &[u8] {
    let mut b = 0;
    while b < s.len() && c_isspace(s[b]) {
        b += 1;
    }
    let mut e = s.len();
    while e > b && c_isspace(s[e - 1]) {
        e -= 1;
    }
    &s[b..e]
}

/// Copy C's raw bytes into a Rust `String` (must be valid UTF-8),
/// cutting at `max` BYTES exactly like `snprintf(dst, max + 1, "%s", src)`.
///
/// DEVIATION: when the byte cut splits a multi-byte UTF-8 sequence the
/// partial tail is dropped (C would keep the broken bytes on disk and
/// print them raw). ASCII — the realistic case — is byte-identical.
fn truncate_bytes(b: &[u8], max: usize) -> String {
    let b = &b[..b.len().min(max)];
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(e) if e.error_len().is_none() => {
            // the cut itself split a sequence: drop the partial tail
            String::from_utf8_lossy(&b[..e.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(b).into_owned(), // interior garbage
    }
}

/// Emulation of `strtol(s, &end, 10)`: `None` when nothing parsed
/// (`end == s`), else `Some((value, end_index))` with strtol's LONG_MAX
/// / LONG_MIN saturation on overflow.
fn strtol10(s: &[u8]) -> Option<(i64, usize)> {
    let mut i = 0;
    while i < s.len() && c_isspace(s[i]) {
        i += 1;
    }
    let start = i;
    let signed = i < s.len() && (s[i] == b'-' || s[i] == b'+');
    if signed {
        i += 1;
    }
    let ds = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == ds {
        return None; // end == s: no conversion
    }
    let v = match std::str::from_utf8(&s[ds..i]).ok()?.parse::<i64>() {
        Ok(v) => {
            if signed && s[start] == b'-' {
                -v
            } else {
                v
            }
        }
        Err(_) => {
            if s[start] == b'-' {
                i64::MIN
            } else {
                i64::MAX
            }
        }
    };
    Some((v, i))
}

// ── token classification ────────────────────────────────────────────────

/// C's `mod_from()`: 0 = not a modifier.
fn mod_from(t: &[u8]) -> i32 {
    if ieq(t, b"CTRL") {
        MOD_CTRL
    } else if ieq(t, b"ALT") {
        MOD_ALT
    } else if ieq(t, b"SHIFT") {
        MOD_SHIFT
    } else if ieq(t, b"SUPER") || ieq(t, b"WIN") || ieq(t, b"WINDOWS") || ieq(t, b"META") {
        MOD_SUPER
    } else {
        0
    }
}

/// The `F1`..`F24` form shared by `key_from()` and `is_key_token()`.
fn fkey_from(t: &[u8]) -> Option<i32> {
    if t.len() >= 2 && t[0] == b'F' && t[1].is_ascii_digit() {
        // C: strtol(t + 1, &end, 10) with *end == 0, n in 1..=24.
        // A full-string decimal parse matches: the first byte is a digit
        // (no sign/space), trailing junk fails, overflow rejects the
        // same way (strtol saturates > 24; i64 parse errors → None).
        let n: i64 = std::str::from_utf8(&t[1..]).ok()?.parse().ok()?;
        if (1..=24).contains(&n) {
            return Some(K_F1 + n as i32 - 1);
        }
    }
    None
}

/// Map a key token to its key code (ASCII char or K_*). 0 = invalid.
fn key_from(t: &[u8]) -> i32 {
    if t.len() == 1 {
        let c = t[0];
        // C: isalpha → tolower (C locale = ASCII), everything else as-is
        return if c.is_ascii_alphabetic() {
            c.to_ascii_lowercase() as i32
        } else {
            c as i32
        };
    }
    if ieq(t, b"ENTER") {
        return K_ENTER;
    }
    if ieq(t, b"TAB") {
        return K_TAB;
    }
    if ieq(t, b"SPACE") {
        return b' ' as i32;
    }
    if ieq(t, b"BACKSPACE") || ieq(t, b"BACK") {
        return K_DEL;
    }
    if ieq(t, b"DELETE") || ieq(t, b"DEL") {
        return K_FDEL;
    }
    if ieq(t, b"UP") {
        return K_UP;
    }
    if ieq(t, b"DOWN") {
        return K_DOWN;
    }
    if ieq(t, b"LEFT") {
        return K_LEFT;
    }
    if ieq(t, b"RIGHT") {
        return K_RIGHT;
    }
    // R-ARROW style tokens (arrive as word-keys when modified)
    if ieq(t, b"U-ARROW") || ieq(t, b"UP-ARROW") {
        return K_UP;
    }
    if ieq(t, b"D-ARROW") || ieq(t, b"DOWN-ARROW") {
        return K_DOWN;
    }
    if ieq(t, b"L-ARROW") || ieq(t, b"LEFT-ARROW") {
        return K_LEFT;
    }
    if ieq(t, b"R-ARROW") || ieq(t, b"RIGHT-ARROW") {
        return K_RIGHT;
    }
    if ieq(t, b"HOME") {
        return K_HOME;
    }
    if ieq(t, b"END") {
        return K_END;
    }
    if ieq(t, b"PGUP") || ieq(t, b"PAGEUP") {
        return K_PGUP;
    }
    if ieq(t, b"PGDN") || ieq(t, b"PAGEDOWN") {
        return K_PGDN;
    }
    if ieq(t, b"ESC") || ieq(t, b"ESCAPE") {
        return 27;
    }
    fkey_from(t).unwrap_or(0)
}

/// Is `t` a bindable named key (for plain-key binds like `F11 = ...`)?
fn is_key_token(t: &[u8]) -> bool {
    if fkey_from(t).is_some() {
        return true;
    }
    ieq(t, b"ENTER")
        || ieq(t, b"TAB")
        || ieq(t, b"SPACE")
        || ieq(t, b"BACKSPACE")
        || ieq(t, b"BACK")
        || ieq(t, b"DELETE")
        || ieq(t, b"DEL")
        || ieq(t, b"UP")
        || ieq(t, b"DOWN")
        || ieq(t, b"LEFT")
        || ieq(t, b"RIGHT")
        || ieq(t, b"U-ARROW")
        || ieq(t, b"UP-ARROW")
        || ieq(t, b"D-ARROW")
        || ieq(t, b"DOWN-ARROW")
        || ieq(t, b"L-ARROW")
        || ieq(t, b"LEFT-ARROW")
        || ieq(t, b"R-ARROW")
        || ieq(t, b"RIGHT-ARROW")
        || ieq(t, b"HOME")
        || ieq(t, b"END")
        || ieq(t, b"PGUP")
        || ieq(t, b"PAGEUP")
        || ieq(t, b"PGDN")
        || ieq(t, b"PAGEDOWN")
        || ieq(t, b"ESC")
        || ieq(t, b"ESCAPE")
}

/// Is `a` a multiplexer verb action (`split-window-vertical 50`,
/// `close-pane`, `focus-next`, `panes`, …)? `split-window-vertial` (the
/// typo in a long-standing rc) is tolerated as an alias.
fn is_verb_action(a: &[u8]) -> bool {
    // C: char w[48] — the first word is capped at 47 BYTES and `rest`
    // starts mid-word when it overflows (replicated exactly).
    let mut i = 0;
    while i < a.len() && !c_isspace(a[i]) && i < 47 {
        i += 1;
    }
    let w = &a[..i];
    let mut rest = &a[i..];
    while !rest.is_empty() && c_isspace(rest[0]) {
        rest = &rest[1..];
    }

    if w == b"split-window-vertical"
        || w == b"split-window-vertial"
        || w == b"split-window-horizontal"
    {
        if rest.is_empty() {
            return true;
        }
        let Some((n, mut ei)) = strtol10(rest) else { return false };
        while ei < rest.len() && c_isspace(rest[ei]) {
            ei += 1;
        }
        return ei == rest.len() && (0..=100).contains(&n);
    }
    if !rest.is_empty() {
        return false;
    }
    // C compares with strcmp — verbs are case-SENSITIVE (unlike the
    // close/copy/paste actions, which use strcasecmp).
    matches!(
        w,
        b"close-pane"
            | b"panes"
            | b"kill-curent-window"
            | b"kill-current-window"
            | b"kill-all-windows"
            | b"move-to-right-window"
            | b"move-to-left-window"
            | b"move-to-up-window"
            | b"move-to-down-window"
            | b"focus-left"
            | b"focus-right"
            | b"focus-up"
            | b"focus-down"
            | b"focus-next"
            | b"focus-prev"
    )
}

/// Parse the flattened `action` string into bind fields.
///
/// The cursor walks the byte string exactly like C: `cmd` stops at the
/// closing quote / `)` or after BIND_CMD_MAX-1 BYTES — and the `ask`
/// search then resumes from that stop point, so an `ask` buried past the
/// truncation point inside a too-long cmd is still found (C quirk,
/// preserved).
fn parse_exec_action(a: &[u8], b: &mut Bind) {
    // ── cmd ──
    let mut p = 0;
    while p < a.len() && a[p] != b'(' {
        p += 1;
    }
    if p < a.len() && a[p] == b'(' {
        p += 1;
        let cmd = if p < a.len() && (a[p] == b'"' || a[p] == b'\'') {
            let cq = a[p];
            p += 1;
            let stop = a[p..].iter().position(|&c| c == cq).map(|i| p + i).unwrap_or(a.len());
            stop.min(p + (BIND_CMD_MAX - 1))
        } else {
            let stop = a[p..].iter().position(|&c| c == b')').map(|i| p + i).unwrap_or(a.len());
            stop.min(p + (BIND_CMD_MAX - 1))
        };
        b.cmd = truncate_bytes(&a[p..cmd], BIND_CMD_MAX - 1);
        p = cmd;
    }

    // ── ask (search resumes where the cmd copy stopped) ──
    let Some(off) = a[p..].windows(3).position(|w| w == b"ask") else { return };
    let q = p + off;
    let c3 = a.get(q + 3).copied().unwrap_or(0); // C reads the NUL past "ask"
    if c3 != b'(' && !c_isspace(c3) {
        return;
    }
    let mut ap = q + 3;
    while ap < a.len() && a[ap] != b'(' {
        ap += 1;
    }
    if ap >= a.len() {
        return;
    }
    ap += 1;
    let ask = if ap < a.len() && (a[ap] == b'"' || a[ap] == b'\'') {
        let cq = a[ap];
        ap += 1;
        let stop = a[ap..].iter().position(|&c| c == cq).map(|i| ap + i).unwrap_or(a.len());
        stop.min(ap + (BIND_ASK_MAX - 1))
    } else {
        let stop = a[ap..].iter().position(|&c| c == b')').map(|i| ap + i).unwrap_or(a.len());
        stop.min(ap + (BIND_ASK_MAX - 1))
    };
    b.ask = truncate_bytes(&a[ap..ask], BIND_ASK_MAX - 1);
}

// ── parse_line ──────────────────────────────────────────────────────────

/// Parse one `bind = …` / `MOD + KEY = action` line. Returns true when
/// the line was a binding (consumed as config).
pub fn parse_line(line: &str) -> bool {
    // C: snprintf(buf, sizeof buf, "%s", line) — stops at the first NUL
    // and truncates at 511 BYTES.
    let raw = line.as_bytes();
    let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let buf = &raw[..nul.min(LINE_BUF - 1)];

    let mut t = svtrim(buf);
    if t.is_empty() || t[0] == b'#' {
        return false;
    }

    // wm-style prefix: `bind = ALT + C = action` (also `bind :` — the
    // legacy separator is accepted like everywhere else in the config).
    // The prefix is stripped here and the remainder runs through the
    // exact same grammar as the bare form, which stays supported.
    if t.len() >= 4 && ieq(&t[..4], b"bind") {
        let c4 = t.get(4).copied().unwrap_or(0);
        if c4 == 0 || c_isspace(c4) || c4 == b'=' || c4 == b':' {
            let mut p = 4;
            while p < t.len() && c_isspace(t[p]) {
                p += 1;
            }
            if p >= t.len() || (t[p] != b'=' && t[p] != b':') {
                eprintln!("{THESH_NAME}: bind: expected '=' after 'bind'");
                return true;
            }
            t = svtrim(&t[p + 1..]);
            if t.is_empty() {
                eprintln!("{THESH_NAME}: bind: nothing to bind after 'bind = '");
                return true;
            }
        }
    }

    // First word must be a modifier (CTRL/ALT/SUPER/SHIFT/...) or a named
    // key, so plain `F11 = ...` / `UP = ...` binds parse, while ordinary
    // command/config lines fall through. Stops only at space / '+' / '='.
    let mut sp = 0;
    while sp < t.len() && !c_isspace(t[sp]) && t[sp] != b'+' && t[sp] != b'=' {
        sp += 1;
    }
    if sp == 0 || sp >= 16 {
        return false;
    }
    let first = &t[..sp];
    if mod_from(first) == 0 && !is_key_token(first) {
        return false;
    }

    let Some(eq) = t.iter().position(|&b| b == b'=') else { return false };
    let action = svtrim(&t[eq + 1..]);
    if action.is_empty() {
        return false;
    }
    let left = &t[..eq]; // C: `*eq = 0` truncates the left side

    // C: strtok_r(t, "+", …) — collapses runs of '+', skips leading /
    // trailing ones, and stops collecting after 16 tokens (extras are
    // silently ignored). Each token is svtrim'ed (an all-space token
    // survives strtok and trims to "" — kept here too).
    let toks: Vec<&[u8]> = left
        .split(|&b| b == b'+')
        .filter(|s| !s.is_empty())
        .map(svtrim)
        .take(16)
        .collect();
    if toks.is_empty() {
        return false;
    }

    let mut mods = 0;
    for tk in &toks[..toks.len() - 1] {
        let m = mod_from(tk);
        if m == 0 {
            eprintln!(
                "{THESH_NAME}: bind: unknown modifier '{}' (use CTRL/ALT/SUPER/SHIFT)",
                String::from_utf8_lossy(tk)
            );
            return true;
        }
        mods |= m;
    }
    let key = key_from(toks[toks.len() - 1]);
    if key == 0 {
        eprintln!(
            "{THESH_NAME}: bind: unknown key '{}'",
            String::from_utf8_lossy(toks[toks.len() - 1])
        );
        return true;
    }

    // SHIFT + <single character> can never fire: the terminal sends a
    // capital letter/symbol as a plain byte with no shift distinction.
    // Reject it loudly instead of storing a dead binding.
    if (mods & MOD_SHIFT) != 0 && ((key >= 32 && key <= 126) || key == 27) {
        eprintln!(
            "{THESH_NAME}: bind: SHIFT + plain keys never fire \
             (terminals send letters/symbols without a shift bit)"
        );
        return true;
    }

    let mut b = Bind {
        mods,
        key,
        kind: BindKind::Close, // set by the dispatch below
        cmd: String::new(),
        ask: String::new(),
    };

    // Strip a matching pair of outer quotes from the action.
    let al = action.len();
    let action = if al >= 2
        && (action[0] == b'"' || action[0] == b'\'')
        && action[al - 1] == action[0]
    {
        &action[1..al - 1]
    } else {
        action
    };

    if ieq(action, b"close") {
        b.kind = BindKind::Close;
    } else if ieq(action, b"copy") {
        b.kind = BindKind::Copy;
    } else if ieq(action, b"paste") {
        b.kind = BindKind::Paste;
    } else if action.len() >= 4
        && ieq(&action[..4], b"exec")
        && matches!(action.get(4), Some(&c) if c == b'(' || c_isspace(c))
    {
        b.kind = BindKind::Exec;
        parse_exec_action(action, &mut b);
        if b.cmd.is_empty() {
            eprintln!("{THESH_NAME}: bind: exec action needs a command");
            return true;
        }
    } else if is_verb_action(action) {
        b.kind = BindKind::Verb;
        b.cmd = truncate_bytes(action, BIND_CMD_MAX - 1);
    } else {
        eprintln!(
            "{THESH_NAME}: bind: unknown action '{}'",
            String::from_utf8_lossy(action)
        );
        return true;
    }

    let mut binds = lock();
    for existing in binds.iter_mut() {
        if existing.mods == b.mods && existing.key == b.key {
            *existing = b; // last definition wins
            return true;
        }
    }
    if binds.len() < BIND_MAX {
        binds.push(b);
    } else {
        eprintln!("{THESH_NAME}: bind: too many bindings ({BIND_MAX} max)");
    }
    true
}

// ── lookup ──────────────────────────────────────────────────────────────

/// Look up the binding for (mods, key), with the C fallback rules.
pub fn lookup(mods: i32, key: i32) -> Option<Bind> {
    // Ali/Ctrl+arrow presses arrive as the word-move keys (K_WLEFT /
    // K_WRIGHT), so a bind stored as (ALT, K_LEFT) must also match the
    // arrival (ALT, K_WLEFT). Exact candidates come first, then the
    // word-key alias, with and without Shift. The modset loop is the
    // OUTER loop in C — the unshifted modset is only tried after every
    // key of the requested modset.
    let keys = [
        key,
        if key == K_WLEFT {
            K_LEFT
        } else if key == K_WRIGHT {
            K_RIGHT
        } else {
            key
        },
    ];
    let nkeys = if keys[1] != keys[0] { 2 } else { 1 };
    let modset = [mods, mods & !MOD_SHIFT];
    let nmodset = if modset[1] != mods { 2 } else { 1 };

    let binds = lock();
    for m in 0..nmodset {
        for k in 0..nkeys {
            for b in binds.iter() {
                if b.mods == modset[m] && b.key == keys[k] {
                    return Some(b.clone());
                }
            }
        }
    }
    None
}

// ── preset export ───────────────────────────────────────────────────────

fn key_token_name(key: i32) -> Option<String> {
    match key {
        k if k == K_ENTER => Some("ENTER".into()),
        k if k == K_TAB => Some("TAB".into()),
        k if k == b' ' as i32 => Some("SPACE".into()),
        k if k == K_DEL => Some("BACKSPACE".into()),
        k if k == K_FDEL => Some("DELETE".into()),
        k if k == K_UP => Some("UP".into()),
        k if k == K_DOWN => Some("DOWN".into()),
        k if k == K_LEFT => Some("LEFT".into()),
        k if k == K_RIGHT => Some("RIGHT".into()),
        k if k == K_HOME => Some("HOME".into()),
        k if k == K_END => Some("END".into()),
        k if k == K_PGUP => Some("PAGEUP".into()),
        k if k == K_PGDN => Some("PAGEDOWN".into()),
        k if k == 27 => Some("ESC".into()),
        k if (K_F1..=K_F24).contains(&k) => Some(format!("F{}", k - K_F1 + 1)),
        _ => None,
    }
}

fn dump_bind_action(f: &mut dyn Write, b: &Bind) -> std::io::Result<()> {
    match b.kind {
        BindKind::Close => writeln!(f, "close")?,
        BindKind::Copy => writeln!(f, "copy")?,
        BindKind::Paste => writeln!(f, "paste")?,
        BindKind::Exec => {
            write!(f, "exec(")?;
            if !b.cmd.contains('\'') {
                write!(f, "'{}'", b.cmd)?;
            } else {
                write!(f, "\"{}\"", b.cmd)?;
            }
            write!(f, ")")?;
            if !b.ask.is_empty() {
                write!(f, " ask(")?;
                if !b.ask.contains('\'') {
                    write!(f, "'{}'", b.ask)?;
                } else {
                    write!(f, "\"{}\"", b.ask)?;
                }
                write!(f, ")")?;
            }
            writeln!(f)?;
        }
        BindKind::Verb => writeln!(f, "{}", b.cmd)?,
    }
    Ok(())
}

/// Serialize all bindings (`bind = …` lines) — used by savepreset.
///
/// Canonical wm-style form: `bind = CTRL + SHIFT + LEFT = exec('…')`.
/// A modifier-less bind emits `bind = F11 = …` (no stray separator).
/// Write errors propagate (C's `fputs` results are ignored; the only
/// caller, `savepreset`, discards them like C does).
pub fn dump(f: &mut dyn Write) -> std::io::Result<()> {
    let binds = lock().clone(); // release the lock before any I/O
    for b in &binds {
        let mut sep = "";
        write!(f, "bind = ")?;
        if (b.mods & MOD_CTRL) != 0 {
            write!(f, "CTRL")?;
            sep = " + ";
        }
        if (b.mods & MOD_ALT) != 0 {
            write!(f, "{sep}ALT")?;
            sep = " + ";
        }
        if (b.mods & MOD_SUPER) != 0 {
            write!(f, "{sep}SUPER")?;
            sep = " + ";
        }
        if (b.mods & MOD_SHIFT) != 0 {
            write!(f, "{sep}SHIFT")?;
            sep = " + ";
        }
        write!(f, "{sep}")?;
        match key_token_name(b.key) {
            Some(nm) => write!(f, "{nm}")?,
            None => {
                // C: key >= 0 && key < 256 && isgraph(key) → toupper
                if (33..=126).contains(&b.key) {
                    write!(f, "{}", (b.key as u8).to_ascii_uppercase() as char)?;
                } else {
                    write!(f, "?")?;
                }
            }
        }
        write!(f, " = ")?;
        dump_bind_action(f, b)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clears the bind table while holding the shared test lock; the
    /// table is emptied again when the guard drops — still under the
    /// lock, so parallel tests never observe a dirty table.
    struct BindGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for BindGuard {
        fn drop(&mut self) {
            lock().clear();
        }
    }

    fn bind_guard() -> BindGuard {
        let g = crate::util::test_lock();
        lock().clear();
        BindGuard { _lock: g }
    }

    fn dump_str() -> String {
        let mut v = Vec::new();
        dump(&mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn rejects_plain_config_and_command_lines() {
        let _g = bind_guard();
        for line in [
            "",
            "   ",
            "# comment",
            "  # indented",
            "echo hi",
            "ls -la",
            "history = yes",
            "PATH = /usr/bin",     // first word is not a mod/named key
            "T = close",           // bare single letter is not bindable
            "F = close",           // lone "F" is not a named key
            "F11",                 // named key but no '='
            "CTRL +",              // modifier but no '='
            "bindings = x",        // "bind" prefix needs a separator
            "bindx = ALT + T = paste",
            "ALT:T = paste",       // first word stops only at ws/+/=
            "bind = ALT + T = ",   // empty action after the '='
        ] {
            assert!(!parse_line(line), "{line:?} must not parse as a bind");
        }
        assert!(dump_str().is_empty());
        assert!(lookup(MOD_CTRL, 'a' as i32).is_none());
    }

    #[test]
    fn prefix_and_bare_forms_accepted() {
        let _g = bind_guard();
        assert!(parse_line("bind = ALT + T = exec('top')"));
        assert!(parse_line("bind : CTRL + C = copy"));
        assert!(parse_line("BIND = SUPER + W = paste"));
        assert!(parse_line("CTRL + A = close")); // bare form
        assert!(parse_line("ALT+D=paste")); // no spaces at all
        assert!(parse_line("bind=CTRL+B=exec(ls)"));
        assert!(parse_line("F11 = exec('x')")); // no modifier
        assert!(parse_line("UP = close"));
        let d = dump_str();
        assert_eq!(d.lines().count(), 8, "{d}");
        for expect in [
            "bind = ALT + T = exec('top')",
            "bind = CTRL + C = copy",
            "bind = SUPER + W = paste",
            "bind = CTRL + A = close",
            "bind = ALT + D = paste",
            "bind = CTRL + B = exec('ls')",
            "bind = F11 = exec('x')",
            "bind = UP = close",
        ] {
            assert!(d.contains(&format!("{expect}\n")), "missing {expect:?} in:\n{d}");
        }
    }

    #[test]
    fn malformed_lines_are_consumed_but_not_stored() {
        let _g = bind_guard();
        let consumed = [
            "bind = ",                            // nothing after 'bind = '
            "bind x",                             // expected '=' after 'bind'
            "bind",                               // ditto (EOF after "bind")
            "CTRL + FOO = close",                 // unknown key
            "F11 + CTRL = close",                 // first token not a modifier
            "CTRL + A = frobnicate",              // unknown action
            "CTRL + A = exec",                    // exec without '(' or ' '
            "CTRL + A = exec foo",                // exec with no command
            "CTRL + A = exec()",                  // empty command
            "CTRL + A = exec('')",                // empty quoted command
            "CTRL + A = focus-next now",          // verb with junk argument
            "CTRL + A = split-window-vertical 101", // pct out of range
            "CTRL + A = split-window-vertical abc",
            "CTRL + A = Focus-Next", // verbs are case-sensitive
            "SHIFT + A = close",     // SHIFT + plain key
            "SHIFT + ESC = close",
            "SHIFT + SPACE = close", // key 32 is in 32..=126
        ];
        for line in consumed {
            assert!(parse_line(line), "{line:?} must be consumed (return 1)");
        }
        assert!(dump_str().is_empty(), "nothing may be stored");
    }

    #[test]
    fn shift_accepted_only_for_keys_with_modifier_params() {
        let _g = bind_guard();
        assert!(parse_line("SHIFT + LEFT = exec('x')"));
        assert!(parse_line("SHIFT + F11 = close"));
        assert!(parse_line("SHIFT + ENTER = paste"));
        assert!(parse_line("SHIFT + HOME = close"));
        let b = lookup(MOD_SHIFT, K_LEFT).unwrap();
        assert_eq!((b.kind, b.cmd.as_str()), (BindKind::Exec, "x"));
        assert_eq!(lookup(MOD_SHIFT, K_F11).unwrap().kind, BindKind::Close);
        assert_eq!(lookup(MOD_SHIFT, K_ENTER).unwrap().kind, BindKind::Paste);
        assert_eq!(lookup(MOD_SHIFT, K_HOME).unwrap().kind, BindKind::Close);
        // plain keys stay unreachable under SHIFT (rejected above)
        assert!(lookup(MOD_SHIFT, 'a' as i32).is_none());
        assert!(lookup(MOD_SHIFT, 27).is_none());
        assert!(lookup(MOD_SHIFT, b' ' as i32).is_none());
    }

    #[test]
    fn exec_actions_parse_cmd_and_ask() {
        let _g = bind_guard();
        assert!(parse_line("CTRL + R = exec('ls -l') ask('run: ')"));
        let b = lookup(MOD_CTRL, 'r' as i32).unwrap();
        assert_eq!(b.kind, BindKind::Exec);
        assert_eq!(b.cmd, "ls -l");
        assert_eq!(b.ask, "run: ");

        assert!(parse_line("CTRL + Y = exec(ls -l) ask(run)"));
        let b = lookup(MOD_CTRL, 'y' as i32).unwrap();
        assert_eq!(b.cmd, "ls -l");
        assert_eq!(b.ask, "run");

        assert!(parse_line("CTRL + U = exec(\"say 'hi'\")"));
        let b = lookup(MOD_CTRL, 'u' as i32).unwrap();
        assert_eq!(b.cmd, "say 'hi'");
        assert_eq!(b.ask, "");

        // unquoted cmd may contain a single quote (dump re-quotes it)
        assert!(parse_line("CTRL + I = exec(it's)"));
        assert_eq!(lookup(MOD_CTRL, 'i' as i32).unwrap().cmd, "it's");

        // quoted action wrappers are stripped before dispatch
        assert!(parse_line("CTRL + P = \"paste\""));
        assert_eq!(lookup(MOD_CTRL, 'p' as i32).unwrap().kind, BindKind::Paste);
        assert!(parse_line("CTRL + O = 'copy'"));
        assert_eq!(lookup(MOD_CTRL, 'o' as i32).unwrap().kind, BindKind::Copy);

        // exec without parens but with a space still routes through
        // parse_exec_action → finds the first '(' … here: no parens at
        // all → empty cmd → rejected (covered in the malformed test);
        // with parens later in the string the first '(' wins:
        assert!(parse_line("CTRL + S = exec ask('x')"));
        let b = lookup(MOD_CTRL, 's' as i32).unwrap();
        assert_eq!(b.cmd, "x"); // cmd copied from after the first '('
        assert_eq!(b.ask, "");
    }

    #[test]
    fn ask_is_found_past_the_cmd_truncation_point() {
        let _g = bind_guard();
        // Quirk: the cmd copy stops after 255 BYTES; the `ask` search
        // resumes from there, so `ask` inside the command text (at/after
        // the cut) is still parsed as the ask label.
        let line = format!("CTRL + Z = exec('{}ask('x')')", "a".repeat(255));
        assert!(parse_line(&line));
        let b = lookup(MOD_CTRL, 'z' as i32).unwrap();
        assert_eq!(b.cmd, "a".repeat(255));
        assert_eq!(b.ask, "x");
    }

    #[test]
    fn cmd_and_ask_truncate_at_buffer_caps() {
        let _g = bind_guard();
        assert!(parse_line(&format!("CTRL + J = exec({})", "a".repeat(300))));
        let b = lookup(MOD_CTRL, 'j' as i32).unwrap();
        assert_eq!(b.cmd.len(), BIND_CMD_MAX - 1); // 255 BYTES, like snprintf
        assert_eq!(b.cmd, "a".repeat(255));

        assert!(parse_line(&format!("CTRL + K = exec(ls) ask({})", "b".repeat(100))));
        let b = lookup(MOD_CTRL, 'k' as i32).unwrap();
        assert_eq!(b.ask.len(), BIND_ASK_MAX - 1); // 63 BYTES

        // multi-byte cmd: C cuts at 255 BYTES (possibly mid-sequence);
        // Rust drops the partial tail instead — DEVIATION (see
        // truncate_bytes); ASCII above is byte-identical to C.
        assert!(parse_line(&format!("CTRL + L = exec({})", "é".repeat(200))));
        let b = lookup(MOD_CTRL, 'l' as i32).unwrap();
        assert_eq!(b.cmd.as_bytes().len(), 254); // 255 would split an "é"
        assert_eq!(b.cmd.chars().count(), 127);
    }

    #[test]
    fn line_is_snprintf_truncated_to_511_bytes() {
        let _g = bind_guard();
        // '=' lands past byte 511 → the truncated buffer has no '=' →
        // the line is NOT consumed (returns 0), like C.
        let mut long = String::from("bind = CTRL + a");
        long.push_str(&" ".repeat(600));
        long.push_str("= close");
        assert!(!parse_line(&long));
        assert!(dump_str().is_empty());

        // same line with the '=' inside the 512-byte buffer parses
        let mut short = String::from("bind = CTRL + a");
        short.push_str(&" ".repeat(10));
        short.push_str("= close");
        assert!(parse_line(&short));
        assert_eq!(dump_str(), "bind = CTRL + A = close\n");
    }

    #[test]
    fn single_char_keys_are_lowercased() {
        let _g = bind_guard();
        assert!(parse_line("CTRL + T = paste"));
        assert!(lookup(MOD_CTRL, 't' as i32).is_some());
        assert!(lookup(MOD_CTRL, 'T' as i32).is_none());
    }

    #[test]
    fn last_definition_wins() {
        let _g = bind_guard();
        assert!(parse_line("CTRL + T = paste"));
        assert!(parse_line("CTRL + T = exec('top')"));
        assert_eq!(dump_str(), "bind = CTRL + T = exec('top')\n");
        assert_eq!(lookup(MOD_CTRL, 't' as i32).unwrap().cmd, "top");
    }

    #[test]
    fn bind_table_caps_at_bind_max() {
        let _g = bind_guard();
        let keys: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789"
            .chars()
            .take(BIND_MAX + 1)
            .collect();
        assert_eq!(keys.len(), BIND_MAX + 1);
        for (i, k) in keys.iter().enumerate() {
            assert!(parse_line(&format!("CTRL + {k} = paste")), "bind {i} must be consumed");
        }
        let d = dump_str();
        assert_eq!(d.lines().count(), BIND_MAX, "table caps at BIND_MAX: {d}");
        // the overflow binding was rejected …
        assert!(lookup(MOD_CTRL, keys[BIND_MAX] as i32).is_none());
        // … but earlier ones survive, in insertion order
        assert!(lookup(MOD_CTRL, keys[0] as i32).is_some());
        assert!(d.starts_with("bind = CTRL + A = paste\n"));
    }

    #[test]
    fn lookup_falls_back_from_word_keys_and_shift() {
        let _g = bind_guard();
        assert!(lookup(MOD_ALT, K_LEFT).is_none()); // empty table

        assert!(parse_line("ALT + LEFT = exec('left')"));
        assert!(parse_line("CTRL + SHIFT + LEFT = exec('cs-left')"));

        // exact candidate first
        assert_eq!(lookup(MOD_CTRL | MOD_SHIFT, K_LEFT).unwrap().cmd, "cs-left");
        // Ctrl+arrow arrives as K_WLEFT → alias to K_LEFT; the exact
        // modset (with SHIFT) is still tried before the unshifted one
        assert_eq!(lookup(MOD_CTRL | MOD_SHIFT, K_WLEFT).unwrap().cmd, "cs-left");
        // ALT was never bound with SHIFT → the unshifted modset matches
        assert_eq!(lookup(MOD_ALT | MOD_SHIFT, K_WLEFT).unwrap().cmd, "left");
        assert_eq!(lookup(MOD_ALT, K_WLEFT).unwrap().cmd, "left");
        // K_WRIGHT aliases to K_RIGHT, which has no binding
        assert!(lookup(MOD_ALT, K_WRIGHT).is_none());

        // an exact word-key bind beats the LEFT alias (modset is the
        // outer loop, keys the inner one — same as C)
        lock().push(Bind {
            mods: MOD_ALT,
            key: K_WRIGHT,
            kind: BindKind::Close,
            cmd: String::new(),
            ask: String::new(),
        });
        assert_eq!(lookup(MOD_ALT, K_WRIGHT).unwrap().kind, BindKind::Close);
        // and with SHIFT requested: exact modset first (no K_WRIGHT
        // there), then the unshifted modset's exact word key
        assert_eq!(lookup(MOD_ALT | MOD_SHIFT, K_WRIGHT).unwrap().kind, BindKind::Close);
    }

    #[test]
    fn dump_is_canonical_wm_style() {
        let _g = bind_guard();
        assert!(parse_line("ALT + CTRL + V = paste")); // mods reordered on dump
        assert!(parse_line("SHIFT + LEFT = exec('ls')"));
        assert!(parse_line("CTRL + R = exec('ls -l') ask('run')"));
        assert!(parse_line("CTRL + Q = exec(it's) ask(don't)"));
        assert!(parse_line("F11 = close"));
        assert!(parse_line("CTRL + SPACE = copy"));
        assert!(parse_line("CTRL + @ = close"));
        assert!(parse_line("ESC = copy"));
        assert!(parse_line("CTRL + N = focus-next"));
        let expect = [
            "bind = CTRL + ALT + V = paste",
            "bind = SHIFT + LEFT = exec('ls')",
            "bind = CTRL + R = exec('ls -l') ask('run')",
            "bind = CTRL + Q = exec(\"it's\") ask(\"don't\")",
            "bind = F11 = close",
            "bind = CTRL + SPACE = copy",
            "bind = CTRL + @ = close",
            "bind = ESC = copy",
            "bind = CTRL + N = focus-next",
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>();
        assert_eq!(dump_str(), expect);
    }

    /// The bound key of a `CTRL + <k> = …` line (test helper).
    fn key_of(line: &str) -> i32 {
        let left = line.split('=').next().unwrap();
        let tok = left.split('+').last().unwrap().trim();
        key_from(tok.as_bytes())
    }

    #[test]
    fn verb_action_grammar() {
        let _g = bind_guard();
        // accepted verbs (case-sensitive) with optional percentage
        for line in [
            "CTRL + 1 = focus-next",
            "CTRL + 2 = panes",
            "CTRL + 3 = kill-curent-window", // tolerated typo
            "CTRL + 4 = split-window-vertical",
            "CTRL + 5 = split-window-vertial 50", // tolerated typo
            "CTRL + 6 = split-window-horizontal 0",
            "CTRL + 7 = split-window-vertical 100",
            "CTRL + 8 = split-window-vertical  50 ", // ws around pct
            "CTRL + 9 = focus-next ", // action is svtrim'ed first
        ] {
            assert!(parse_line(line), "{line:?} should be a verb");
            let b = lookup(MOD_CTRL, key_of(line)).unwrap();
            assert_eq!(b.kind, BindKind::Verb, "{line:?}");
            let want = line.split("= ").nth(1).unwrap().trim();
            assert_eq!(b.cmd, want, "{line:?}");
        }
        // rejected forms (consumed, but nothing stored)
        for line in [
            "CTRL + 0 = split-window-vertical -1",
            "CTRL + 0 = split-window-vertical 101",
            "CTRL + 0 = split-window-vertical 50 x",
            "CTRL + 0 = close-pane now",
        ] {
            assert!(parse_line(line), "{line:?} must be consumed");
            assert!(lookup(MOD_CTRL, '0' as i32).is_none(), "{line:?} must not store");
        }
    }
}
