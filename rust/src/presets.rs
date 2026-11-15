//! Port of `src/presets.c` — the `presets` / `savepreset` builtins.
//!
//! SKELETON: signatures pinned from exec.c's dispatch
//! (`shell_status = cmd_presets(argc, args); return 1;`).
//!
//! Presets are plain theshrc-style files with a name, found in
//!     user:   ~/.config/thesh/presets/
//!     system: /etc/thesh/presets/
//! `presets` lists them and, after a selection, asks which rc file(s) to
//! make the shell default for (system / user / both), then copies the
//! preset over those rc files and re-sources them so the change takes
//! effect immediately. `savepreset` serializes the current config —
//! options, aliases and key bindings — out as a preset.
//!
//! Locking: this module holds no locks of its own. When it needs other
//! modules' data it calls `config::dump_current`, `exec::dump_aliases`
//! and `bind::dump` *sequentially* — each takes and releases its own
//! lock, so locks are never nested.

use crate::util::out_fmt;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

const THESH_NAME: &str = "thesh";
const PRESET_NAME_MAX: usize = 128;
const PRESET_MAX: usize = 128;
/// `PATH_MAX` from <limits.h> (glibc/Linux: 4096) — C sizes its fixed
/// path buffers with it.
const PATH_MAX: usize = 4096;
/// `LINE_MAX_CP` from thesh.h — the select-prompt line buffer.
const LINE_MAX_CP: usize = 4096;

const SCOPE_SYSTEM: i32 = 1; // "make default for"
const SCOPE_USER: i32 = 2;
const SCOPE_BOTH: i32 = 3;

const LOC_SYSTEM: i32 = 1; // savepreset target
const LOC_USER: i32 = 2;
const LOC_CUSTOM: i32 = 3;

const SCOPE_PROMPT: &str = "make this shell default for...?\n\
    1 : system\n\
    2 : user\n\
    3 : both\n\
    > ";
const SAVE_AT_PROMPT: &str = "save preset at? :\n\
    1 : system /etc/thesh/presets\n\
    2 : user ~/.config/thesh/presets\n\
    3 : custom\n\
    > ";

struct PresetEntry {
    name: String,
    path: String,
}

struct PresetList {
    v: Vec<PresetEntry>,
}

// ── byte helpers ────────────────────────────────────────────────────────

/// C's `isspace()` in the C locale: space, tab, LF, VT, FF, CR.
fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Copy C's raw bytes into a Rust `String` (must be valid UTF-8),
/// cutting at `max` BYTES exactly like `snprintf(dst, max + 1, "%s", src)`.
///
/// DEVIATION: when the byte cut splits a multi-byte UTF-8 sequence the
/// partial tail is dropped (C would keep the broken bytes). ASCII — the
/// realistic case — is byte-identical to C.
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

/// `strerror(errno)` — byte-identical text to C's `dprintf` messages
/// (io::Error's Display would append "(os error N)").
fn strerror(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(errno) => unsafe {
            std::ffi::CStr::from_ptr(libc::strerror(errno))
                .to_string_lossy()
                .into_owned()
        },
        None => e.to_string(),
    }
}

// ── prompt helpers ──────────────────────────────────────────────────────

fn strip_crlf(s: &str) -> &str {
    s.trim_end_matches(|c| c == '\n' || c == '\r')
}

/// Print `prompt`, then read one line. `None` cancels (C: returns −1
/// with an empty buffer, aborting the surrounding command). `sz` mirrors
/// the C caller's `char buf[sz]` — prompt_line wrote at most sz−1 BYTES
/// into it, so we re-apply the cap here (idempotent with a faithful
/// prompt_line, and pins presets.c's own buffer sizes either way).
fn read_line(
    prompt: &str,
    sz: usize,
    f: &mut dyn FnMut(&str) -> Option<String>,
) -> Option<String> {
    let s = f(prompt)?;
    let s = truncate_bytes(s.as_bytes(), sz - 1);
    Some(strip_crlf(&s).to_string())
}

// ── collecting the preset list ──────────────────────────────────────────

