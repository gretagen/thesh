//! Port of `src/editor.c` — the interactive line editor (raw keys,
//! drawing, history search, completion, ghost suggestions, animations,
//! bind dispatch).
//!
//! State mirrors the C file's statics inside one process-global
//! [`EdState`] behind a mutex. Lock rules: never hold the ED lock while
//! blocking on input, calling `exec_line`, or running a bind — every
//! helper takes the lock for its own body only (single-threaded
//! equivalence with the C sequential flow).
//!
//! Deviations: surrogate/out-of-range codepoints in the buffer render
//! byte-exactly while drawing (raw `utf8::encode`), but surface as
//! U+FFFD when text is handed to exec/history as `str`; animation glyphs
//! use a private PRNG instead of glibc `rand()` (cosmetic frames only);
//! completion results are not truncated at C's 4096/512-byte buffers.

use crate::bind::{self, Bind, BindKind};
use crate::keys::*;
use crate::{config, dict, exec, history, mux, term, utf8, util};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// LINE_MAX_CP from thesh.h — editing cap in *codepoints*.
const LINE_MAX_CP: usize = 4096;
const ANIM_FRAME_MS: u64 = 40;
const ANIM_FRAMES: i32 = 10;
const AMPLIFY_HOLD_MS: i64 = 400;
const NAME: &str = "thesh";

// ── Editing buffer (array of codepoints) ───────────────────────────────

#[derive(Default, Clone)]
struct Buf {
    v: Vec<u32>,
    cur: usize,
}

impl Buf {
    fn new() -> Self {
        Buf::default()
    }

    /// Port of `bins()` — insert one codepoint (capped at LINE_MAX_CP).
    fn insert(&mut self, pos: usize, cp: u32) {
        if self.v.len() >= LINE_MAX_CP {
            return;
        }
        self.v.insert(pos.min(self.v.len()), cp);
    }

    /// Port of `bdel()`.
    fn del(&mut self, pos: usize) {
        if pos < self.v.len() {
            self.v.remove(pos);
        }
    }

    /// Port of `bclr()`.
    fn clear(&mut self) {
        self.v.clear();
        self.cur = 0;
    }

    /// Port of `bset_str()` — decode UTF-8 into codepoints (capped).
    fn set_str(&mut self, s: &str) {
        self.clear();
        let mut rest = s;
        while !rest.is_empty() {
            let (cp, n, _) = utf8::decode(rest.as_bytes());
            if self.v.len() < LINE_MAX_CP {
                self.v.push(cp);
            }
            if n == 0 {
                break;
            }
            rest = &rest[n.min(rest.len())..];
        }
        self.cur = self.v.len();
    }

    /// Port of `btext()` — raw byte encoding (surrogates kept as C keeps
    /// them, via the non-validating encoder).
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.v.len() * 2 + 8);
        let mut tmp = [0u8; 8];
        for &cp in &self.v {
            let n = utf8::encode(cp, &mut tmp);
            out.extend_from_slice(&tmp[..n]);
        }
        out
    }

    /// Lossy `str` view for lookups/exec (documented deviation for
    /// pathological codepoints; identical for everything typed).
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    /// Port of `bcells()` — cell width of a range.
    fn cells(&self, from: usize, to: usize) -> i32 {
        let mut c = 0;
        for i in from..to.min(self.v.len()) {
            c += utf8::cp_width(self.v[i]);
        }
        c
    }

    /// Port of `bdump()` — emit raw bytes of a range.
    fn dump(&self, from: usize, to: usize) {
        let mut tmp = [0u8; 8];
        for i in from..to.min(self.v.len()) {
            let n = utf8::encode(self.v[i], &mut tmp);
            util::outn(&tmp[..n]);
        }
    }
}

fn is_word(c: u32) -> bool {
    // C: isalnum((int)c) || c == '_' — for c > 255 C reads past the
    // ctype table (UB); Rust uses Unicode alnum (DEVIATION, sane result).
    char::from_u32(c).map_or(false, |ch| ch.is_alphanumeric()) || c == b'_' as u32
}

// ── Editor state (C's statics) ─────────────────────────────────────────

struct EdState {
    e: Buf,
    gh: Buf,
    prompt: String,
    histq: Option<String>,
    browse: i32, // -1 = not browsing
    bound: Option<String>,
    rq: String, // reverse-search query (byte cap 512 like C)
    rmatch: i32,
    rsearch_on: bool,
    rbase: Option<String>,
    // wrap-aware drawing anchor (mux pane rows)
    ed_anchor: i32,
    ed_anchor_col: i32,
    ed_off: i32,
    ed_span: i32,
    ed_w: i32,
    ed_h: i32,
    ed_drew: bool,
    last_arrow: [Option<Instant>; 4],
    clip: Option<String>,
    key_raw: Vec<u8>,
    pushback: VecDeque<u8>,
    mouse_fwd: bool,
    probe_ok: bool, // cursor_probe stickiness
    probe_misses: i32,
}

impl EdState {
    const fn new() -> Self {
        EdState {
            e: Buf { v: Vec::new(), cur: 0 },
            gh: Buf { v: Vec::new(), cur: 0 },
            prompt: String::new(),
            histq: None,
            browse: -1,
            bound: None,
            rq: String::new(),
            rmatch: -1,
            rsearch_on: false,
            rbase: None,
            ed_anchor: -1,
            ed_anchor_col: 0,
            ed_off: 0,
            ed_span: 1,
            ed_w: 80,
            ed_h: 24,
            ed_drew: false,
            last_arrow: [None; 4],
            clip: None,
            key_raw: Vec::new(),
            pushback: VecDeque::new(),
            mouse_fwd: false,
            probe_ok: true,
            probe_misses: 0,
        }
    }

    /// Port of `hist_reset()`.
    fn hist_reset(&mut self) {
        self.histq = None;
        self.bound = None;
        self.browse = -1;
    }
}

static ED: Mutex<EdState> = Mutex::new(EdState::new());

fn lock() -> std::sync::MutexGuard<'static, EdState> {
    ED.lock().unwrap_or_else(|p| p.into_inner())
}

fn with<R>(f: impl FnOnce(&EdState) -> R) -> R {
    f(&lock())
}

fn with_mut<R>(f: impl FnOnce(&mut EdState) -> R) -> R {
    f(&mut lock())
}

// ── Key reading ────────────────────────────────────────────────────────

/// Port of `ed_pushback(b, n)` — cap 128, overflow dropped.
pub fn pushback(b: &[u8]) {
    with_mut(|st| {
        let room = 128usize.saturating_sub(st.pushback.len());
        for &byte in b.iter().take(room) {
            st.pushback.push_back(byte);
        }
    });
}

fn pop_pushback() -> Option<u8> {
    with_mut(|st| st.pushback.pop_front())
}

fn pushback_pending() -> bool {
    with(|st| !st.pushback.is_empty())
}

fn check_resize() {
    mux::on_cont(); // C: thesh_on_cont()
    if !run_winched() {
        return;
    }
    if mux::active() {
        mux::pump(); // router: relayout on winch
    }
    if !mux::active() {
        ed_draw();
    }
}

fn run_winched() -> bool {
    crate::run::WINCHED.swap(false, Ordering::SeqCst)
}

use std::sync::atomic::Ordering;

/// Port of `read_byte()`: pushback first, then poll — 16 ms tick while
/// the mux pumps, infinite otherwise.
fn read_byte() -> i32 {
    loop {
        check_resize();
        if let Some(b) = pop_pushback() {
            return b as i32;
        }
        let timeout = if mux::active() { 16 } else { -1 };
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r == 0 {
            mux::pump(); // idle tick: repaint panes
            continue;
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return -1;
        }
        let mut c = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, c.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return c[0] as i32;
        }
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return -1; // EOF or error
    }
}

/// Port of `mod_bits()` — decode an xterm modifier number.
fn mod_bits(m: i32) -> i32 {
    if m <= 1 {
        return 0;
    }
    let m = m - 1;
    let mut bits = 0;
    if m & 1 != 0 {
        bits |= MOD_SHIFT;
    }
    if m & 2 != 0 {
        bits |= MOD_ALT;
    }
    if m & 4 != 0 {
        bits |= MOD_CTRL;
    }
    if m & 8 != 0 {
        bits |= MOD_SUPER; // meta
    }
    if m & 32 != 0 {
        bits |= MOD_SUPER; // super
    }
    bits
}

/// Result of parsing one input sequence.
struct SeqOut {
    key: i32,
    cp: u32,
    /// mouse event dispatch result for `ed_mouse_forward`
    mouse_fwd: Option<bool>,
}

