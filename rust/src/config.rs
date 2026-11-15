//! Port of `src/config.c` — configuration state, rc-line parsing and
//! prompt rendering.
//!
//! Locking model (port contract): the global `Config` sits behind a
//! plain [`Mutex`]. Read hot paths take a cheap [`snapshot`] clone at
//! function entry or use a brief [`with`] callback that must not call
//! back into this module. [`apply_line`] holds the write lock only while
//! mutating, then runs side effects (history clear / limit, mux teardown)
//! after releasing it — lock order is always config → (history | bind |
//! mux), never the reverse.

use crate::{bind, history, mux};
use std::io::Write;
use std::sync::Mutex;

pub const ANIM_NONE: i32 = 0;
pub const ANIM_MATRIX: i32 = 1;
pub const ANIM_NEWCOMER: i32 = 2;
pub const ANIM_PLACEMENT: i32 = 3;
pub const ANIM_SPINNER: i32 = 4;

/// MUX_MAX_PANES from thesh.h (window-limit clamp).
pub const MUX_MAX_PANES: i32 = 128;

// ── Color presets (C's `colors[]` table) ───────────────────────────────

struct ColorSpec {
    name: &'static str,
    sgr: &'static str,
    r: u8,
    g: u8,
    b: u8,
}

const COLORS: &[ColorSpec] = &[
    ColorSpec { name: "black", sgr: "30", r: 0, g: 0, b: 0 },
    ColorSpec { name: "red", sgr: "31", r: 205, g: 49, b: 49 },
    ColorSpec { name: "green", sgr: "32", r: 13, g: 188, b: 121 },
    ColorSpec { name: "yellow", sgr: "33", r: 229, g: 229, b: 16 },
    ColorSpec { name: "blue", sgr: "34", r: 36, g: 114, b: 200 },
    ColorSpec { name: "purple", sgr: "35", r: 188, g: 63, b: 188 },
    ColorSpec { name: "cyan", sgr: "36", r: 17, g: 168, b: 205 },
    ColorSpec { name: "white", sgr: "37", r: 229, g: 229, b: 229 },
    ColorSpec { name: "grey", sgr: "90", r: 102, g: 102, b: 102 },
    ColorSpec { name: "pink", sgr: "95", r: 255, g: 105, b: 180 },
    ColorSpec { name: "brightred", sgr: "91", r: 255, g: 84, b: 84 },
    ColorSpec { name: "brightgreen", sgr: "92", r: 5, g: 221, b: 139 },
    ColorSpec { name: "brightyellow", sgr: "93", r: 255, g: 255, b: 64 },
    ColorSpec { name: "brightblue", sgr: "94", r: 90, g: 140, b: 255 },
    ColorSpec { name: "brightcyan", sgr: "96", r: 102, g: 229, b: 255 },
    ColorSpec { name: "brightwhite", sgr: "97", r: 255, g: 255, b: 255 },
    ColorSpec { name: "default", sgr: "39", r: 0, g: 0, b: 0 },
    ColorSpec { name: "none", sgr: "", r: 0, g: 0, b: 0 },
];

const DEFAULT_LOOKS: &str = "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER \
    $HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR $SPACER";