fn collect_dir(dir: &str, pl: &mut PresetList) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for de in rd {
        let Ok(de) = de else { break }; // readdir error → stop, like C's NULL
        let nm = de.file_name();
        let nm = nm.as_bytes();
        let l = nm.len();
        if l == 0 || nm[0] == b'.' || nm[l - 1] == b'~' {
            continue; // hidden / swap files
        }
        if l >= PRESET_NAME_MAX {
            continue;
        }
        // user preset shadows system (first dir collected wins; C uses
        // strcmp — byte-exact for the valid-UTF-8 names we can store)
        if pl.v.iter().any(|e| e.name.as_bytes() == nm) {
            continue;
        }
        if pl.v.len() >= PRESET_MAX {
            break;
        }
        // C: snprintf(full, PATH_MAX, "%s/%s", dir, nm) — byte truncate
        let mut full = Vec::with_capacity(dir.len() + 1 + l);
        full.extend_from_slice(dir.as_bytes());
        full.push(b'/');
        full.extend_from_slice(nm);
        let fullb = &full[..full.len().min(PATH_MAX - 1)];
        // C: stat(full) must succeed and be a regular file (stat follows
        // symlinks, so a link to a file is kept — like C)
        let Ok(md) = std::fs::metadata(OsStr::from_bytes(fullb)) else { continue };
        if !md.is_file() {
            continue;
        }
        pl.v.push(PresetEntry {
            // names are < PRESET_NAME_MAX already; the conversion is
            // lossy only for non-UTF-8 file names (C stores raw bytes —
            // DEVIATION for that corner case)
            name: truncate_bytes(nm, PRESET_NAME_MAX - 1),
            path: truncate_bytes(fullb, PATH_MAX - 1),
        });
    }
}

/// C's `preset_collect()`: $HOME first (skipped when unset/empty —
/// deliberately *no* passwd fallback here, unlike `get_home()`), then
/// the system dir. DEVIATION: a non-UTF-8 $HOME is lossily converted.
fn preset_collect() -> PresetList {
    let mut pl = PresetList { v: Vec::new() };
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            collect_dir(&format!("{}/.config/thesh/presets", home.to_string_lossy()), &mut pl);
        }
    }
    collect_dir("/etc/thesh/presets", &mut pl);
    pl
}

// ── input parsing ───────────────────────────────────────────────────────

/// Accept a number (1..n) or an exact name (case-insensitive).
fn preset_pick(pl: &PresetList, input: &str) -> i32 {
    let b = input.as_bytes();
    let mut i = 0;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    let in_ = &b[i..];
    if in_.is_empty() {
        return -1;
    }
    if in_[0].is_ascii_digit() {
        // C: strtol with end == in && !*end → the WHOLE tail must be
        // digits. Overflow (or an out-of-range value) falls through to
        // the name match below, exactly like strtol's LONG_MAX would.
        let digits: String = in_.iter().take_while(|&&c| c.is_ascii_digit()).map(|&c| c as char).collect();
        if digits.len() == in_.len() {
            if let Ok(n) = digits.parse::<i64>() {
                if n >= 1 && n as usize <= pl.v.len() {
                    return (n - 1) as i32;
                }
            }
        }
    }
    let name = String::from_utf8_lossy(in_);
    pl.v
        .iter()
        .position(|e| e.name.eq_ignore_ascii_case(&name))
        .map(|i| i as i32)
        .unwrap_or(-1)
}

/// C's inline scope check in `preset_apply_flow` — leading ws is
/// skipped, but NOT trailing (strcmp on the raw tail: "1 " is USER).
fn parse_scope(input: &str) -> i32 {
    let b = input.as_bytes();
    let mut i = 0;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    let s = &input[i..];
    if s == "1" || s.eq_ignore_ascii_case("system") {
        SCOPE_SYSTEM
    } else if s == "3" || s.eq_ignore_ascii_case("both") || s.eq_ignore_ascii_case("all") {
        SCOPE_BOTH
    } else {
        SCOPE_USER // 2 / user / empty / anything else
    }
}

/// C's inline location check in `cmd_savepreset` — no ws trimming here
/// at all (unlike parse_scope), and only exact "1"/"3" compare as nums.
fn parse_loc(input: &str) -> i32 {
    if input == "1" || input.eq_ignore_ascii_case("system") {
        LOC_SYSTEM
    } else if input == "3" || input.eq_ignore_ascii_case("custom") {
        LOC_CUSTOM
    } else {
        LOC_USER
    }
}

// ── filesystem helpers ──────────────────────────────────────────────────