/// Port of `parse_seq()`. The SGR mouse branch dispatches through
/// `mux::mouse` (C: mux_mouse) and reports the forward flag back so the
/// caller can store it — no ED lock is held here.
fn parse_seq(s: &[u8], mods: &mut i32) -> SeqOut {
    let n = s.len();
    if n == 1 {
        return SeqOut { key: K_NONE, cp: 27, mouse_fwd: None }; // bare ESC
    }

    // meta-prefixed CSI (ESC ESC [ x)
    if n >= 3 && s[0] == 27 && s[1] == 27 && (s[2] == b'[' || s[2] == b'O') {
        *mods |= MOD_ALT;
        return parse_seq(&s[1..], mods);
    }

    if n == 2 && s[0] == 27 {
        // Alt+X
        if s[1] == b'b' {
            *mods |= MOD_ALT;
            return SeqOut { key: K_WLEFT, cp: 0, mouse_fwd: None };
        }
        if s[1] == b'f' {
            *mods |= MOD_ALT;
            return SeqOut { key: K_WRIGHT, cp: 0, mouse_fwd: None };
        }
        if s[1] < 0x20 {
            *mods |= MOD_CTRL | MOD_ALT;
            return SeqOut { key: K_NONE, cp: s[1] as u32, mouse_fwd: None };
        }
        if s[1] == 0x7f {
            *mods |= MOD_ALT;
            return SeqOut { key: K_NONE, cp: 0x7f, mouse_fwd: None };
        }
        *mods |= MOD_ALT;
        return SeqOut { key: K_NONE, cp: s[1] as u32, mouse_fwd: None };
    }

    // SGR mouse event — dispatched centrally, never inserted
    if n > 2 && s[1] == b'[' && s[2] == b'<' {
        let mut fwd = false;
        if n > 4 && (s[n - 1] == b'M' || s[n - 1] == b'm') {
            let mut v = [0i32; 3];
            let mut vi = 0usize;
            for &c in &s[3..n - 1] {
                if vi < 3 {
                    if c.is_ascii_digit() {
                        v[vi] = v[vi] * 10 + (c - b'0') as i32;
                    } else if c == b';' && vi < 2 {
                        vi += 1;
                    }
                }
            }
            fwd = mux::mouse(v[0], v[1], v[2], if s[n - 1] == b'm' { 1 } else { 0 }) != 0;
        }
        return SeqOut { key: K_NONE, cp: 0, mouse_fwd: Some(fwd) };
    }

    // a DSR cursor reply answers our own probe — never typed input
    if n > 4 && s[1] == b'[' && s[n - 1] == b'R' {
        let mut semi = 0;
        let mut ok = true;
        for &c in &s[2..n - 1] {
            if c == b';' {
                semi += 1;
                if semi > 1 {
                    ok = false;
                    break;
                }
            } else if !c.is_ascii_digit() {
                ok = false;
                break;
            }
        }
        if ok && semi == 1 {
            return SeqOut { key: K_NONE, cp: 0, mouse_fwd: None };
        }
    }

    if n >= 2 && (s[1] == b'[' || s[1] == b'O') {
        // CSI / SS3
        let mut num: i32 = 0;
        let mut mod_ = 1;
        let mut i = 2;
        while i < n && s[i].is_ascii_digit() {
            num = num * 10 + (s[i] - b'0') as i32;
            i += 1;
        }
        if num == 0 {
            num = 1; // parameter absent → 1
        }
        if i < n && s[i] == b';' {
            i += 1;
        }
        if i < n && s[i].is_ascii_digit() {
            let start = i;
            while i < n && s[i].is_ascii_digit() {
                i += 1;
            }
            mod_ = 0;
            for k in start..i {
                mod_ = mod_ * 10 + (s[k] - b'0') as i32;
            }
        }
        let mut fin = *s.get(i).unwrap_or(&0);
        if n > 2 && s[1] == b'O' {
            fin = s[n - 1];
        }

        let key = match fin {
            b'A' => K_UP,
            b'B' => K_DOWN,
            b'C' => {
                if mod_ == 3 || mod_ == 5 {
                    K_WRIGHT
                } else {
                    K_RIGHT
                }
            }
            b'D' => {
                if mod_ == 3 || mod_ == 5 {
                    K_WLEFT
                } else {
                    K_LEFT
                }
            }
            b'P' | b'Q' | b'R' | b'S' => K_F1 + (fin - b'P') as i32, // F1-F4
            b'H' => K_HOME,
            b'F' => K_END,
            b'M' => K_ENTER,
            b'Z' => {
                *mods |= MOD_SHIFT | mod_bits(mod_);
                return SeqOut { key: K_TAB, cp: 0, mouse_fwd: None };
            }
            b'~' => {
                *mods |= mod_bits(mod_);
                let k = match num {
                    1 | 7 => K_HOME,
                    4 | 8 => K_END,
                    3 => K_FDEL,
                    5 => K_PGUP,
                    6 => K_PGDN,
                    11 => K_F1,
                    12 => K_F2,
                    13 => K_F3,
                    14 => K_F4,
                    15 => K_F5,
                    17 => K_F6,
                    18 => K_F7,
                    19 => K_F8,
                    20 => K_F9,
                    21 => K_F10,
                    23 => K_F11,
                    24 => K_F12,
                    25 => K_F13,
                    26 => K_F14,
                    28 => K_F15,
                    29 => K_F16,
                    31 => K_F17,
                    32 => K_F18,
                    33 => K_F19,
                    34 => K_F20,
                    42 => K_F21,
                    43 => K_F22,
                    44 => K_F23,
                    45 => K_F24,
                    _ => 0,
                };
                if k == 0 {
                    return SeqOut { key: K_NONE, cp: s[n - 1] as u32, mouse_fwd: None };
                }
                return SeqOut { key: k, cp: 0, mouse_fwd: None };
            }
            _ => 0,
        };
        if key != 0 {
            *mods |= mod_bits(mod_);
            return SeqOut { key, cp: 0, mouse_fwd: None };
        }
    }
    SeqOut { key: K_NONE, cp: s[n - 1] as u32, mouse_fwd: None }
}

/// Port of `seq_complete()`.
fn seq_complete(s: &[u8]) -> bool {
    let n = s.len();
    if n < 2 {
        return false;
    }
    if s[1] == b'[' {
        return s[2..].iter().any(|&c| (0x40..=0x7e).contains(&c));
    }
    if s[1] == b'O' {
        return n >= 3;
    }
    if s[1] == 27 {
        // ESC ESC … (meta prefix)
        if n >= 3 && s[2] == b'[' {
            return s[3..].iter().any(|&c| (0x40..=0x7e).contains(&c));
        }
        if n >= 3 && s[2] == b'O' {
            return n >= 4;
        }
        return false; // keep reading
    }
    true // Alt+X at 2 bytes
}

/// Port of `ed_raw()` — raw bytes of the last key read.
pub fn raw() -> Vec<u8> {
    with(|st| st.key_raw.clone())
}

/// Port of `ed_read_key()`.
pub fn read_key(cp: &mut u32, mods: &mut i32) -> i32 {
    *mods = 0;
    let c = read_byte();
    if c < 0 {
        return K_EOF;
    }
    let mut key_raw: Vec<u8> = vec![c as u8];

    if c == 27 {
        let mut seq: Vec<u8> = vec![27];
        while seq.len() < 31 {
            // A DSR reply can sit in the pushback — drain it once the
            // sequence started; a plain pushback of keystrokes with only
            // ESC so far still leaves ESC bare.
            if pushback_pending() {
                if seq.len() > 1 {
                    let b = read_byte();
                    if b < 0 {
                        break;
                    }
                    seq.push(b as u8);
                    if seq_complete(&seq) {
                        break; // one event per call
                    }
                    continue;
                }
                let nxt = with(|st| st.pushback.front().copied().unwrap_or(0));
                if nxt != b'[' && nxt != b'O' && nxt != 27 {
                    break;
                }
            }
            // poll 50 ms: silence decides bare ESC
            let mut pfd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, 50) };
            if r <= 0 {
                break;
            }
            let b = read_byte();
            if b < 0 {
                break;
            }
            seq.push(b as u8);
            if seq_complete(&seq) {
                break;
            }
        }
        key_raw = seq.clone();
        let mut fwd = None;
        let out = {
            let mut m = *mods;
            let r = parse_seq(&seq, &mut m);
            *mods = m;
            r
        };
        if let Some(f) = out.mouse_fwd {
            fwd = Some(f);
        }
        with_mut(|st| {
            st.key_raw = key_raw;
            if let Some(f) = fwd {
                st.mouse_fwd = f;
            }
        });
        // all callers zero `cp` per call; parse branches that don't set
        // it in C leave it 0 — out.cp mirrors exactly that
        *cp = out.cp;
        return out.key;
    }

    if (0..0x20).contains(&c) {
        *mods = MOD_CTRL; // raw control byte
    }

    // Plain (possibly UTF-8) character
    let mut rawb: Vec<u8> = vec![c as u8];
    let need = if c & 0xE0 == 0xC0 {
        1
    } else if c & 0xF0 == 0xE0 {
        2
    } else if c & 0xF8 == 0xF0 {
        3
    } else {
        0
    };
    if need > 0 {
        for _ in 1..=need {
            let mut pfd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, 10) };
            if r <= 0 {
                break;
            }
            let b = read_byte();
            if b < 0 {
                break;
            }
            rawb.push(b as u8);
        }
    }

    let (decoded, _len, ok) = utf8::decode(&rawb);
    if ok {
        *cp = decoded;
        with_mut(|st| st.key_raw = rawb);
        return K_NONE;
    }
    *cp = c as u32;
    with_mut(|st| st.key_raw = rawb);
    K_NONE
}