// ── Config state ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Config {
    pub looks: Option<String>, // None → PS1/default (C: NULL)
    pub rightwall: String,
    pub leftwall: String,
    pub sep: String,
    pub cursor: String,
    pub col_rightwall: Option<String>,
    pub col_leftwall: Option<String>,
    pub col_sep: Option<String>,
    pub col_host: Option<String>,
    pub col_path: Option<String>,
    pub col_cursor: Option<String>,
    pub col_guess: Option<String>,
    pub col_user: Option<String>,
    pub textcolor: Option<String>,
    pub col_mux_active: Option<String>,
    pub col_mux_inactive: Option<String>,
    pub multiplexer: bool,
    pub mux_window_limit: i32, // 0 = unlimited ("inf")
    pub mux_style: i32,        // 0 = crosshair
    pub op_rightwall: i32,     // 0..=100, -1 = unset
    pub op_leftwall: i32,
    pub op_sep: i32,
    pub op_host: i32,
    pub op_user: i32,
    pub op_path: i32,
    pub op_cursor: i32,
    pub op_guess: i32,
    pub hybrid: bool,   // typing: hybrid folds command names
    pub guesser: bool,  // ghost suggestions
    pub corrector: i32, // 0 passive, 1 active, 2 consent, 3 inactive
    pub autoreload: bool,
    pub history_limit: i32,
    pub history_enabled: bool,
    pub animation: i32,
    pub movespeed: i32,
    pub scrollspeed: i32,
    pub amplify: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            looks: None,
            rightwall: "[".into(),
            leftwall: "]".into(),
            sep: "|".into(),
            cursor: "$".into(),
            col_rightwall: None,
            col_leftwall: None,
            col_sep: None,
            col_host: None,
            col_path: None,
            col_cursor: None,
            col_guess: None,
            col_user: None,
            textcolor: None,
            col_mux_active: None,
            col_mux_inactive: None,
            multiplexer: true,
            mux_window_limit: 0,
            mux_style: 0,
            op_rightwall: -1,
            op_leftwall: -1,
            op_sep: -1,
            op_host: -1,
            op_user: -1,
            op_path: -1,
            op_cursor: -1,
            op_guess: -1,
            hybrid: true,
            guesser: true,
            corrector: 0,
            autoreload: true,
            history_limit: history::HIST_MAX,
            history_enabled: true,
            animation: ANIM_NONE,
            movespeed: 1,
            scrollspeed: 1,
            amplify: 2,
        }
    }
}

static CFG: Mutex<Config> = Mutex::new(Config {
    looks: None,
    rightwall: String::new(),
    leftwall: String::new(),
    sep: String::new(),
    cursor: String::new(),
    col_rightwall: None,
    col_leftwall: None,
    col_sep: None,
    col_host: None,
    col_path: None,
    col_cursor: None,
    col_guess: None,
    col_user: None,
    textcolor: None,
    col_mux_active: None,
    col_mux_inactive: None,
    multiplexer: false,
    mux_window_limit: 0,
    mux_style: 0,
    op_rightwall: -1,
    op_leftwall: -1,
    op_sep: -1,
    op_host: -1,
    op_user: -1,
    op_path: -1,
    op_cursor: -1,
    op_guess: -1,
    hybrid: false,
    guesser: false,
    corrector: 0,
    autoreload: false,
    history_limit: 0,
    history_enabled: true,
    animation: 0,
    movespeed: 1,
    scrollspeed: 1,
    amplify: 1,
});

fn lock() -> std::sync::MutexGuard<'static, Config> {
    CFG.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `config_init()`: reset every field to its default.
pub fn init() {
    *lock() = Config::default();
}

/// Deep copy of the current config (read hot paths: grab once at entry).
pub fn snapshot() -> Config {
    lock().clone()
}

/// Brief read access. `f` must not call back into this module (no
/// nested locks) and should be side-effect free.
pub fn with<R>(f: impl FnOnce(&Config) -> R) -> R {
    f(&lock())
}

/// Port of `config_default_looks()`.
pub fn default_looks() -> &'static str {
    DEFAULT_LOOKS
}

// ── Helpers (C static functions) ───────────────────────────────────────