/// C's `mkdir(path, mode)` on raw bytes: an interior NUL truncates the
/// path exactly like C's string buffers would.
fn mkdir_c(path: &str, mode: u32) -> io::Result<()> {
    let mut b = Vec::with_capacity(path.len() + 1);
    b.extend_from_slice(path.as_bytes());
    b.push(0);
    if unsafe { libc::mkdir(b.as_ptr().cast(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// mkdir -p; safe to call when components already exist (EEXIST is
/// ignored everywhere, as in C — even when the component is a file,
/// which makes the final mkdir fail with ENOTDIR → −1). mode 0755 like
/// C's callers (`create_dir` would use 0777&~umask instead — hence the
/// raw libc::mkdir).
///
/// DEVIATION: C first snprintf's the path into a PATH_MAX buffer; paths
/// ≥ 4096 bytes are not truncated here (they fail outright instead of
/// operating on a truncated prefix).
fn mkdir_p(path: &str) -> i32 {
    let mut tmp = path.to_string();
    while tmp.ends_with('/') {
        tmp.pop();
    }
    if tmp.is_empty() {
        return 0;
    }
    let bytes = tmp.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'/' {
            if let Err(e) = mkdir_c(&tmp[..i], 0o755) {
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return -1;
                }
            }
        }
    }
    match mkdir_c(&tmp, 0o755) {
        Ok(()) => 0,
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => 0,
        Err(_) => -1,
    }
}

fn mkdir_parent(file: &str) {
    let Some(pos) = file.rfind('/') else { return };
    if pos == 0 {
        return; // slash == start of the path (C: `slash == tmp`)
    }
    mkdir_p(&file[..pos]);
}

/// C's `copy_file()`: fopen(src, "r") + fopen(dst, "w") + byte copy.
/// The error is captured at the failing operation — C reads `errno`
/// after the call, which its own fclose() may already have clobbered;
/// keeping the intended errno reproduces C's messages in practice.
fn copy_file(src: &str, dst: &str) -> io::Result<()> {
    let mut inp = std::fs::File::open(src)?;
    let mut outp = std::fs::File::create(dst)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = inp.read(&mut buf)?; // C: fread error → ferror → −1
        if n == 0 {
            break;
        }
        outp.write_all(&buf[..n])?; // C: short fwrite → −1
    }
    outp.flush()?; // C: fclose(out) != 0 → bad
    // (a close(2)-level error after a clean flush is not observable
    // through std::fs::File — DEVIATION)
    Ok(())
}

// ── applying a preset ───────────────────────────────────────────────────

/// `$HOME/.theshrc`, or `/.theshrc` when $HOME is unset/empty (C does
/// NOT fall back to the passwd entry here).
fn user_rc_path() -> String {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => format!("{}/.theshrc", h.to_string_lossy()),
        _ => "/.theshrc".to_string(),
    }
}

/// `$HOME/.config/thesh/presets`, or `/.config/thesh/presets` when
/// $HOME is unset/empty.
fn user_preset_dir() -> String {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => format!("{}/.config/thesh/presets", h.to_string_lossy()),
        _ => "/.config/thesh/presets".to_string(),
    }
}

/// Copy the preset over the requested rc file(s); re-source them so the
/// shell picks the change up right away. Returns (rc, wrote_system,
/// wrote_user) — 0 on full success.
fn preset_apply(src: &str, scope: i32) -> (i32, bool, bool) {
    let mut rc = 0;
    let mut ws = false;
    let mut wu = false;

    if scope == SCOPE_SYSTEM || scope == SCOPE_BOTH {
        match copy_file(src, "/etc/theshrc") {
            Ok(()) => ws = true,
            Err(e) => {
                eprintln!(
                    "{THESH_NAME}: presets: cannot write /etc/theshrc: {}",
                    strerror(&e)
                );
                rc = 1;
            }
        }
    }
    if scope == SCOPE_USER || scope == SCOPE_BOTH {
        let rcpath = user_rc_path();
        match copy_file(src, &rcpath) {
            Ok(()) => wu = true,
            Err(e) => {
                eprintln!("{THESH_NAME}: presets: cannot write {rcpath}: {}", strerror(&e));
                rc = 1;
            }
        }
    }

    if ws {
        crate::exec::run_file_silent("/etc/theshrc");
    }
    if wu {
        // C recomputes rcpath from getenv("HOME") again — same value
        crate::exec::run_file_silent(&user_rc_path());
    }
    (rc, ws, wu)
}