// ── Tiny prompt line editor (presets / ask()) ──────────────────────────

/// Port of `prompt_line(buf, sz, prompt)` → `Option<String>`
/// (None = cancel/EOF ⇔ C's −1, Some = Enter ⇔ C's 0). Content is
/// uncapped here; see [`prompt_line_cap`] for C's buffer-size limit.
pub fn prompt_line(prompt: &str) -> Option<String> {
    prompt_line_cap(prompt, LINE_MAX_CP)
}

/// C's `prompt_line` with an explicit caller buffer size (bytes, NUL
/// room excluded): typed content stops at `cap - 1` bytes like `sz`.
pub fn prompt_line_cap(prompt: &str, cap: usize) -> Option<String> {
    let was_raw = term::raw_active();

    if term::enter_raw().is_err() {
        // not interactive — plain fgets fallback
        if !prompt.is_empty() {
            util::out(prompt);
        }
        return fgets_line(cap);
    }

    if !prompt.is_empty() {
        util::out(prompt);
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut r: Option<String> = None;

    'outer: loop {
        let mut cp = 0u32;
        let mut mods = 0;
        let key = read_key(&mut cp, &mut mods);
        if key == K_EOF {
            break;
        }

        if key == K_NONE && cp < 0x20 {
            match cp {
                0x0d | 0x0a => {
                    r = Some(String::from_utf8_lossy(&buf).into_owned());
                    break 'outer;
                }
                0x1b | 0x03 | 0x04 => break 'outer, // ESC / ^C / ^D cancel
                0x08 => {
                    if let Some(last) = buf.pop() {
                        let _ = last;
                        util::out("\x08 \x08");
                    }
                    continue;
                }
                0x15 => {
                    // ^U: clear
                    while buf.pop().is_some() {
                        util::out("\x08 \x08");
                    }
                    continue;
                }
                _ => continue,
            }
        }

        if key == K_NONE && mods == 0 {
            if cp == 0x7f {
                if buf.pop().is_some() {
                    util::out("\x08 \x08");
                }
                continue;
            }
            if cp >= 0x20 {
                let mut tmp = [0u8; 8];
                let l = utf8::encode(cp, &mut tmp);
                if buf.len() + 4 < cap {
                    buf.extend_from_slice(&tmp[..l]);
                    util::outn(&tmp[..l]);
                }
                continue;
            }
            continue;
        }
        // mods set or named key: consume and ignore
    }

    util::out("\r\n");
    if !was_raw {
        term::exit_raw();
    }
    r
}