/// Emulation of `strtol(v, &end, 10)`: skip ASCII whitespace, accept an
/// optional sign and digits; `None` when nothing parsed (`end == v`).
/// Overflow saturates like strtol does (LONG_MAX). Shared with exec
/// (split percentages, exit codes).
pub(crate) fn strtol10(v: &str) -> Option<i64> {
    let t = v.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let mut s = t;
    if s.starts_with('+') || s.starts_with('-') {
        s = &s[1..];
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some(digits.parse::<i64>().unwrap_or(i64::MAX))
}

/// Port of `str_bool()`: yes/on/true/1 → true; everything else (incl.
/// unknown values) → false.
fn str_bool(v: &str) -> bool {
    if v.eq_ignore_ascii_case("yes")
        || v.eq_ignore_ascii_case("on")
        || v.eq_ignore_ascii_case("true")
        || v == "1"
    {
        return true;
    }
    if v.eq_ignore_ascii_case("no")
        || v.eq_ignore_ascii_case("off")
        || v.eq_ignore_ascii_case("false")
        || v == "0"
    {
        return false;
    }
    false
}

/// Port of `parse_opacity()`: returns (supplied?, value 0..=100 | −1).
fn parse_opacity(v: &str) -> (bool, i32) {
    if v.is_empty() {
        return (true, -1);
    }
    let mut s = v;
    if s.starts_with('"') || s.starts_with('\'') {
        s = &s[1..];
    }
    let Some(n) = strtol10(s) else { return (false, -1) };
    (true, n.clamp(0, 100) as i32)
}

/// Port of `find_spec()` by SGR string.
fn find_spec(sgr: &str) -> Option<&'static ColorSpec> {
    COLORS.iter().find(|c| c.sgr == sgr)
}

/// Port of `set_color()`: resolve a *name* to its SGR; unknown names
/// warn, "none"/empty → no color.
fn color_from_name(v: &str) -> Option<String> {
    if v.is_empty() {
        return None;
    }
    for c in COLORS {
        if c.name.eq_ignore_ascii_case(v) {
            return if c.sgr.is_empty() { None } else { Some(c.sgr.to_string()) };
        }
    }
    eprintln!("thesh: unknown color: '{v}'");
    None
}

/// Port of `color_name_of()`: SGR → canonical name (reverse lookup).
fn color_name_of(sgr: Option<&str>) -> Option<&'static str> {
    let sgr = sgr?;
    COLORS.iter().find(|c| !c.sgr.is_empty() && c.sgr == sgr).map(|c| c.name)
}

/// Port of `color_sgr()`: SGR number(s) for a stored color at the given
/// opacity; empty string when there's nothing to emit (C: `buf[0] = 0`).
///
/// `default`/unset emit nothing below 100% opacity; below 100% known
/// colors blend toward black and emit a 256-color code.
pub fn color_sgr(sgr: Option<&str>, opacity: i32) -> String {
    let Some(sgr) = sgr else { return String::new() };
    if sgr.is_empty() {
        return String::new();
    }
    // C: `if (opacity < 0) opacity = 100; if (opacity > 100) …` — negative
    // means UNSET (config stores −1) → full color, not zero.
    let opacity = if opacity < 0 { 100 } else { opacity.min(100) };

    if sgr == "39" {
        return if opacity >= 100 { "39".to_string() } else { String::new() };
    }
    let Some(sp) = find_spec(sgr) else {
        return sgr.to_string(); // pass through unknown SGR strings
    };
    if opacity >= 100 {
        return sp.sgr.to_string();
    }
    let r = sp.r as i32 * opacity / 100;
    let g = sp.g as i32 * opacity / 100;
    let b = sp.b as i32 * opacity / 100;
    format!("38;5;{}", cube_index(r, g, b))
}

/// Port of `cube_index()` (config.c): nearest xterm-256 cube color.
fn cube_index(r: i32, g: i32, b: i32) -> i32 {
    let mut best = 16;
    let mut bestd = i32::MAX;
    for ri in 0..6 {
        let rv = if ri == 0 { 0 } else { 95 + (ri - 1) * 40 };
        for gi in 0..6 {
            let gv = if gi == 0 { 0 } else { 95 + (gi - 1) * 40 };
            for bi in 0..6 {
                let bv = if bi == 0 { 0 } else { 95 + (bi - 1) * 40 };
                let (dr, dg, db) = (r - rv, g - gv, b - bv);
                let d = dr * dr + dg * dg + db * db;
                if d < bestd {
                    bestd = d;
                    best = 16 + 36 * ri + 6 * gi + bi;
                }
            }
        }
    }
    best
}