fn apply_flow(
    pl: &PresetList,
    idx: usize,
    interactive: bool,
    prompt: &mut dyn FnMut(&str) -> Option<String>,
) -> i32 {
    let mut scope = SCOPE_USER;
    if interactive {
        // ESC / Ctrl+C / Ctrl+D at the prompt aborts the command
        let Some(answer) = read_line(SCOPE_PROMPT, 64, prompt) else { return 0 };
        scope = parse_scope(&answer);
    }

    let (rc, ws, wu) = preset_apply(&pl.v[idx].path, scope);
    if ws || wu {
        out_fmt(format_args!("preset '{}' applied", pl.v[idx].name));
        if ws && wu {
            out_fmt(format_args!(" (system + user)\n"));
        } else if ws {
            out_fmt(format_args!(" (system)\n"));
        } else {
            out_fmt(format_args!(" (user)\n"));
        }
    }
    rc
}

// ── commands ────────────────────────────────────────────────────────────

fn cmd_presets_impl(
    argc: i32,
    argv: &[String],
    interactive: bool,
    prompt: &mut dyn FnMut(&str) -> Option<String>,
) -> i32 {
    let pl = preset_collect();

    // `presets NAME` applies directly (system scope is asked on a tty).
    if argc > 1 {
        // DEVIATION: a caller passing argc > 1 with a short argv gets
        // "" where C would read past argv (UB).
        let name = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        let idx = preset_pick(&pl, name);
        if idx < 0 {
            eprintln!("{THESH_NAME}: presets: no preset '{name}'");
            return 1;
        }
        return apply_flow(&pl, idx as usize, interactive, prompt);
    }

    if pl.v.is_empty() {
        out_fmt(format_args!(
            "no presets found (~/.config/thesh/presets, /etc/thesh/presets)\n"
        ));
        return 0;
    }

    // Non-interactive: just list available preset names.
    if !interactive {
        for e in &pl.v {
            out_fmt(format_args!("{}\n", e.name));
        }
        return 0;
    }

    out_fmt(format_args!("select preset :\n\n"));
    for (i, e) in pl.v.iter().enumerate() {
        out_fmt(format_args!("{} : {}\n", i + 1, e.name));
    }

    let Some(answer) = read_line("\nselect preset : ", LINE_MAX_CP, prompt) else {
        return 0;
    };
    let idx = preset_pick(&pl, &answer);
    if idx < 0 {
        out_fmt(format_args!("no such preset\n"));
        return 1;
    }
    apply_flow(&pl, idx as usize, interactive, prompt)
}

/// `presets [NAME]` — list/apply preset configs.
pub fn cmd_presets(argc: i32, argv: &[String]) -> i32 {
    let mut prompt = crate::editor::prompt_line;
    cmd_presets_impl(argc, argv, stdin_is_tty(), &mut prompt)
}

// ── savepreset ──────────────────────────────────────────────────────────

/// C's path assembly in `cmd_savepreset` (non-custom locations): the
/// name must be non-empty, free of '/' and of ".." anywhere (strstr,
/// not path-aware — `a..b` is rejected too), and
/// `strlen(dir) + strlen(name) + 2 <= PATH_MAX`. C's exact errors are
/// printed; `None` means "return 1".
fn preset_target_path(dir: &str, name: &str) -> Option<String> {
    if name.is_empty() {
        eprintln!("{THESH_NAME}: savepreset: no preset name given");
        return None;
    }
    if name.contains('/') || name.contains("..") {
        eprintln!("{THESH_NAME}: savepreset: invalid preset name '{name}'");
        return None;
    }
    let dl = dir.len();
    let bl = name.len();
    if dl + bl + 2 > PATH_MAX {
        eprintln!("{THESH_NAME}: savepreset: preset path too long");
        return None;
    }
    Some(format!("{dir}/{name}"))
}