/// fgets-style line read (reads at most cap-1 bytes or a newline, CRLF
/// trimmed like C). None = EOF before any byte (C: NULL).
fn fgets_line(cap: usize) -> Option<String> {
    let max = cap.saturating_sub(1);
    let mut buf: Vec<u8> = Vec::new();
    use std::io::Read;
    let stdin = std::io::stdin();
    let mut h = stdin.lock();
    let mut b = [0u8; 1];
    let mut eof = false;
    while buf.len() < max {
        match h.read(&mut b) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(_) => {
                buf.push(b[0]);
                if b[0] == b'\n' {
                    break;
                }
            }
            Err(_) => {
                eof = true;
                break;
            }
        }
    }
    if buf.is_empty() && eof {
        return None; // fgets: NULL
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ── Candidates / completion ────────────────────────────────────────────

/// Port of `dir_stem_split()`: last `/` splits dir/stem; no slash → `.`.
fn dir_stem_split(tok: &str) -> (String, String) {
    match tok.rfind('/') {
        Some(sl) => {
            let dir = if sl == 0 { "/".to_string() } else { tok[..sl].to_string() };
            (dir, tok[sl + 1..].to_string())
        }
        None => (".".to_string(), tok.to_string()),
    }
}

fn is_dir_path(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// Port of `collect_candidates()` — deduped, order: builtins, dict, files.
fn collect_candidates(tok: &str, is_first: bool) -> Vec<String> {
    if tok.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let add = |s: &str, out: &mut Vec<String>| {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };

    if is_first {
        for b in exec::builtin_names() {
            if b.starts_with(tok) {
                add(b, &mut out);
            }
        }
        dict::with_all(|v| {
            for d in v {
                if d.starts_with(tok) {
                    add(d, &mut out);
                }
            }
        });
        if tok.contains('/') || tok.starts_with('.') {
            let (dir, stem) = dir_stem_split(tok);
            for m in dict::file_matches(Some(&dir), &stem) {
                add(&m, &mut out);
            }
        }
        return out;
    }

    // argument position: complete filenames (dir + stem)
    let (dir, stem) = dir_stem_split(tok);
    for m in dict::file_matches(Some(&dir), &stem) {
        add(&m, &mut out);
    }
    out
}

/// Port of `token_parts()`: split at the last space (kept in `pre`).
fn token_parts(text: &str) -> (String, String) {
    match text.rfind(' ') {
        Some(sp) => (text[..sp + 1].to_string(), text[sp + 1..].to_string()),
        None => (String::new(), text.to_string()),
    }
}

// ── Fish-style ghost suggestion ────────────────────────────────────────

/// Port of `compute_suggest()` (state-local).
fn compute_suggest(st: &mut EdState) {
    st.gh.clear();
    let cfg = config::snapshot();
    if !cfg.guesser {
        return;
    }
    if st.e.cur != st.e.v.len() {
        return;
    }
    let text = st.e.text();
    if text.is_empty() {
        return;
    }

    // most recent history entry starting with the whole line
    let idx = history::find(&text, history::count() as i32 - 1, true);
    if idx >= 0 {
        let item = history::with_all(|v| v.get(idx as usize).cloned()).unwrap_or_default();
        if item.len() >= text.len() {
            let rest = &item[text.len()..];
            if !rest.is_empty() {
                st.gh.set_str(rest);
            }
        }
        return;
    }

    // suggestion for the current token
    let (pre, tok) = token_parts(&text);
    if !tok.is_empty() {
        let c = collect_candidates(&tok, pre.is_empty());
        if c.len() == 1 {
            let c0 = &c[0];
            if c0.starts_with(&tok) {
                st.gh.set_str(&c0[tok.len()..]);
            } else {
                st.gh.set_str(c0);
            }
        }
    }
}

// ── DSR cursor probe (mux pane shells) ─────────────────────────────────

/// Port of `cursor_probe()`. Takes `&mut EdState` because it pushes
/// reply-adjacent bytes back into the pushback — it runs while the
/// caller holds the ED lock (no nested locking).
#[allow(unused_assignments)] // `rep = …` feeds the loop guard in C; we return on hit
fn cursor_probe(st: &mut EdState, row: &mut i32, col: &mut i32) -> i32 {
    if std::env::var_os("THESH_MUX").is_none() {
        st.probe_ok = false;
        return -1;
    }
    if !st.probe_ok {
        return -1;
    }
    util::out("\x1b[6n");
    // read until the reply shows up; keystrokes forwarded by the mux may
    // arrive first and are pushed back afterwards in order
    let mut buf = [0u8; 256];
    let mut nr: usize = 0;
    let mut rep: i64 = -1; // offset of the reply's ESC
    let mut stage = 0;
    while stage < 3 && rep < 0 {
        let timeout = if stage == 0 { 100 } else { 75 };
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r <= 0 {
            break;
        }
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf[nr..255].as_mut_ptr().cast(),
                255 - nr,
            )
        };
        if n <= 0 {
            break;
        }
        nr += n as usize;
        let mut i = 0;
        while i + 1 < nr {
            if buf[i] != 0x1b || buf[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let mut j = i + 2;
            let mut r0: i32 = 0;
            while j < nr && buf[j].is_ascii_digit() {
                r0 = r0 * 10 + (buf[j] - b'0') as i32;
                j += 1;
            }
            if j >= nr || buf[j] != b';' {
                i += 1;
                continue;
            }
            j += 1;
            let mut c0: i32 = 0;
            while j < nr && buf[j].is_ascii_digit() {
                c0 = c0 * 10 + (buf[j] - b'0') as i32;
                j += 1;
            }
            if j < nr && buf[j] == b'R' && r0 > 0 && c0 > 0 {
                rep = i as i64;
                *row = r0 - 1;
                *col = c0 - 1;
                util::muxdbg(&format!(
                    "probe: reply row={} col={}\n",
                    *row, *col
                ));
                // keys before the reply keep their order; after it too
                if i > 0 {
                    pushback(&buf[..i]);
                }
                if j + 1 < nr {
                    pushback(&buf[j + 1..nr]);
                }
                return 0;
            }
        }
        stage += 1;
    }
    // Never push back a partial reply prefix — only an ESC-anchored
    // reply-shaped suffix is dropped; real keys are kept.
    let mut keep = nr as i64;
    let last_esc = buf[..nr].iter().rposition(|&c| c == 0x1b);
    if let Some(le) = last_esc {
        let mut ok = true;
        for &c in &buf[le + 1..nr] {
            if !(c == b'[' || c == b';' || c.is_ascii_digit()) {
                ok = false;
                break;
            }
        }
        if ok {
            keep = le as i64;
        }
    }
    if keep > 0 {
        pushback(&buf[..keep as usize]);
    }
    if nr == 0 {
        st.probe_misses += 1;
        if st.probe_misses >= 3 {
            st.probe_ok = false; // true silence: fall back
        }
    }
    -1
}

// ── Wrap-aware drawing ─────────────────────────────────────────────────

/// Port of `prompt_cells()` — ANSI escapes render as zero cells.
fn prompt_cells(st: &EdState) -> i32 {
    let mut c = 0;
    let p = st.prompt.as_bytes();
    let mut i = 0;
    while i < p.len() {
        if p[i] == 0x1b {
            i += 1;
            if i < p.len() && p[i] == b'[' {
                i += 1;
                while i < p.len() && !((0x40..=0x7e).contains(&p[i])) {
                    i += 1;
                }
                if i < p.len() {
                    i += 1;
                }
            } else if i < p.len() && p[i] == b']' {
                i += 1;
                while i < p.len() && p[i] != 0x07 {
                    if p[i] == 0x1b && i + 1 < p.len() && p[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < p.len() && p[i] == 0x07 {
                    i += 1;
                }
            } else if i < p.len() {
                i += 1; // ESC x
            }
            continue;
        }
        let (cp, n, _) = utf8::decode(&p[i..]);
        c += utf8::cp_width(cp);
        i += if n == 0 { 1 } else { n };
    }
    c
}

/// Position of cell `c` on the anchor row (first row is short).
fn ed_row_of(st: &EdState, c: i32) -> i32 {
    let mut cap0 = st.ed_w - st.ed_anchor_col;
    if cap0 < 1 {
        cap0 = 1;
    }
    if c < cap0 {
        return 0;
    }
    1 + (c - cap0) / st.ed_w
}

fn ed_col_of(st: &EdState, c: i32) -> i32 {
    let mut cap0 = st.ed_w - st.ed_anchor_col;
    if cap0 < 1 {
        cap0 = 1;
    }
    if c < cap0 {
        return st.ed_anchor_col + c;
    }
    (c - cap0) % st.ed_w
}

/// Draw the ghost with its color (shared by ed_draw / anim_draw).
fn dump_ghost(st: &EdState) {
    if st.gh.v.is_empty() {
        return;
    }
    let cfg = config::snapshot();
    if let Some(col) = cfg.col_guess.as_deref() {
        let sgb = config::color_sgr(Some(col), cfg.op_guess);
        if !sgb.is_empty() {
            util::out(&format!("\x1b[{sgb}m"));
        }
        st.gh.dump(0, st.gh.v.len());
        util::out("\x1b[0m");
    } else {
        util::out("\x1b[2m");
        st.gh.dump(0, st.gh.v.len());
        util::out("\x1b[22m");
    }
}

/// Port of `ed_draw()` — body; callers hold the ED lock.
fn ed_draw_impl(st: &mut EdState) {
        let mut cols = 0;
        let mut rows = 0;
        let mut cx = 0;
        let mut cy = 0;

        let have = if std::env::var_os("THESH_MUX").is_some() {
            mux::term_size(&mut cols, &mut rows); // the pty IS the pane
            if st.ed_anchor < 0 {
                // anchor comes from one DSR probe per fresh line
                if cursor_probe(st, &mut cy, &mut cx) == 0 {
                    1
                } else {
                    0
                }
            } else {
                1
            }
        } else {
            0 // plain terminal
        };

        if have == 1 && cols > 0 && rows > 0 {
            let plen = prompt_cells(st);
            let cur_cell = plen + st.e.cells(0, st.e.cur);
            let tcells = plen + st.e.cells(0, st.e.v.len()) + st.gh.cells(0, st.gh.v.len());

            if util::muxdbg_on() {
                util::muxdbg(&format!(
                    "draw: pid={} have={} geom={}x{} cx={} cy={} \
                     anchor_in={} acol={} off={} span={} w={} h={} \
                     cur_cell={} tcells={} gh={} elen={} ecur={}\n",
                    unsafe { libc::getpid() },
                    have,
                    cols,
                    rows,
                    cx,
                    cy,
                    st.ed_anchor,
                    st.ed_anchor_col,
                    st.ed_off,
                    st.ed_span,
                    st.ed_w,
                    st.ed_h,
                    cur_cell,
                    tcells,
                    st.gh.v.len(),
                    st.e.v.len(),
                    st.e.cur
                ));
            }

            if st.ed_anchor >= 0 && (cols != st.ed_w || rows != st.ed_h) {
                // resized: keep the anchor and re-fit it
                if st.ed_anchor + st.ed_span > rows {
                    st.ed_anchor = rows - st.ed_span;
                    if st.ed_anchor < 0 {
                        st.ed_anchor = 0;
                    }
                }
            }
            if st.ed_anchor == -1 {
                // fresh line: starts AT the cursor
                st.ed_anchor = cy;
                st.ed_anchor_col = cx;
                st.ed_off = 0;
                st.ed_span = 1;
                if st.ed_anchor > rows - 1 {
                    st.ed_anchor = rows - 1;
                }
                if st.ed_anchor < 0 {
                    st.ed_anchor = 0;
                }
            }
            st.ed_w = cols;
            st.ed_h = rows;
            let old_span = st.ed_span;
            if util::muxdbg_on() {
                util::muxdbg(&format!(
                    "draw: pid={} anchor_out={} acol={} off={} span={}\n",
                    unsafe { libc::getpid() },
                    st.ed_anchor,
                    st.ed_anchor_col,
                    st.ed_off,
                    st.ed_span
                ));
            }

            // absolute draw start: pane-local CUP to the anchor
            util::out_fmt(format_args!(
                "\x1b[{};{}H",
                st.ed_anchor + 1,
                st.ed_anchor_col + 1
            ));

            if !st.prompt.is_empty() {
                util::out(&st.prompt);
            }
            st.e.dump(0, st.e.cur);
            st.e.dump(st.e.cur, st.e.v.len());
            dump_ghost(st);
            util::out("\x1b[K");

            let last = if tcells > 0 { tcells - 1 } else { 0 };
            let end_row = ed_row_of(st, last);
            let new_span = end_row + 1;

            // the draw may have scrolled the pane: shift the anchor
            let over = (st.ed_anchor + new_span - 1) - (rows - 1);
            if over > 0 {
                st.ed_anchor -= over;
            }

            // clear rows only when the line got SHORTER
            let mut rr = new_span;
            while rr < old_span {
                if st.ed_anchor + rr > rows - 1 {
                    break;
                }
                util::out_fmt(format_args!("\x1b[{};1H\x1b[K", st.ed_anchor + rr + 1));
                rr += 1;
            }

            // park the cursor on its cell
            let cur_row = ed_row_of(st, cur_cell);
            let cur_col = ed_col_of(st, cur_cell);
            util::out_fmt(format_args!(
                "\x1b[{};{}H",
                st.ed_anchor + cur_row + 1,
                cur_col + 1
            ));
            if util::muxdbg_on() {
                util::muxdbg(&format!(
                    "draw: pid={} cur_row={} cur_col={} expect_park=({},{})\n",
                    unsafe { libc::getpid() },
                    cur_row,
                    cur_col,
                    st.ed_anchor + cur_row,
                    cur_col
                ));
            }

            st.ed_off = cur_row;
            st.ed_span = new_span;
            st.ed_drew = true;
            return;
        }

        // plain terminal (or mux not ready): historical single-row draw
        if util::muxdbg_on() {
            util::muxdbg(&format!("draw: PLAIN path have={}\n", have));
        }
        util::out("\r");
        if !st.prompt.is_empty() {
            util::out(&st.prompt);
        }
        st.e.dump(0, st.e.cur);
        st.e.dump(st.e.cur, st.e.v.len());
        dump_ghost(st);
        util::out("\x1b[K");
        let back = st.e.cells(st.e.cur, st.e.v.len()) + st.gh.cells(0, st.gh.v.len());
        if back > 0 {
            util::out_fmt(format_args!("\x1b[{back}D"));
        }
        if st.ed_drew {
            st.ed_anchor = -1;
        }
        st.ed_drew = true;
}

/// Locking wrapper (used by check_resize).
fn ed_draw() {
    with_mut(ed_draw_impl);
}

/// Port of `ed_leave_line()` — park at the span end, drop the anchor.
fn ed_leave_line(st: &mut EdState) {
    if st.ed_anchor >= 0 {
        util::out_fmt(format_args!("\x1b[{};1H", st.ed_anchor + st.ed_span));
    }
    st.ed_anchor = -1;
    st.ed_off = 0;
    st.ed_span = 1;
    st.ed_drew = false;
}

/// Port of `rsearch_draw()`.
fn rsearch_draw(st: &mut EdState) {
    let m = if st.rmatch >= 0 {
        history::with_all(|v| v.get(st.rmatch as usize).cloned()).unwrap_or_default()
    } else {
        "(no match)".to_string()
    };
    if st.ed_anchor >= 0 {
        let cur_cell = prompt_cells(st) + st.e.cells(0, st.e.cur);
        util::out_fmt(format_args!("\x1b[{};1H\x1b[K", st.ed_anchor + st.ed_span + 1));
        util::out_fmt(format_args!(
            "(reverse-i-search)`{}': {}",
            st.rq, m
        ));
        util::out_fmt(format_args!(
            "\x1b[{};{}H",
            st.ed_anchor + st.ed_off + 1,
            ed_col_of(st, cur_cell) + 1
        ));
        return;
    }
    util::out("\n\x1b[K");
    util::out_fmt(format_args!("(reverse-i-search)`{}': {}", st.rq, m));
}

/// Port of `clear_screen()`.
fn clear_screen(st: &mut EdState) {
    util::out("\x1b[H\x1b[2J");
    st.ed_anchor = -1;
}

// ── Typed-character animations ─────────────────────────────────────────

/// Private PRNG for animation frames — C uses glibc rand() (never
/// seeded, but the exact sequence is an implementation detail; DEVIATION
/// is cosmetic-only: frames differ, the final settled state does not).
fn anim_rand() -> u64 {
    static STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x9e3779b97f4a7c15);
    let mut x = STATE.load(std::sync::atomic::Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, std::sync::atomic::Ordering::Relaxed);
    x
}

fn rand_anim_glyph() -> u32 {
    const POOL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#?";
    POOL[(anim_rand() % POOL.len() as u64) as usize] as u32
}

/// Port of `anim_pump()` — pace one frame; true = keystroke waiting.
fn anim_pump() -> bool {
    let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ANIM_FRAME_MS as i32) > 0 }
}