// ── Option application ─────────────────────────────────────────────────

/// Side effects C performs inline in `config_set_option`; collected and
/// run after the config lock is released (lock-order rule).
enum Eff {
    HistClear,
    HistLimit(i32),
    MuxLeave,
}

/// Port of `config_set_option()`: mutate `cfg`, return (consumed, effect).
fn set_option(cfg: &mut Config, key: &str, val: &str) -> (bool, Option<Eff>) {
    let v = val;
    match key {
        "looks" | "layout" => {
            cfg.looks = Some(v.to_string());
            return (true, None);
        }
        "rightwallstyle" => { cfg.rightwall = v.into(); return (true, None); }
        "leftwallstyle" => { cfg.leftwall = v.into(); return (true, None); }
        "seperatorstyle" => { cfg.sep = v.into(); return (true, None); }
        "cursorstyle" => { cfg.cursor = v.into(); return (true, None); }
        "rightwall-color" => { cfg.col_rightwall = color_from_name(v); return (true, None); }
        "leftwall-color" => { cfg.col_leftwall = color_from_name(v); return (true, None); }
        "seperator-color" => { cfg.col_sep = color_from_name(v); return (true, None); }
        "hostname-color" => { cfg.col_host = color_from_name(v); return (true, None); }
        "path-color" => { cfg.col_path = color_from_name(v); return (true, None); }
        "cursor-color" => { cfg.col_cursor = color_from_name(v); return (true, None); }
        "guesser-color" => { cfg.col_guess = color_from_name(v); return (true, None); }
        "user-color" => { cfg.col_user = color_from_name(v); return (true, None); }
        "multiplexer-active-color" => { cfg.col_mux_active = color_from_name(v); return (true, None); }
        "multiplexer-inactive-color" => { cfg.col_mux_inactive = color_from_name(v); return (true, None); }
        "textcolor" => { cfg.textcolor = color_from_name(v); return (true, None); }
        "rightwall-opacity" => { cfg.op_rightwall = parse_opacity(v).1; return (true, None); }
        "leftwall-opacity" => { cfg.op_leftwall = parse_opacity(v).1; return (true, None); }
        "seperator-opacity" => { cfg.op_sep = parse_opacity(v).1; return (true, None); }
        "hostname-opacity" => { cfg.op_host = parse_opacity(v).1; return (true, None); }
        "user-opacity" => { cfg.op_user = parse_opacity(v).1; return (true, None); }
        "path-opacity" => { cfg.op_path = parse_opacity(v).1; return (true, None); }
        "cursor-opacity" => { cfg.op_cursor = parse_opacity(v).1; return (true, None); }
        "guesser-opacity" => { cfg.op_guess = parse_opacity(v).1; return (true, None); }
        "typing" => {
            cfg.hybrid = v.eq_ignore_ascii_case("hybrid");
            return (true, None);
        }
        "guesser" => { cfg.guesser = str_bool(v); return (true, None); }
        "corrector" => {
            cfg.corrector = if v.eq_ignore_ascii_case("active") {
                1
            } else if v.eq_ignore_ascii_case("consent") {
                2
            } else if v.eq_ignore_ascii_case("inactive") {
                3
            } else {
                0 // passive and any unknown value
            };
            return (true, None);
        }
        "autoreload" => { cfg.autoreload = str_bool(v); return (true, None); }
        "history" => {
            cfg.history_enabled = str_bool(v);
            let eff = if !cfg.history_enabled { Some(Eff::HistClear) } else { None };
            return (true, eff);
        }
        "historylimit" => {
            // C quirk: an *empty* value yields n = 0 (clears the limit!),
            // an unparsable non-empty one keeps the current value.
            let mut n: i64 = 0;
            if !v.is_empty() {
                n = strtol10(v).unwrap_or(cfg.history_limit as i64);
            }
            n = n.clamp(0, 1_000_000);
            cfg.history_limit = n as i32;
            return (true, Some(Eff::HistLimit(n as i32)));
        }
        "animation" => {
            cfg.animation = if v.is_empty() || v.eq_ignore_ascii_case("none") {
                ANIM_NONE
            } else if v.eq_ignore_ascii_case("matrix") {
                ANIM_MATRIX
            } else if v.eq_ignore_ascii_case("newcomer") {
                ANIM_NEWCOMER
            } else if v.eq_ignore_ascii_case("placement") {
                ANIM_PLACEMENT
            } else if v.eq_ignore_ascii_case("spinner") {
                ANIM_SPINNER
            } else {
                ANIM_NONE
            };
            return (true, None);
        }
        "movespeed" | "scrollspeed" | "amplify" => {
            let mut n: i64 = 0;
            let mut ok = false;
            if !v.is_empty() {
                if v.eq_ignore_ascii_case("yes") {
                    n = 2;
                    ok = true;
                } else if v.eq_ignore_ascii_case("no") {
                    n = 1;
                    ok = true;
                } else if let Some(parsed) = strtol10(v) {
                    n = parsed;
                    ok = true;
                }
            }
            if ok {
                n = n.clamp(1, 100);
                match key {
                    "movespeed" => cfg.movespeed = n as i32,
                    "scrollspeed" => cfg.scrollspeed = n as i32,
                    _ => cfg.amplify = n as i32,
                }
            }
            return (true, None);
        }
        "multiplexer" => {
            cfg.multiplexer = str_bool(v);
            let eff = if !cfg.multiplexer { Some(Eff::MuxLeave) } else { None };
            return (true, eff);
        }
        "multiplexer-window-limit" => {
            if !v.is_empty() {
                if v.eq_ignore_ascii_case("inf") || v.eq_ignore_ascii_case("infinite") {
                    cfg.mux_window_limit = 0;
                } else if let Some(n) = strtol10(v) {
                    cfg.mux_window_limit = n.clamp(1, MUX_MAX_PANES as i64) as i32;
                }
            }
            return (true, None);
        }
        "multiplexer-style" => {
            if v.is_empty() || v.eq_ignore_ascii_case("crosshair") {
                cfg.mux_style = 0;
            } else {
                eprintln!("thesh: unknown multiplexer style: '{v}' (using 'crosshair')");
            }
            return (true, None);
        }
        _ => (false, None),
    }
}