fn savepreset_impl(
    argc: i32,
    argv: &[String],
    interactive: bool,
    prompt: &mut dyn FnMut(&str) -> Option<String>,
) -> i32 {
    // C: name = (argc > 1) ? argv[1] : NULL — an empty argv[1] is a
    // non-NULL pointer and reaches the "no preset name" check below.
    let name = if argc > 1 {
        Some(argv.get(1).map(|s| s.as_str()).unwrap_or(""))
    } else {
        None
    };
    let mut loc = LOC_USER;

    if interactive {
        let Some(answer) = read_line(SAVE_AT_PROMPT, 128, prompt) else {
            return 0; // ESC: abort
        };
        loc = parse_loc(&answer);
    }

    let path: String;
    if loc == LOC_CUSTOM {
        let Some(buf) = read_line("path : ", PATH_MAX, prompt) else {
            return 0; // ESC: abort
        };
        if buf.is_empty() {
            eprintln!("{THESH_NAME}: savepreset: no path given");
            return 1;
        }
        path = truncate_bytes(buf.as_bytes(), PATH_MAX - 1);
    } else {
        let dir = if loc == LOC_SYSTEM {
            "/etc/thesh/presets".to_string()
        } else {
            user_preset_dir()
        };

        let name_buf = match name {
            Some(n) => truncate_bytes(n.as_bytes(), PRESET_NAME_MAX - 1),
            None => match read_line("preset name : ", PRESET_NAME_MAX, prompt) {
                None => return 0, // ESC: abort
                Some(s) => s,
            },
        };
        let Some(p) = preset_target_path(&dir, &name_buf) else { return 1 };
        path = p;
    }

    mkdir_parent(&path);

    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{THESH_NAME}: savepreset: cannot write {path}: {}", strerror(&e));
            return 1;
        }
    };
    // C ignores every write error here (ferror is never checked):
    let _ = crate::config::dump_current(&mut f);
    let _ = writeln!(f); // C: fputc('\n', f)
    let _ = crate::exec::dump_aliases(&mut f);
    let _ = crate::bind::dump(&mut f);
    drop(f); // fclose — result ignored, like C

    out_fmt(format_args!("preset saved to {path}\n"));
    0
}

/// `savepreset [NAME]` — export the effective config as a preset.
pub fn cmd_savepreset(argc: i32, argv: &[String]) -> i32 {
    let mut prompt = crate::editor::prompt_line;
    savepreset_impl(argc, argv, stdin_is_tty(), &mut prompt)
}