/// Port of `anim_draw()`.
fn anim_draw(st: &mut EdState, idx: usize, offs: i32, glyph: u32) {
    let w = utf8::cp_width(st.e.v[idx]); // natural char width
    let mut tmp = [0u8; 8];
    let gl = if glyph != 0 { utf8::encode(glyph, &mut tmp) } else { 0 };
    let gw = if gl > 0 { utf8::cp_width(glyph) } else { 0 };
    let printed: i32;

    if st.ed_anchor >= 0 {
        util::out_fmt(format_args!("\x1b[{};1H", st.ed_anchor + 1));
    } else {
        util::out("\r");
    }
    if !st.prompt.is_empty() {
        util::out(&st.prompt);
    }
    st.e.dump(0, idx);

    if offs < 0 {
        for _ in 0..w {
            util::out(" ");
        }
        util::out_fmt(format_args!("\x1b[{}D", w - offs));
        if gl > 0 {
            util::outn(&tmp[..gl]);
        }
        util::out_fmt(format_args!("\x1b[{}C", w - offs - gw));
        printed = w;
    } else if offs == 0 {
        if gl > 0 {
            util::outn(&tmp[..gl]);
        } else {
            for _ in 0..w {
                util::out(" ");
            }
        }
        for i in gw..w {
            let _ = i;
            util::out(" ");
        }
        printed = gw.max(w);
    } else {
        for _ in 0..offs {
            util::out(" ");
        }
        if gl > 0 {
            util::outn(&tmp[..gl]);
        }
        for i in (offs + gw)..w {
            let _ = i;
            util::out(" ");
        }
        printed = (offs + gw).max(w);
    }

    st.e.dump(idx + 1, st.e.v.len());
    dump_ghost(st);
    util::out("\x1b[K");
    let back =
        printed + st.e.cells(idx + 1, st.e.v.len()) + st.gh.cells(0, st.gh.v.len()) - w;
    if back > 0 {
        util::out_fmt(format_args!("\x1b[{back}D"));
    }
}

/// Port of `ed_animate()`.
fn ed_animate(st: &mut EdState, idx: usize) {
    let cfg = config::snapshot();
    if cfg.animation == config::ANIM_NONE {
        return;
    }
    if !term::raw_active() {
        return;
    }
    if st.ed_anchor >= 0
        && (st.ed_anchor_col > 0
            || st.ed_span > 1
            || prompt_cells(st)
                + st.e.cells(0, st.e.v.len())
                + st.gh.cells(0, st.gh.v.len())
                + ANIM_FRAMES
                + 2
                > st.ed_w - st.ed_anchor_col)
    {
        return; // wrapped / mid-row line: plain redraw instead
    }

    let real = st.e.v[idx];
    compute_suggest(st); // fresh ghost behind the frames

    match cfg.animation {
        config::ANIM_MATRIX => {
            for _ in 0..ANIM_FRAMES {
                anim_draw(st, idx, 0, rand_anim_glyph());
                if anim_pump() {
                    return;
                }
            }
        }
        config::ANIM_SPINNER => {
            const FR: [u32; 4] = [b'/' as u32, b'-' as u32, b'\\' as u32, b'|' as u32];
            for f in 0..ANIM_FRAMES {
                anim_draw(st, idx, 0, FR[(f % 4) as usize]);
                if anim_pump() {
                    return;
                }
            }
        }
        config::ANIM_NEWCOMER => {
            let mut f = ANIM_FRAMES;
            while f >= 1 {
                anim_draw(st, idx, f, real);
                if anim_pump() {
                    return;
                }
                f -= 1;
            }
        }
        config::ANIM_PLACEMENT => {
            let mut f = ANIM_FRAMES;
            while f >= 1 {
                anim_draw(st, idx, (anim_rand() % 7) as i32 - 3, rand_anim_glyph());
                if anim_pump() {
                    return;
                }
                f -= 1;
            }
        }
        _ => return,
    }
    anim_draw(st, idx, 0, real); // settle on the real character
}

// ── History navigation ─────────────────────────────────────────────────

/// Port of `hist_up()`.
fn hist_up(st: &mut EdState) {
    if history::count() == 0 {
        return;
    }
    if st.histq.is_none() {
        let t = st.e.text();
        st.histq = Some(t.clone());
        st.bound = Some(t);
    }
    let histq = st.histq.clone().unwrap_or_default();
    let start = if st.browse >= 0 { st.browse - 1 } else { history::count() as i32 - 1 };
    let idx = history::find(&histq, start, true);
    if idx >= 0 {
        st.browse = idx;
        let item = history::with_all(|v| v.get(idx as usize).cloned()).unwrap_or_default();
        st.e.set_str(&item);
    }
}

/// Port of `hist_down()`.
fn hist_down(st: &mut EdState) {
    if st.browse < 0 || st.histq.is_none() {
        return;
    }
    let histq = st.histq.clone().unwrap_or_default();
    let idx = history::find(&histq, st.browse + 1, false);
    if idx >= 0 {
        st.browse = idx;
        let item = history::with_all(|v| v.get(idx as usize).cloned()).unwrap_or_default();
        st.e.set_str(&item);
    } else {
        st.browse = -1;
        if let Some(b) = st.bound.take() {
            st.e.set_str(&b);
        } else {
            st.e.clear();
        }
    }
}