/// The known option names (C's `known_keys[]`).
const KNOWN_KEYS: &[&str] = &[
    "looks", "layout", "rightwallstyle", "leftwallstyle", "seperatorstyle",
    "cursorstyle", "rightwall-color", "leftwall-color", "seperator-color",
    "hostname-color", "path-color", "cursor-color", "guesser-color",
    "user-color", "textcolor",
    "rightwall-opacity", "leftwall-opacity", "seperator-opacity",
    "hostname-opacity", "user-opacity", "path-opacity", "cursor-opacity",
    "guesser-opacity",
    "typing", "guesser", "corrector", "autoreload",
    "history", "historylimit", "animation",
    "movespeed", "scrollspeed", "amplify",
    "multiplexer-active-color", "multiplexer-inactive-color",
    "multiplexer", "multiplexer-window-limit", "multiplexer-style",
];

/// Port of `config_apply_line()`: consume one config directive.
/// Returns true when the line was a known option or a key binding
/// (binds are tried first, exactly like C).
pub fn apply_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'#' {
        return false;
    }

    if bind::parse_line(line) {
        return true; // user key binding
    }

    let kstart = i;
    while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'='
        && bytes[i] != b':'
    {
        i += 1;
    }
    let klen = i - kstart;
    if klen == 0 {
        return false;
    }
    // C copies at most 63 key bytes (key[64]); truncation can never make
    // a too-long key match a known one (longest known key < 63), so no
    // replication needed — documented only.
    let key = &line[kstart..kstart + klen];

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
        return false;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    if !KNOWN_KEYS.contains(&key) {
        return false;
    }

    // trailing whitespace is not part of the value — `'inf' ` must parse
    // as `inf`, not as a missing-close quote (C: isspace trim)
    let rest = &line[i..];
    let vlen = rest.trim_end_matches(|c: char| c.is_ascii_whitespace()).len();
    let p = &rest[..vlen];
    let val: String = if vlen >= 2
        && (p.starts_with('"') || p.starts_with('\''))
        && p.as_bytes()[vlen - 1] == p.as_bytes()[0]
    {
        p[1..vlen - 1].to_string()
    } else if p.starts_with('"') || p.starts_with('\'') {
        // tolerate a missing closing quote
        p[1..].to_string()
    } else {
        p.to_string()
    };

    let mut cfg = lock();
    let (consumed, eff) = set_option(&mut cfg, key, &val);
    drop(cfg); // release before side effects (lock-order rule)

    match eff {
        Some(Eff::HistClear) => {
            history::set_enabled(false); // C: Cfg first…
            history::clear();            // …then hist_clear()
        }
        Some(Eff::HistLimit(n)) => history::set_limit(n),
        Some(Eff::MuxLeave) => mux::leave(),
        None => {}
    }
    consumed
}