// ── misc ────────────────────────────────────────────────────────────────

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sets $HOME for the duration and restores the previous value (even
    /// on panic) while still holding the shared test lock — the restore
    /// runs in Drop::drop, before the lock field is dropped.
    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
        _g: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn set(v: &str) -> Self {
            let g = crate::util::test_lock();
            let saved = std::env::var_os("HOME");
            std::env::set_var("HOME", v);
            HomeGuard { saved, _g: g }
        }
        fn unset() -> Self {
            let g = crate::util::test_lock();
            let saved = std::env::var_os("HOME");
            std::env::remove_var("HOME");
            HomeGuard { saved, _g: g }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("thesh-presets-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            let p = self.dir.join(name);
            std::fs::write(&p, "x").unwrap();
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn never(_p: &str) -> Option<String> {
        panic!("prompt must not be called")
    }

    // ── prompt helpers ──────────────────────────────────────────────────

    #[test]
    fn read_line_strips_crlf_and_propagates_cancel() {
        let mut ok = |_p: &str| Some("hello\r\n".to_string());
        assert_eq!(read_line("p", 64, &mut ok), Some("hello".to_string()));
        let mut crlf = |_p: &str| Some("x\r\n\r\n".to_string());
        assert_eq!(read_line("p", 64, &mut crlf), Some("x".to_string()));
        let mut plain = |_p: &str| Some("abc".to_string());
        assert_eq!(read_line("p", 64, &mut plain), Some("abc".to_string()));
        let mut cancel = |_p: &str| -> Option<String> { None };
        assert_eq!(read_line("p", 64, &mut cancel), None);
    }

    #[test]
    fn read_line_truncates_like_the_c_buffer() {
        // C: char buf[64] → prompt_line writes at most 63 bytes
        let long = "a".repeat(200);
        let mut f = |_p: &str| Some(long.clone());
        assert_eq!(read_line("p", 64, &mut f).unwrap().len(), 63);
    }

    #[test]
    fn truncate_bytes_caps_and_keeps_utf8_valid() {
        assert_eq!(truncate_bytes(b"hello", 3), "hel");
        assert_eq!(truncate_bytes(b"hi", 10), "hi");
        // 255th byte would split a 2-byte char → the partial tail is
        // dropped (DEVIATION from C, which keeps the broken byte)
        let e = "é".repeat(200); // 400 bytes
        let t = truncate_bytes(e.as_bytes(), 255);
        assert_eq!(t.as_bytes().len(), 254);
        assert_eq!(t.chars().count(), 127);
    }

    // ── input parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_scope_rules() {
        assert_eq!(parse_scope("1"), SCOPE_SYSTEM);
        assert_eq!(parse_scope("system"), SCOPE_SYSTEM);
        assert_eq!(parse_scope("SYSTEM"), SCOPE_SYSTEM);
        assert_eq!(parse_scope("  1"), SCOPE_SYSTEM); // leading ws skipped
        assert_eq!(parse_scope("3"), SCOPE_BOTH);
        assert_eq!(parse_scope("both"), SCOPE_BOTH);
        assert_eq!(parse_scope("ALL"), SCOPE_BOTH);
        assert_eq!(parse_scope("2"), SCOPE_USER);
        assert_eq!(parse_scope("user"), SCOPE_USER);
        assert_eq!(parse_scope(""), SCOPE_USER);
        assert_eq!(parse_scope("banana"), SCOPE_USER);
        // C does NOT trim trailing whitespace (strcmp on the raw tail)
        assert_eq!(parse_scope("1 "), SCOPE_USER);
        assert_eq!(parse_scope("both "), SCOPE_USER);
    }

    #[test]
    fn parse_loc_rules() {
        assert_eq!(parse_loc("1"), LOC_SYSTEM);
        assert_eq!(parse_loc("system"), LOC_SYSTEM);
        assert_eq!(parse_loc("SYSTEM"), LOC_SYSTEM);
        assert_eq!(parse_loc("3"), LOC_CUSTOM);
        assert_eq!(parse_loc("custom"), LOC_CUSTOM);
        assert_eq!(parse_loc("CUSTOM"), LOC_CUSTOM);
        assert_eq!(parse_loc("2"), LOC_USER);
        assert_eq!(parse_loc("user"), LOC_USER);
        assert_eq!(parse_loc(""), LOC_USER);
        assert_eq!(parse_loc("junk"), LOC_USER);
        // no ws trimming at all here (unlike parse_scope)
        assert_eq!(parse_loc(" 1"), LOC_USER);
        assert_eq!(parse_loc("1 "), LOC_USER);
    }

    #[test]
    fn preset_pick_accepts_numbers_and_exact_names() {
        let pl = PresetList {
            v: vec![
                PresetEntry { name: "alpha".into(), path: "/a".into() },
                PresetEntry { name: "beta".into(), path: "/b".into() },
                PresetEntry { name: "2x".into(), path: "/2x".into() },
            ],
        };
        assert_eq!(preset_pick(&pl, "1"), 0);
        assert_eq!(preset_pick(&pl, "3"), 2);
        assert_eq!(preset_pick(&pl, " 2"), 1); // leading ws skipped
        assert_eq!(preset_pick(&pl, "0"), -1); // out of range → falls through
        assert_eq!(preset_pick(&pl, "4"), -1);
        assert_eq!(preset_pick(&pl, ""), -1);
        assert_eq!(preset_pick(&pl, "   "), -1);
        assert_eq!(preset_pick(&pl, "alpha"), 0);
        assert_eq!(preset_pick(&pl, "ALPHA"), 0); // case-insensitive
        assert_eq!(preset_pick(&pl, "gamma"), -1);
        // quirk: a junk number falls through to the name match
        assert_eq!(preset_pick(&pl, "2x"), 2);
        // no trailing trim anywhere: "2 " ≠ any name
        assert_eq!(preset_pick(&pl, "2 "), -1);
        assert_eq!(preset_pick(&pl, "1x"), -1);
    }

    // ── collecting ──────────────────────────────────────────────────────

    #[test]
    fn collect_dir_skips_hidden_swap_dirs_and_long_names() {
        let fx = Fixture::new("filter");
        fx.file("visible");
        fx.file(".hidden");
        fx.file("swap~");
        std::fs::create_dir(fx.dir.join("subdir")).unwrap();
        fx.file(&"n".repeat(128)); // >= PRESET_NAME_MAX → skipped
        fx.file(&"m".repeat(127)); // 127 → kept
        // symlink to a regular file is followed (C: stat) and kept
        std::os::unix::fs::symlink(fx.dir.join("visible"), fx.dir.join("link")).unwrap();

        let mut pl = PresetList { v: Vec::new() };
        collect_dir(fx.dir.to_str().unwrap(), &mut pl);
        let mut names: Vec<&str> = pl.v.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        let long = "m".repeat(127); // 127 bytes → kept
        let mut expect = vec!["link", "visible", long.as_str()];
        expect.sort_unstable();
        assert_eq!(names, expect);

        // missing dir → nothing (C: opendir fails → return)
        let mut pl2 = PresetList { v: Vec::new() };
        collect_dir("/nonexistent/thesh/presets", &mut pl2);
        assert!(pl2.v.is_empty());
    }

    #[test]
    fn collect_dir_dedups_across_dirs_first_wins() {
        let d1 = Fixture::new("dedup1");
        let d2 = Fixture::new("dedup2");
        d1.file("dup");
        d1.file("only1");
        d2.file("dup"); // same name → shadowed by the first dir
        d2.file("only2");
        let mut pl = PresetList { v: Vec::new() };
        collect_dir(d1.dir.to_str().unwrap(), &mut pl);
        collect_dir(d2.dir.to_str().unwrap(), &mut pl);
        let names: Vec<&str> = pl.v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"dup"));
        assert!(names.contains(&"only1"));
        assert!(names.contains(&"only2"));
        assert_eq!(names.iter().filter(|n| **n == "dup").count(), 1);
        let dup = pl.v.iter().find(|e| e.name == "dup").unwrap();
        assert!(dup.path.starts_with(d1.dir.to_str().unwrap()));
    }

    #[test]
    fn collect_dir_stops_at_preset_max() {
        let fx = Fixture::new("cap");
        for i in 0..(PRESET_MAX + 2) {
            fx.file(&format!("p{i:03}"));
        }
        let mut pl = PresetList { v: Vec::new() };
        collect_dir(fx.dir.to_str().unwrap(), &mut pl);
        assert_eq!(pl.v.len(), PRESET_MAX);
    }

    #[test]
    fn preset_collect_scans_home_first() {
        let fx = Fixture::new("home");
        let pdir = fx.dir.join(".config/thesh/presets");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("userpreset"), "x").unwrap();
        let _home = HomeGuard::set(fx.dir.to_str().unwrap());
        let pl = preset_collect();
        let e = pl.v.iter().find(|e| e.name == "userpreset").expect("user preset found");
        assert!(e.path.starts_with(fx.dir.to_str().unwrap()));

        // $HOME unset → the user dir is skipped entirely (no passwd
        // fallback in preset_collect)
        drop(_home);
        let _home = HomeGuard::unset();
        let pl2 = preset_collect();
        assert!(!pl2.v.iter().any(|e| e.name == "userpreset"));
    }

    // ── filesystem helpers ──────────────────────────────────────────────

    #[test]
    fn mkdir_p_creates_components_idempotently() {
        let fx = Fixture::new("mkdir");
        let deep = format!("{}/a/b/c", fx.dir.display());
        assert_eq!(mkdir_p(&deep), 0);
        assert!(fx.dir.join("a/b/c").is_dir());
        assert_eq!(mkdir_p(&deep), 0); // EEXIST → still 0
        assert_eq!(mkdir_p(&format!("{deep}/")), 0); // trailing slashes
        assert_eq!(mkdir_p(""), 0);
        assert_eq!(mkdir_p("/"), 0);
        // a regular file where a component should be → −1 (ENOTDIR)
        let f = fx.file("plain");
        assert_eq!(mkdir_p(&format!("{}/x", f.display())), -1);
    }

    #[test]
    fn mkdir_parent_creates_only_the_parent() {
        let fx = Fixture::new("parent");
        let target = format!("{}/sub/file", fx.dir.display());
        mkdir_parent(&target);
        assert!(fx.dir.join("sub").is_dir());
        assert!(!fx.dir.join("sub/file").exists());
        // no slash → no-op (C: strrchr fails)
        mkdir_parent("just-a-name");
        assert!(!fx.dir.join("just-a-name").exists());
        // slash at position 0 → no-op (C: slash == tmp)
        mkdir_parent("/root-level-file");
    }

    // ── savepreset path rules ───────────────────────────────────────────

    #[test]
    fn preset_target_path_rules() {
        assert_eq!(
            preset_target_path("/etc/thesh/presets", "my-preset"),
            Some("/etc/thesh/presets/my-preset".to_string())
        );
        assert_eq!(preset_target_path("/d", ""), None); // no name given
        assert_eq!(preset_target_path("/d", "a/b"), None); // '/' rejected
        assert_eq!(preset_target_path("/d", ".."), None);
        assert_eq!(preset_target_path("/d", "a..b"), None); // strstr quirk
        // dl + bl + 2 == PATH_MAX still fits (4095 chars + NUL); one
        // byte more is rejected
        let dir = "d".repeat(PATH_MAX - 6); // dl = 4090
        assert_eq!(dir.len(), 4090);
        assert!(preset_target_path(&dir, "abcd").is_some()); // 4090+4+2
        assert_eq!(preset_target_path(&dir, "abcde"), None); // 4097
    }

    #[test]
    fn user_paths_honor_home() {
        let fx = Fixture::new("rcpath");
        {
            let _home = HomeGuard::set(fx.dir.to_str().unwrap());
            assert_eq!(user_rc_path(), format!("{}/.theshrc", fx.dir.display()));
            assert_eq!(
                user_preset_dir(),
                format!("{}/.config/thesh/presets", fx.dir.display())
            );
        } // HOME restored here
        {
            // empty $HOME → the "/..." fallbacks (C: `home && *home`)
            let _home = HomeGuard::set("");
            assert_eq!(user_rc_path(), "/.theshrc");
            assert_eq!(user_preset_dir(), "/.config/thesh/presets");
        }
        {
            let _home = HomeGuard::unset();
            assert_eq!(user_rc_path(), "/.theshrc");
            assert_eq!(user_preset_dir(), "/.config/thesh/presets");
        }
    }

    // ── command flows (non-interactive / injected prompts only) ─────────

    #[test]
    fn savepreset_cancel_returns_zero() {
        // ESC at the location prompt
        let mut f = |_p: &str| -> Option<String> { None };
        assert_eq!(savepreset_impl(1, &["savepreset".to_string()], true, &mut f), 0);

        // custom location: answer "3", then cancel at the path prompt
        let mut answers: Vec<Option<String>> = vec![Some("3".to_string()), None];
        let mut seen: Vec<String> = Vec::new();
        {
            let mut f = |p: &str| {
                seen.push(p.to_string());
                answers.remove(0)
            };
            assert_eq!(savepreset_impl(1, &["savepreset".to_string()], true, &mut f), 0);
        }
        assert_eq!(seen, vec![SAVE_AT_PROMPT.to_string(), "path : ".to_string()]);
    }

    #[test]
    fn savepreset_bad_names_fail_before_touching_disk() {
        let fx = Fixture::new("savebad");
        let _home = HomeGuard::set(fx.dir.to_str().unwrap());
        let mut f = never;
        for bad in ["", "a/b", "..", "a..b"] {
            let argv = ["savepreset".to_string(), bad.to_string()];
            assert_eq!(
                savepreset_impl(2, &argv, false, &mut f),
                1,
                "name {bad:?} must be rejected"
            );
        }
        assert!(!fx.dir.join(".config").exists(), "nothing may be created");
    }

    #[test]
    fn savepreset_write_failure_returns_one() {
        // $HOME points at a regular file → mkdir/fopen fails with C's
        // message and returns 1 long before dump_aliases (todo!()) runs
        let fx = Fixture::new("savefail");
        let blocker = fx.file("not-a-dir");
        let _home = HomeGuard::set(blocker.to_str().unwrap());
        let argv = ["savepreset".to_string(), "nm".to_string()];
        let mut f = never;
        assert_eq!(savepreset_impl(2, &argv, false, &mut f), 1);
    }

    #[test]
    fn cmd_presets_unknown_name_returns_1() {
        let fx = Fixture::new("empty");
        let _home = HomeGuard::set(fx.dir.to_str().unwrap());
        let argv = ["presets".to_string(), "no-such-preset-zzz-42".to_string()];
        let mut f = never;
        assert_eq!(cmd_presets_impl(2, &argv, false, &mut f), 1);
    }
}