// ── Tab completion ─────────────────────────────────────────────────────

/// Port of `tab_complete()`.
fn tab_complete(st: &mut EdState) {
    let text = st.e.text();
    let (pre, tok) = token_parts(&text);
    let first = pre.is_empty();
    let c = collect_candidates(&tok, first);

    if c.len() == 1 {
        let mut nl = format!("{pre}{}", c[0]);
        if !nl.ends_with('/') && is_dir_path(&nl) {
            nl.push('/');
        }
        st.e.set_str(&nl);
    } else if c.len() > 1 {
        let tl = tok.len();
        // byte-wise common prefix of all candidates (C compares bytes)
        let mut cpref = c[0].clone();
        for cand in &c[1..] {
            let mut j = 0;
            let a = cpref.as_bytes();
            let b = cand.as_bytes();
            while j < a.len() && j < b.len() && a[j] == b[j] {
                j += 1;
            }
            cpref.truncate(j);
        }
        let cpref = String::from_utf8_lossy(cpref.as_bytes()).into_owned();
        if cpref.len() > tl {
            st.e.set_str(&format!("{pre}{cpref}"));
        } else {
            // list the matches; the next keystroke redraws the line
            ed_leave_line(st);
            util::out("\r\n");
            for (i, cand) in c.iter().enumerate() {
                util::out_fmt(format_args!("{}{}", if i > 0 { "  " } else { "" }, cand));
            }
            util::out("\r\n");
        }
    }
}

// ── Non-interactive fallback ───────────────────────────────────────────

fn edit_line_fallback() -> Option<String> {
    fgets_line(LINE_MAX_CP)
}

// ── Clipboard (internal + OSC 52) ──────────────────────────────────────

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Port of `b64encode()`.
fn b64encode(input: &[u8]) -> String {
    let mut o = String::new();
    let n = input.len();
    let mut i = 0;
    while i + 2 < n {
        let v = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        o.push(B64[((v >> 18) & 63) as usize] as char);
        o.push(B64[((v >> 12) & 63) as usize] as char);
        o.push(B64[((v >> 6) & 63) as usize] as char);
        o.push(B64[(v & 63) as usize] as char);
        i += 3;
    }
    if i < n {
        let mut v = (input[i] as u32) << 16;
        let rem = n - i;
        if rem == 2 {
            v |= (input[i + 1] as u32) << 8;
        }
        o.push(B64[((v >> 18) & 63) as usize] as char);
        o.push(B64[((v >> 12) & 63) as usize] as char);
        o.push(if rem == 2 { B64[((v >> 6) & 63) as usize] as char } else { '=' });
        o.push('=');
    }
    o
}

/// Port of `clip_set()`.
fn clip_set(st: &mut EdState, s: &str) {
    st.clip = Some(s.to_string());
    let b = b64encode(s.as_bytes());
    util::out("\x1b]52;c;"); // OSC 52, best effort
    util::out(&b);
    util::out("\x1b\\");
}

/// Port of `clip_paste()`.
fn clip_paste(st: &mut EdState) {
    let Some(clip) = st.clip.clone() else { return };
    let mut rest = clip.as_str();
    while !rest.is_empty() {
        let (c, n, _) = utf8::decode(rest.as_bytes());
        if c >= 0x20 && c != 0x7f {
            st.e.insert(st.e.cur, c);
            st.e.cur += 1;
        }
        if n == 0 {
            break;
        }
        rest = &rest[n.min(rest.len())..];
    }
}

// ── User key bindings ──────────────────────────────────────────────────

/// Port of `bind_run()`: 1 = the shell should exit.
fn bind_run(b: &Bind) -> bool {
    match b.kind {
        BindKind::Close => {
            with_mut(ed_leave_line);
            util::out("\r\n");
            true
        }
        BindKind::Copy => {
            let t = with(|st| st.e.text());
            with_mut(|st| clip_set(st, &t));
            with_mut(ed_leave_line);
            util::out("\r\n");
            util::out_fmt(format_args!("{NAME}: copied\r\n"));
            false
        }
        BindKind::Paste => {
            with_mut(clip_paste);
            false
        }
        BindKind::Exec => {
            if !b.ask.is_empty() {
                with_mut(ed_leave_line);
                util::out("\r\n"); // leave the typing line
                let pbuf = format!("{} : ", b.ask);
                if let Some(input) = prompt_line_cap(&pbuf, bind::BIND_CMD_MAX) {
                    let cmd = format!("{} {}", b.cmd, input);
                    exec::exec_line(&cmd);
                }
                return false;
            }
            with_mut(ed_leave_line);
            util::out("\r\n");
            exec::exec_line(&b.cmd);
            false
        }
        BindKind::Verb => {
            with_mut(ed_leave_line);
            util::out("\r\n");
            exec::exec_line(&b.cmd);
            false
        }
    }
}

// ── Movement speed & hold-to-amplify ───────────────────────────────────

/// Port of `arrow_step()`.
fn arrow_step(st: &mut EdState, key: i32) -> i32 {
    let idx = (key - K_UP) as usize; // K_UP K_DOWN K_LEFT K_RIGHT
    let cfg = config::snapshot();
    let mut base = if key == K_UP || key == K_DOWN {
        cfg.scrollspeed
    } else {
        cfg.movespeed
    };

    let now = Instant::now();
    let dt_ms: i64 = match st.last_arrow[idx.min(3)] {
        Some(prev) => now.duration_since(prev).as_millis() as i64,
        None => -1, // first press of this key
    };
    st.last_arrow[idx.min(3)] = Some(now);

    if dt_ms >= 0 && dt_ms <= AMPLIFY_HOLD_MS {
        base *= cfg.amplify;
    }
    base
}

/// Port of `try_bind()`: 0 = no bind, 1 = ran, 2 = wants EOF.
fn try_bind(key: i32, cp: u32, mods: i32) -> i32 {
    // Shared bytes (Enter/Ctrl+M/Ctrl+J, Backspace/Ctrl+H) only consult
    // binds on an empty line.
    let shared_byte = key == K_NONE && (cp == 0x0d || cp == 0x0a || cp == 0x08);
    if shared_byte && with(|st| st.e.v.len()) > 0 {
        return 0;
    }

    let bkey = if key == K_NONE {
        if cp > 0 && cp < 0x1b {
            (cp + 0x60) as i32 // ctrl letter
        } else {
            cp as i32
        }
    } else {
        key
    };
    let Some(bd) = bind::lookup(mods, bkey) else { return 0 };
    if bind_run(&bd) {
        2
    } else {
        1
    }
}

/// Port of `ed_bind_lookup()` — shared with the router (no shared-byte
/// rule).
pub fn bind_lookup(key: i32, cp: u32, mods: i32) -> Option<Bind> {
    let bkey = if key == K_NONE {
        if cp > 0 && cp < 0x1b {
            (cp + 0x60) as i32
        } else {
            cp as i32
        }
    } else {
        key
    };
    bind::lookup(mods, bkey)
}

/// Port of `ed_reset_anchor()`.
pub fn reset_anchor() {
    with_mut(|st| {
        st.ed_anchor = -1;
        st.ed_off = 0;
        st.ed_span = 1;
        st.ed_drew = false;
    });
}

/// Port of `mux_nav()` / `ed_mux_nav()`:
/// 0 left, 1 right, 2 up, 3 down, −1 = not a nav key.
/// (CHANGED from the skeleton's `bool`: the router needs the direction.)
pub fn mux_nav(key: i32, mods: i32) -> i32 {
    if !mux::active() || mods & MOD_CTRL == 0 {
        return -1;
    }
    match key {
        K_LEFT | K_WLEFT => 0,
        K_RIGHT | K_WRIGHT => 1,
        K_UP => 2,
        K_DOWN => 3,
        _ => -1,
    }
}

/// Port of `ed_mouse_forward()` — consulted only for the event just read.
pub fn mouse_forward() -> bool {
    with(|st| st.mouse_fwd)
}

// ── The main line editor ───────────────────────────────────────────────

enum Flow {
    Next,    // `break` out of the switch → loop again
    Edited,  // `goto edited` → hist_reset + loop
    Submit,  // Enter → done
    Cancel,  // Ctrl+C → done (cancelled)
    Eof,     // EOF / Ctrl+D → done (NULL)
}