// ── Prompt rendering ───────────────────────────────────────────────────

/// Port of `pcat_part()`: append `s` wrapped in `\033[<sgr>m` when sgr
/// is non-empty; tracks whether any color was emitted.
fn push_part(out: &mut String, s: &str, sgr: &str, colored: &mut bool) {
    if s.is_empty() {
        return;
    }
    if !sgr.is_empty() {
        out.push_str("\x1b[");
        out.push_str(sgr);
        out.push('m');
        *colored = true;
    }
    out.push_str(s);
}

/// Port of `render_looks_str()`.
///
/// Deviation: C accumulates literal runs in a fixed 4096-byte buffer and
/// truncates longer runs (possibly mid-UTF-8); Rust keeps them whole.
/// Every realistic prompt is far below that limit.
pub fn render_looks_str(tpl: &str, user: &str, host: &str, dir: &str) -> String {
    let cfg = snapshot();
    let mut out = String::new();
    let mut colored = false;
    let mut lit = String::new();
    let mut chars = tpl.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            lit.push(c);
            continue;
        }
        // collect the token chars exactly like C's bounded loop
        let mut ti = 0usize;
        let mut tok = String::new();
        while let Some(&qc) = chars.peek() {
            if qc.is_ascii_alphanumeric() || qc == '_' {
                if ti < 31 {
                    tok.push(qc);
                    ti += 1;
                }
                chars.next();
            } else {
                break;
            }
        }
        if ti == 0 {
            // lone '$'
            lit.push('$');
            continue;
        }

        // flush the preceding literal (trailing ASCII ws trimmed) — C does
        // this for EVERY token, known or unknown (config.c:410-411)
        const ASCII_WS: [char; 6] = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];
        let flushed = lit.trim_end_matches(ASCII_WS);
        if !flushed.is_empty() {
            let sgr = color_sgr(cfg.textcolor.as_deref(), 100);
            push_part(&mut out, flushed, &sgr, &mut colored);
        }
        lit.clear();

        if tok == "SPACER" {
            out.push(' ');
            continue;
        }

        let (txt, csgr, eop): (&str, Option<&str>, i32) = match tok.as_str() {
            "USER" => (user, cfg.col_user.as_deref(), cfg.op_user),
            "HOSTNAME" => (host, cfg.col_host.as_deref(), cfg.op_host),
            "SEPERATOR" => (&cfg.sep, cfg.col_sep.as_deref(), cfg.op_sep),
            "RIGHTWALL" => (&cfg.rightwall, cfg.col_rightwall.as_deref(), cfg.op_rightwall),
            "LEFTWALL" => (&cfg.leftwall, cfg.col_leftwall.as_deref(), cfg.op_leftwall),
            "PATH" | "DIR" | "PWD" => (dir, cfg.col_path.as_deref(), cfg.op_path),
            "CURSOR" => (&cfg.cursor, cfg.col_cursor.as_deref(), cfg.op_cursor),
            _ => {
                // unknown token → literal `$` + name, stays in lit
                lit.push('$');
                lit.push_str(&tok);
                continue;
            }
        };
        let csgr = csgr.or(cfg.textcolor.as_deref()); // fallback color
        let sgr = color_sgr(csgr, eop);
        push_part(&mut out, txt, &sgr, &mut colored);
    }

    // flush the tail literal (trailing ASCII ws trimmed)
    const ASCII_WS: [char; 6] = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];
    let tail = lit.trim_end_matches(ASCII_WS);
    if !tail.is_empty() {
        let sgr = color_sgr(cfg.textcolor.as_deref(), 100);
        push_part(&mut out, tail, &sgr, &mut colored);
    }
    if colored {
        out.push_str("\x1b[0m");
    }
    out
}

// ── $PS1 expansion ─────────────────────────────────────────────────────

const PS1_T_HMS: i32 = 0;
const PS1_T_HM: i32 = 1;
const PS1_T_12H: i32 = 2;
const PS1_T_DATE: i32 = 3;

/// Port of `ps1_time()` via strftime(3) (locale-dependent like C).
fn ps1_time(which: i32) -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let fmt: &[u8] = match which {
            PS1_T_HMS => b"%H:%M:%S\0",
            PS1_T_HM => b"%H:%M\0",
            PS1_T_12H => b"%I:%M %p\0",
            _ => b"%a %b %d\0",
        };
        let mut b = [0u8; 64];
        let n = libc::strftime(b.as_mut_ptr().cast(), 64, fmt.as_ptr().cast(), &tm);
        if n > 0 && n < 64 {
            String::from_utf8_lossy(&b[..n]).into_owned()
        } else {
            String::new()
        }
    }
}

/// Port of `render_ps1()` (bash-style subset).
pub fn render_ps1(ps1: &str, user: &str, host: &str, dir: &str) -> String {
    let mut out = String::new();
    let mut it = ps1.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek().is_some() {
            let c2 = it.next().unwrap();
            match c2 {
                'u' => out.push_str(user),
                'h' | 'H' => out.push_str(host),
                'w' => out.push_str(dir),
                'W' => {
                    // C: strrchr(dir, '/') → basename ("" for "/")
                    out.push_str(dir.rsplit_once('/').map(|(_, b)| b).unwrap_or(dir));
                }
                '$' => out.push(if unsafe { libc::getuid() } == 0 { '#' } else { '$' }),
                's' => out.push_str("thesh"),
                'v' => out.push_str(env!("CARGO_PKG_VERSION")),
                'n' => out.push('\n'),
                'e' => out.push('\x1b'),
                '\\' => out.push('\\'),
                '[' | ']' => {} // drop markers
                't' => out.push_str(&ps1_time(PS1_T_HMS)),
                'A' => out.push_str(&ps1_time(PS1_T_HM)),
                '@' => out.push_str(&ps1_time(PS1_T_12H)),
                'd' => out.push_str(&ps1_time(PS1_T_DATE)),
                '!' | '#' => out.push_str(&crate::history::count().saturating_add(1).to_string()),
                'j' => out.push('0'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── Preset export ──────────────────────────────────────────────────────

/// Port of `fqval()`: quote a value so `config_apply_line` re-parses it.
fn fqval(f: &mut dyn Write, v: &str) -> std::io::Result<()> {
    if !v.contains('\'') {
        write!(f, "'{v}'")
    } else {
        write!(f, "\"{v}\"")
    }
}

/// Port of `config_dump_current()`: serialize the effective config as a
/// theshrc-style preset. Aliases and key bindings are appended by the
/// callers (exec / bind), exactly like C.
pub fn dump_current(f: &mut dyn Write) -> std::io::Result<()> {
    let cfg = snapshot();

    let cols: [(&str, &Option<String>); 11] = [
        ("rightwall-color", &cfg.col_rightwall),
        ("leftwall-color", &cfg.col_leftwall),
        ("seperator-color", &cfg.col_sep),
        ("hostname-color", &cfg.col_host),
        ("user-color", &cfg.col_user),
        ("path-color", &cfg.col_path),
        ("cursor-color", &cfg.col_cursor),
        ("guesser-color", &cfg.col_guess),
        ("textcolor", &cfg.textcolor),
        ("multiplexer-active-color", &cfg.col_mux_active),
        ("multiplexer-inactive-color", &cfg.col_mux_inactive),
    ];
    let ops: [(&str, i32); 8] = [
        ("rightwall-opacity", cfg.op_rightwall),
        ("leftwall-opacity", cfg.op_leftwall),
        ("seperator-opacity", cfg.op_sep),
        ("hostname-opacity", cfg.op_host),
        ("user-opacity", cfg.op_user),
        ("path-opacity", cfg.op_path),
        ("cursor-opacity", cfg.op_cursor),
        ("guesser-opacity", cfg.op_guess),
    ];

    writeln!(f, "# thesh preset")?;
    let styles: [(&str, &Option<String>); 5] = [
        ("layout", &cfg.looks),
        ("rightwallstyle", &Some(cfg.rightwall.clone())),
        ("leftwallstyle", &Some(cfg.leftwall.clone())),
        ("seperatorstyle", &Some(cfg.sep.clone())),
        ("cursorstyle", &Some(cfg.cursor.clone())),
    ];
    for (key, val) in styles {
        if let Some(v) = val {
            if !v.is_empty() {
                write!(f, "{key} = ")?;
                fqval(f, v)?;
                writeln!(f)?;
            }
        }
    }

    for (key, slot) in cols {
        let Some(sgr) = slot else { continue };
        if sgr.is_empty() {
            continue;
        }
        let Some(nm) = color_name_of(Some(sgr)) else { continue };
        write!(f, "{key} = ")?;
        fqval(f, nm)?;
        writeln!(f)?;
    }
    for (key, v) in ops {
        if v < 0 {
            continue;
        }
        writeln!(f, "{key} = '{v}%'")?;
    }

    writeln!(f, "typing = '{}'", if cfg.hybrid { "hybrid" } else { "lowercase" })?;
    writeln!(f, "guesser = '{}'", if cfg.guesser { "yes" } else { "no" })?;
    let cor = ["passive", "active", "consent", "inactive"];
    let ci = if (0..4).contains(&cfg.corrector) { cfg.corrector as usize } else { 0 };
    writeln!(f, "corrector = '{}'", cor[ci])?;
    writeln!(f, "autoreload = '{}'", if cfg.autoreload { "yes" } else { "no" })?;
    writeln!(
        f,
        "history = '{}'",
        if cfg.history_enabled { "yes" } else { "no" }
    )?;
    writeln!(f, "historylimit = '{}'", cfg.history_limit)?;
    let anims = ["none", "matrix", "newcomer", "placement", "spinner"];
    let ai = if (0..5).contains(&cfg.animation) { cfg.animation as usize } else { 0 };
    writeln!(f, "animation = '{}'", anims[ai])?;
    writeln!(f, "movespeed = '{}'", cfg.movespeed)?;
    writeln!(f, "scrollspeed = '{}'", cfg.scrollspeed)?;
    writeln!(f, "amplify = '{}'", cfg.amplify)?;
    writeln!(f, "multiplexer = '{}'", if cfg.multiplexer { "yes" } else { "no" })?;
    if cfg.mux_window_limit != 0 {
        writeln!(f, "multiplexer-window-limit = '{}'", cfg.mux_window_limit)?;
    } else {
        writeln!(f, "multiplexer-window-limit = 'inf'")?;
    }
    writeln!(f, "multiplexer-style = 'crosshair'")?;
    Ok(())
}