/// Port of `edit_line()`. Returns None on EOF (C: NULL), Some("") on
/// cancel (C: empty string), Some(line) otherwise.
pub fn edit_line(prompt_txt: &str, cancelled: &mut bool) -> Option<String> {
    *cancelled = false;
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return edit_line_fallback();
    }

    let mut eof = false;
    if term::enter_raw().is_err() {
        eprintln!(
            "{NAME}: cannot set raw mode: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }

    with_mut(|st| {
        st.prompt = prompt_txt.to_string();
        st.e = Buf::new();
        st.gh = Buf::new();
        st.ed_anchor = -1; // fresh line: anchor = cursor
        st.ed_off = 0;
        st.ed_span = 1;
        st.ed_drew = false;
        st.hist_reset();
        st.rq.clear();
        st.rmatch = -1;
        st.rsearch_on = false;
        st.rbase = None;
        st.last_arrow = [None; 4]; // first press is always base speed
    });

    'main: loop {
        // every loop draws the line before blocking on a key
        with_mut(|st| {
            compute_suggest(st);
            ed_draw_impl(st);
            if st.rsearch_on {
                rsearch_draw(st);
            }
        });

        let mut cp = 0u32;
        let mut mods = 0;
        let key = read_key(&mut cp, &mut mods);

        if key == K_EOF {
            eof = true;
            break 'main;
        }

        let control = key == K_NONE && cp < 0x20;

        // user bindings intercept before the default key actions
        let tb = try_bind(key, cp, mods);
        if tb == 2 {
            eof = true;
            break 'main;
        }
        if tb == 1 {
            continue;
        }

        // map plain control keys
        let mut key = key;
        if key == K_NONE && control {
            key = match cp {
                0x01 => K_CTL_A,
                0x02 => K_CTL_B,
                0x03 => K_CTL_C,
                0x04 => K_CTL_D,
                0x05 => K_CTL_E,
                0x06 => K_CTL_F,
                0x09 => K_TAB,
                0x0b => K_CTL_K,
                0x0c => K_CTL_L,
                0x0a | 0x0d => K_ENTER,
                0x12 => K_CTL_R,
                0x15 => K_CTL_U,
                0x17 => K_CTL_W,
                _ => K_NONE,
            };
        }
        // DEL (0x7f) and BS (0x08) are both Backspace
        if key == K_NONE && (cp == 0x7f || cp == 0x08) {
            key = K_DEL;
        }

        let flow = match key {
            K_ENTER => {
                let mut f = Flow::Submit;
                with_mut(|st| {
                    if st.rsearch_on {
                        if st.rmatch >= 0 {
                            let item = history::with_all(|v| {
                                v.get(st.rmatch as usize).cloned()
                            })
                            .unwrap_or_default();
                            st.e.set_str(&item);
                        }
                        st.rsearch_on = false;
                        f = Flow::Next; // keep edited line
                        return;
                    }
                    ed_leave_line(st);
                });
                f
            }
            K_CTL_C => with_mut(|st| {
                if st.rsearch_on {
                    st.rsearch_on = false;
                    return Flow::Next;
                }
                st.e.clear();
                ed_leave_line(st);
                Flow::Cancel
            }),
            K_CTL_D => {
                let empty = with(|st| st.e.v.is_empty() && st.e.cur == 0);
                if empty {
                    with_mut(ed_leave_line);
                    Flow::Eof
                } else {
                    with_mut(|st| st.e.del(st.e.cur));
                    Flow::Next
                }
            }
            K_DEL => with_mut(|st| {
                if st.e.cur > 0 {
                    st.e.cur -= 1;
                    st.e.del(st.e.cur);
                }
                Flow::Edited
            }),
            K_FDEL => with_mut(|st| {
                st.e.del(st.e.cur);
                Flow::Edited
            }),
            K_LEFT => with_mut(|st| {
                let mut step = arrow_step(st, K_LEFT);
                while step > 0 && st.e.cur > 0 {
                    st.e.cur -= 1;
                    step -= 1;
                }
                Flow::Next
            }),
            K_RIGHT => with_mut(|st| {
                let mut step = arrow_step(st, K_RIGHT);
                while step > 0 && st.e.cur < st.e.v.len() {
                    st.e.cur += 1;
                    step -= 1;
                }
                Flow::Next
            }),
            K_HOME => with_mut(|st| {
                st.e.cur = 0;
                Flow::Next
            }),
            K_END => with_mut(|st| {
                st.e.cur = st.e.v.len();
                Flow::Next
            }),
            K_WLEFT => with_mut(|st| {
                let mut i = st.e.cur;
                while i > 0 && !is_word(st.e.v[i - 1]) {
                    i -= 1;
                }
                while i > 0 && is_word(st.e.v[i - 1]) {
                    i -= 1;
                }
                st.e.cur = i;
                Flow::Next
            }),
            K_WRIGHT => with_mut(|st| {
                let mut i = st.e.cur;
                while i < st.e.v.len() && is_word(st.e.v[i]) {
                    i += 1;
                }
                while i < st.e.v.len() && !is_word(st.e.v[i]) {
                    i += 1;
                }
                st.e.cur = i;
                Flow::Next
            }),
            K_UP => with_mut(|st| {
                let mut step = arrow_step(st, K_UP);
                while step > 0 {
                    hist_up(st);
                    step -= 1;
                }
                Flow::Next
            }),
            K_DOWN => with_mut(|st| {
                let mut step = arrow_step(st, K_DOWN);
                while step > 0 {
                    hist_down(st);
                    step -= 1;
                }
                Flow::Next
            }),
            K_CTL_K => with_mut(|st| {
                while st.e.cur < st.e.v.len() {
                    st.e.del(st.e.cur);
                }
                Flow::Edited
            }),
            K_CTL_U => with_mut(|st| {
                while st.e.cur > 0 {
                    st.e.cur -= 1;
                    st.e.del(st.e.cur);
                }
                Flow::Edited
            }),
            K_CTL_W => with_mut(|st| {
                let mut i = st.e.cur;
                while i > 0 && !is_word(st.e.v[i - 1]) {
                    i -= 1;
                }
                while i > 0 && is_word(st.e.v[i - 1]) {
                    i -= 1;
                }
                while st.e.cur > i {
                    st.e.cur -= 1;
                    st.e.del(st.e.cur);
                }
                Flow::Edited
            }),
            K_CTL_A => with_mut(|st| {
                st.e.cur = 0;
                Flow::Next
            }),
            K_CTL_E => with_mut(|st| {
                st.e.cur = st.e.v.len();
                Flow::Next
            }),
            K_CTL_B => with_mut(|st| {
                if st.e.cur > 0 {
                    st.e.cur -= 1;
                }
                Flow::Next
            }),
            K_CTL_F => with_mut(|st| {
                if st.e.cur < st.e.v.len() {
                    st.e.cur += 1;
                }
                Flow::Next
            }),
            K_CTL_L => with_mut(|st| {
                clear_screen(st);
                Flow::Next
            }),
            K_CTL_R => with_mut(|st| {
                if !st.rsearch_on {
                    st.rsearch_on = true;
                    st.rq.clear();
                    st.rmatch = -1;
                    st.rbase = Some(st.e.text());
                } else if !st.rq.is_empty() {
                    // earlier match
                    let base = if st.rmatch >= 0 { st.rmatch - 1 } else { history::count() as i32 - 1 };
                    st.rmatch = history::find_substr(&st.rq, base, true);
                }
                Flow::Next
            }),
            K_TAB => with_mut(|st| {
                if !st.rsearch_on {
                    tab_complete(st);
                }
                Flow::Edited
            }),
            K_PGUP => with_mut(|st| {
                st.e.cur = 0;
                Flow::Next
            }),
            K_PGDN => with_mut(|st| {
                st.e.cur = st.e.v.len();
                Flow::Next
            }),
            _ => {
                if key == K_NONE && !control {
                    // printable char
                    let in_rsearch = with(|st| st.rsearch_on);
                    if in_rsearch {
                        with_mut(|st| {
                            // C: rqlen + 4 < 512 (byte cap)
                            let mut tmp = [0u8; 8];
                            let l = utf8::encode(cp, &mut tmp);
                            if st.rq.len() + 4 < 512 {
                                st.rq.push_str(&String::from_utf8_lossy(&tmp[..l]));
                                st.rmatch = history::find_substr(
                                    &st.rq,
                                    history::count() as i32 - 1,
                                    true,
                                );
                            }
                        });
                        Flow::Next
                    } else {
                        with_mut(|st| {
                            st.e.insert(st.e.cur, cp);
                            st.e.cur += 1;
                            ed_animate(st, st.e.cur - 1);
                        });
                        Flow::Edited
                    }
                } else {
                    Flow::Next
                }
            }
        };

        match flow {
            Flow::Next => continue,
            Flow::Edited => {
                with_mut(|st| st.hist_reset());
                continue;
            }
            Flow::Submit => {
                util::out("\r\n");
                break 'main;
            }
            Flow::Cancel => {
                util::out("\r\n");
                *cancelled = true;
                break 'main;
            }
            Flow::Eof => {
                util::out("\r\n");
                eof = true;
                break 'main;
            }
        }
    }

    with_mut(|st| st.hist_reset());
    if eof {
        return None;
    }
    if *cancelled {
        return Some(String::new());
    }
    Some(with(|st| st.e.text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(s: &[u8]) -> (i32, u32, i32) {
        let mut mods = 0;
        let out = parse_seq(s, &mut mods);
        (out.key, out.cp, mods)
    }

    #[test]
    fn parse_plain_and_alt_sequences() {
        // bare ESC
        assert_eq!(seq(&[27]), (K_NONE, 27, 0));
        // Alt+b / Alt+f → word jumps
        assert_eq!(seq(&[27, b'b']), (K_WLEFT, 0, MOD_ALT));
        assert_eq!(seq(&[27, b'f']), (K_WRIGHT, 0, MOD_ALT));
        // Alt+X plain char
        assert_eq!(seq(&[27, b'x']), (K_NONE, b'x' as u32, MOD_ALT));
        // Ctrl+Alt+X
        assert_eq!(seq(&[27, 0x01]), (K_NONE, 0x01, MOD_CTRL | MOD_ALT));
        // Alt+DEL
        assert_eq!(seq(&[27, 0x7f]), (K_NONE, 0x7f, MOD_ALT));
        // meta-prefixed CSI: ESC ESC [ A = Alt+Up
        assert_eq!(seq(&[27, 27, b'[', b'A']), (K_UP, 0, MOD_ALT));
    }

    #[test]
    fn parse_csi_keys_and_modifiers() {
        assert_eq!(seq(b"\x1b[A"), (K_UP, 0, 0));
        assert_eq!(seq(b"\x1b[B"), (K_DOWN, 0, 0));
        assert_eq!(seq(b"\x1b[C"), (K_RIGHT, 0, 0));
        assert_eq!(seq(b"\x1b[D"), (K_LEFT, 0, 0));
        // modified arrows: xterm param 5 = ctrl → word-jump mapping
        assert_eq!(seq(b"\x1b[1;5C"), (K_WRIGHT, 0, MOD_CTRL));
        assert_eq!(seq(b"\x1b[1;5D"), (K_WLEFT, 0, MOD_CTRL));
        assert_eq!(seq(b"\x1b[1;2A"), (K_UP, 0, MOD_SHIFT));
        // shift+tab
        assert_eq!(seq(b"\x1b[Z"), (K_TAB, 0, MOD_SHIFT));
        // tilde form
        assert_eq!(seq(b"\x1b[3~"), (K_FDEL, 0, 0));
        assert_eq!(seq(b"\x1b[5~"), (K_PGUP, 0, 0));
        assert_eq!(seq(b"\x1b[1~"), (K_HOME, 0, 0));
        assert_eq!(seq(b"\x1b[15~"), (K_F5, 0, 0));
        assert_eq!(seq(b"\x1b[45~"), (K_F24, 0, 0));
        // SS3 form
        assert_eq!(seq(b"\x1bOP"), (K_F1, 0, 0));
        assert_eq!(seq(b"\x1bOH"), (K_HOME, 0, 0));
        // unknown CSI final byte falls through with the last byte as cp
        // (NOTE: 'Z' is a KNOWN final — Shift+Tab — hence "99q" here)
        assert_eq!(seq(b"\x1b[99q"), (K_NONE, b'q' as u32, 0));
    }

    #[test]
    fn parse_dsr_reply_is_swallowed() {
        // our own cursor probe answer: never typed input
        assert_eq!(seq(b"\x1b[12;40R"), (K_NONE, 0, 0));
        // malformed R-suffix keeps the fallback (not swallowed)
        assert_eq!(seq(b"\x1b[12;4;5R"), (K_NONE, b'R' as u32, 0));
    }

    #[test]
    fn seq_completion_rules() {
        assert!(!seq_complete(&[27]));
        assert!(seq_complete(b"\x1b[A"));
        assert!(!seq_complete(b"\x1b[1;5")); // no final byte yet
        assert!(seq_complete(b"\x1bOx"));
        assert!(!seq_complete(b"\x1bO"));
        assert!(!seq_complete(&[27, 27, b'['])); // meta prefix mid-sequence
        assert!(seq_complete(&[27, 27, b'[', b'A']));
        assert!(seq_complete(&[27, b'x'])); // Alt+X at 2
    }

    #[test]
    fn mod_bits_decode() {
        assert_eq!(mod_bits(1), 0);
        assert_eq!(mod_bits(2), MOD_SHIFT);
        assert_eq!(mod_bits(3), MOD_ALT);
        assert_eq!(mod_bits(5), MOD_CTRL);
        assert_eq!(mod_bits(7), MOD_CTRL | MOD_ALT);
        assert_eq!(mod_bits(9), MOD_SUPER); // meta
        assert_eq!(mod_bits(33), MOD_SUPER); // super
    }

    #[test]
    fn token_parts_splits_at_last_space() {
        assert_eq!(token_parts("ls -la"), ("ls ".to_string(), "-la".to_string()));
        assert_eq!(token_parts("echo "), ("echo ".to_string(), String::new()));
        assert_eq!(token_parts("ls"), (String::new(), "ls".to_string()));
    }

    #[test]
    fn dir_stem_splits_like_c() {
        assert_eq!(dir_stem_split("foo"), (".".to_string(), "foo".to_string()));
        assert_eq!(dir_stem_split("/bin/ls"), ("/bin".to_string(), "ls".to_string()));
        assert_eq!(dir_stem_split("/ls"), ("/".to_string(), "ls".to_string()));
        assert_eq!(dir_stem_split("./x"), (".".to_string(), "x".to_string()));
    }

    #[test]
    fn b64_matches_c_reference() {
        assert_eq!(b64encode(b""), "");
        assert_eq!(b64encode(b"f"), "Zg==");
        assert_eq!(b64encode(b"fo"), "Zm8=");
        assert_eq!(b64encode(b"foo"), "Zm9v");
        assert_eq!(b64encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn buf_operations_and_cap() {
        let mut b = Buf::new();
        b.set_str("h\u{e9}llo");
        assert_eq!(b.v.len(), 5); // codepoints, not bytes
        b.insert(1, b'X' as u32);
        assert_eq!(b.text(), "hX\u{e9}llo");
        b.del(1);
        assert_eq!(b.text(), "h\u{e9}llo");
        assert_eq!(b.cells(0, b.v.len()), 5);
        // LINE_MAX_CP cap: inserts beyond 4096 are dropped (C quirk)
        let mut big = Buf::new();
        for _ in 0..(LINE_MAX_CP + 10) {
            big.insert(big.v.len(), b'a' as u32);
        }
        assert_eq!(big.v.len(), LINE_MAX_CP);
    }

    #[test]
    fn is_word_unicode_deviation() {
        assert!(is_word(b'a' as u32));
        assert!(is_word(b'0' as u32));
        assert!(is_word(b'_' as u32));
        assert!(!is_word(b' ' as u32));
        assert!(!is_word(b'-' as u32));
        // C: isalnum(c) for c > 255 is UB; we accept Unicode alnum
        assert!(is_word(0x4e2d));
    }

    #[test]
    fn prompt_cells_counts_ansi_as_zero() {
        let _g = crate::util::test_lock();
        with_mut(|st| {
            st.prompt = "plain".to_string();
            assert_eq!(prompt_cells(st), 5);
            st.prompt = "\x1b[31mred\x1b[0m".to_string();
            assert_eq!(prompt_cells(st), 3);
            st.prompt = "\x1b]0;title\x07after".to_string();
            assert_eq!(prompt_cells(st), 5);
            st.prompt = format!("{}!", "\u{5bbd}".repeat(2));
            // locale-dependent width (wcwidth): assert via the same
            // primitive so the test is stable under any locale
            assert_eq!(prompt_cells(st), 2 * utf8::cp_width(0x5bbd) + 1);
            st.prompt.clear();
        });
    }

    #[test]
    fn mux_nav_needs_mux_and_ctrl() {
        // mux::active() is false outside the router → -1 regardless
        assert_eq!(mux_nav(K_LEFT, MOD_CTRL), -1);
        assert_eq!(mux_nav(K_LEFT, 0), -1);
    }
}
