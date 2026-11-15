//! Port of `src/exec.c` — the tokenizer, builtins, pipelines,
//! redirections, aliases and command execution.
//!
//! Deviations (all documented at their sites): input lines are `&str`
//! (UTF-8) rather than raw bytes — non-UTF-8 script lines and env values
//! are converted lossily; PATH/path buffers are unbounded (C truncated at
//! PATH_MAX); the redirection-only line that crashed C (`strchr(NULL)`)
//! is a no-op, matching the patched oracle.

use crate::{config, dict, history, mux, presets, spell, term, util};
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

extern "C" {
    // libc doesn't export `environ` on all targets; declare it here.
    static environ: *mut *mut libc::c_char;
}

pub const ALIAS_MAX: usize = 128;

// ── Aliases ────────────────────────────────────────────────────────────

struct Alias {
    name: String,
    value: String,
}

static ALIASES: Mutex<Vec<Alias>> = Mutex::new(Vec::new());

fn aliases() -> std::sync::MutexGuard<'static, Vec<Alias>> {
    ALIASES.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `alias_lookup()` (clone: the C pointer is only ever read
/// transiently before the list can change).
pub fn alias_lookup(name: &str) -> Option<String> {
    aliases().iter().find(|a| a.name == name).map(|a| a.value.clone())
}

/// Port of `alias_set()` — capped at ALIAS_MAX (128), silently.
pub fn alias_set(name: &str, value: &str) {
    let mut v = aliases();
    if let Some(a) = v.iter_mut().find(|a| a.name == name) {
        a.value = value.to_string();
        return;
    }
    if v.len() < ALIAS_MAX {
        v.push(Alias { name: name.to_string(), value: value.to_string() });
    }
}

/// Port of `alias_unset()`.
pub fn alias_unset(name: &str) {
    let mut v = aliases();
    if let Some(i) = v.iter().position(|a| a.name == name) {
        v.remove(i);
    }
}

/// Port of `print_aliases()` — `name=value\n` per line.
pub fn print_aliases() {
    let v = aliases();
    for a in v.iter() {
        util::out_fmt(format_args!("{}={}\n", a.name, a.value));
    }
}

/// Port of `dump_aliases()` — config-style `alias name='value'` lines.
pub fn dump_aliases(f: &mut dyn Write) -> std::io::Result<()> {
    let v = aliases();
    for a in v.iter() {
        write!(f, "alias {}=", a.name)?;
        if !a.value.contains('\'') {
            writeln!(f, "'{}'", a.value)?;
        } else {
            writeln!(f, "\"{}\"", a.value)?;
        }
    }
    Ok(())
}

// ── Builtins ───────────────────────────────────────────────────────────

/// C's `builtin_list[]` in exact order (NULL terminator excluded).
pub static BUILTIN_LIST: &[&str] = &[
    "cd", "pwd", "echo", "export", "unset", "alias", "unalias",
    "history", "source", "exit", "type", "set", "clearhistory", "hash",
    "presets", "savepreset",
    "split-window-vertical", "split-window-horizontal", "close-pane",
    "focus-left", "focus-right", "focus-up", "focus-down",
    "focus-next", "focus-prev", "panes",
    "kill-curent-window", "kill-current-window", "kill-all-windows",
    "move-to-right-window", "move-to-left-window",
    "move-to-up-window", "move-to-down-window",
];

/// Port of `builtin_names()`.
pub fn builtin_names() -> &'static [&'static str] {
    BUILTIN_LIST
}

/// Port of `is_builtin()`.
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_LIST.contains(&name)
}

// ── Tokenizer ──────────────────────────────────────────────────────────

/// `quoted` marks a word that had any character pass through a quote or
/// backslash — such words are never globbed (grep 'foo*' stays literal).
struct Word {
    b: String,
    quoted: bool,
}

impl Word {
    fn new() -> Self {
        Word { b: String::new(), quoted: false }
    }
    fn add(&mut self, c: char) {
        self.b.push(c);
    }
    fn adds(&mut self, s: &str) {
        self.b.push_str(s);
    }
}

fn glob_has_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Expand one glob pattern via glob(3) — sorted (flags = 0), no match
/// keeps the word literal (C's else branch), quoted words never arrive.
fn glob_splice(pat: &str, args: &mut Vec<String>) {
    let Ok(cpat) = std::ffi::CString::new(pat) else {
        args.push(pat.to_string()); // interior NUL: C would truncate the
        return; // pattern; keep it literal instead (documented)
    };
    unsafe {
        let mut g: libc::glob_t = std::mem::zeroed();
        let r = libc::glob(cpat.as_ptr(), 0, None, &mut g);
        if r == 0 && g.gl_pathc > 0 {
            for i in 0..g.gl_pathc {
                let p = *g.gl_pathv.add(i);
                if !p.is_null() {
                    let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                    args.push(s);
                }
            }
        } else {
            args.push(pat.to_string());
        }
        libc::globfree(&mut g);
    }
}

/// Env lookup for expansions. DEVIATION: non-UTF-8 values (C passed raw
/// bytes) are converted lossily.
fn env_str(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Port of `expand_var()`: returns the cursor after the expansion.
/// Quirks preserved: `$?`, 255-byte name caps (with the remainder
/// re-parsed as literal text), `${name}` accepts ANY bytes up to `}`,
/// and a lone `$` consumes itself and adds nothing.
fn expand_var<'a>(w: &mut Word, rest: &'a str) -> &'a str {
    let bytes = rest.as_bytes();
    debug_assert_eq!(bytes[0], b'$');
    let mut p = &rest[1..];
    if p.starts_with('?') {
        w.adds(&util::shell_status().to_string());
        return &p[1..];
    }
    if let Some(after) = p.strip_prefix('{') {
        let mut name = Vec::new();
        let pb = after.as_bytes();
        let mut i = 0;
        while i < pb.len() && pb[i] != b'}' && name.len() < 255 {
            name.push(pb[i]);
            i += 1;
        }
        p = &after[i..];
        if p.starts_with('}') {
            p = &p[1..];
        }
        let name = String::from_utf8_lossy(&name).into_owned();
        if let Some(v) = env_str(&name) {
            w.adds(&v);
        }
        return p;
    }
    let mut name = String::new();
    for c in p.chars() {
        if (c.is_ascii_alphanumeric() || c == '_') && name.len() < 255 {
            name.push(c);
            p = &p[c.len_utf8()..];
        } else {
            break;
        }
    }
    if !name.is_empty() {
        if let Some(v) = env_str(&name) {
            w.adds(&v);
        }
    }
    p
}

/// Port of `tokenize()`.
///
/// The C loop walks bytes; all syntax characters are ASCII, so a slice
/// cursor gives identical behavior (multibyte chars are literal in every
/// context). NUL terminates tokenization, like C's `while (*p)`.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut w = Word::new();
    let mut started = false;
    let mut p = line;

    loop {
        let Some(&c0) = p.as_bytes().first() else { break };
        if c0 == 0 {
            break; // C: *p ends the loop
        }
        if c0.is_ascii_whitespace() {
            // FLUSH: push only when a word started, always reset state
            if started {
                let word = std::mem::take(&mut w.b);
                if glob_has_meta(&word) && !w.quoted {
                    glob_splice(&word, &mut args);
                } else {
                    args.push(word);
                }
            }
            w.quoted = false;
            started = false;
            p = &p[1..];
            continue;
        }
        if c0 == b'#' && !started {
            break; // comment to end of line
        }
        started = true;

        if c0 == b'\'' {
            w.quoted = true;
            p = &p[1..];
            loop {
                let Some(&c) = p.as_bytes().first() else { break };
                if c == 0 {
                    break; // unterminated: C stops at NUL (end of line)
                }
                if c == b'\'' {
                    p = &p[1..];
                    break;
                }
                let ch = p.chars().next().unwrap();
                w.add(ch);
                p = &p[ch.len_utf8()..];
            }
            continue;
        }
        if c0 == b'"' {
            w.quoted = true;
            p = &p[1..];
            loop {
                let Some(&c) = p.as_bytes().first() else { break };
                if c == 0 {
                    break;
                }
                if c == b'"' {
                    p = &p[1..];
                    break;
                }
                if c == b'\\' && matches!(p.as_bytes().get(1), Some(&n) if n != 0) {
                    let esc = p.as_bytes()[1];
                    // C only unescapes " $ ` \ inside double quotes —
                    // anything else keeps the backslash literally
                    if matches!(esc, b'"' | b'$' | b'`' | b'\\') {
                        p = &p[1..];
                        let ch = p.chars().next().unwrap();
                        w.add(ch);
                        p = &p[ch.len_utf8()..];
                    } else {
                        w.add('\\');
                        p = &p[1..];
                    }
                    continue;
                }
                if c == b'$' {
                    p = expand_var(&mut w, p);
                    continue;
                }
                let ch = p.chars().next().unwrap();
                w.add(ch);
                p = &p[ch.len_utf8()..];
            }
            continue;
        }
        if c0 == b'\\' && matches!(p.as_bytes().get(1), Some(&n) if n != 0) {
            w.quoted = true;
            p = &p[1..];
            let ch = p.chars().next().unwrap();
            w.add(ch);
            p = &p[ch.len_utf8()..];
            continue;
        }
        if c0 == b'$' {
            p = expand_var(&mut w, p);
            continue;
        }
        if c0 == b'~' && w.b.is_empty() {
            if let Some(h) = env_str("HOME") {
                w.adds(&h);
            }
            p = &p[1..];
            continue;
        }
        let ch = p.chars().next().unwrap();
        w.add(ch);
        p = &p[ch.len_utf8()..];
    }

    // final FLUSH
    if started {
        let word = std::mem::take(&mut w.b);
        if glob_has_meta(&word) && !w.quoted {
            glob_splice(&word, &mut args);
        } else {
            args.push(word);
        }
    }
    args
}

// ── Command lookup ─────────────────────────────────────────────────────

/// Port of `path_of_command()`: first $PATH entry holding an executable
/// `cmd`. DEVIATION: C truncated the candidate at PATH_MAX; unbounded here.
fn path_of_command(cmd: &str) -> Option<String> {
    for seg in path_env()?.split(':') {
        if seg.is_empty() {
            continue;
        }
        let cand = format!("{seg}/{cmd}");
        if executable(&cand) {
            return Some(cand);
        }
    }
    None
}

/// access(X_OK) — same permission semantics as C (real uid, ACLs).
fn executable(p: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(p.as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

fn path_env() -> Option<String> {
    std::env::var("PATH").ok()
}

// ── Redirections ───────────────────────────────────────────────────────

/// Port of the `Redir` struct. mode: 0 read, 1 truncate, 2 append.
struct Redir {
    fd: i32,
    mode: i32,
    dupfd: i32,
    shared: i32,
    path: Option<String>,
}

/// C's `Redir redr[16]` stack cap — extra redirections are silently
/// dropped (tokens still consumed from argv), exactly like C.
const REDIR_CAP: usize = 16;

struct RedirTok {
    kind: i32,
    mode: i32,
    fd: i32,
    dupfd: i32,
    rest: Option<String>,
}

/// Port of `parse_redir_tok()`.
fn parse_redir_tok(tok: &str) -> Option<RedirTok> {
    let b = tok.as_bytes();
    let l = b.len();
    let digit = |c: u8| c.is_ascii_digit();

    if l >= 1 && digit(b[0]) && l >= 2 && b[1] == b'>' {
        let fd = (b[0] - b'0') as i32;
        if l >= 4 && b[2] == b'&' && digit(b[3]) {
            return Some(RedirTok { kind: 5, mode: 0, fd, dupfd: (b[3] - b'0') as i32, rest: None });
        }
        if l >= 3 && b[2] == b'>' {
            return Some(RedirTok { kind: 3, mode: 2, fd, dupfd: -1, rest: Some(tok[3..].to_string()) });
        }
        return Some(RedirTok { kind: 3, mode: 1, fd, dupfd: -1, rest: Some(tok[2..].to_string()) });
    }
    if l >= 2 && b[0] == b'&' && b[1] == b'>' {
        let (mode, at) = if l >= 3 && b[2] == b'>' { (2, 3) } else { (1, 2) };
        return Some(RedirTok { kind: 4, mode, fd: 1, dupfd: -1, rest: Some(tok[at..].to_string()) });
    }
    if l >= 1 && b[0] == b'>' {
        let (mode, at) = if l >= 2 && b[1] == b'>' { (2, 2) } else { (1, 1) };
        return Some(RedirTok { kind: 2, mode, fd: 1, dupfd: -1, rest: Some(tok[at..].to_string()) });
    }
    if l >= 1 && b[0] == b'<' {
        return Some(RedirTok { kind: 1, mode: 0, fd: 0, dupfd: -1, rest: Some(tok[1..].to_string()) });
    }
    None
}

/// Port of `collect_redirs()`: filter redir tokens out of argv into a
/// clean arg list + redir actions. `None` on syntax error (message
/// printed like C).
fn collect_redirs(args: &[String]) -> Option<(Vec<String>, Vec<Redir>)> {
    let mut clean: Vec<String> = Vec::new();
    let mut redr: Vec<Redir> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let Some(t) = parse_redir_tok(&args[i]) else {
            clean.push(args[i].clone());
            i += 1;
            continue;
        };
        if t.kind == 5 {
            if redr.len() < REDIR_CAP {
                redr.push(Redir { fd: t.fd, mode: 0, dupfd: t.dupfd, shared: 0, path: None });
            }
            i += 1;
            continue;
        }
        let rest_empty = t.rest.as_deref().map_or(true, |r| r.is_empty());
        let path = if !rest_empty {
            t.rest
        } else if i + 1 < args.len() {
            i += 1;
            Some(args[i].clone())
        } else {
            None
        };
        let Some(path) = path else {
            eprintln!("thesh: parse error: redirection with no file");
            return None;
        };
        let nred = if t.kind == 4 { 2 } else { 1 };
        for k in 0..nred {
            if redr.len() >= REDIR_CAP {
                break; // C: `nr < capr` — silently dropped
            }
            let tfd = if t.kind == 1 {
                0
            } else if t.kind == 3 || t.kind == 2 {
                t.fd
            } else if k == 1 {
                2
            } else {
                1
            };
            redr.push(Redir {
                fd: tfd,
                mode: t.mode,
                dupfd: -1,
                shared: if t.kind == 4 { k as i32 } else { 0 },
                path: Some(path.clone()),
            });
        }
        i += 1;
    }
    Some((clean, redr))
}

/// Port of `apply_redirs()` — runs in the child (or saved/restored
/// around a builtin). Returns false on failure with C's messages.
fn apply_redirs(redr: &[Redir]) -> bool {
    for (i, r) in redr.iter().enumerate() {
        let nfd;
        if r.dupfd >= 0 {
            nfd = unsafe { libc::dup(r.dupfd) };
        } else if r.shared != 0 && i > 0 {
            nfd = unsafe { libc::dup(redr[i - 1].fd) };
        } else {
            let fl = if r.mode == 0 {
                libc::O_RDONLY
            } else if r.mode == 2 {
                libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT
            } else {
                libc::O_WRONLY | libc::O_TRUNC | libc::O_CREAT
            };
            let p = cstr(r.path.as_deref().unwrap_or(""));
            nfd = unsafe { libc::open(p.as_ptr(), fl, 0o644) };
            if nfd < 0 {
                eprintln!("thesh: {}: {}", r.path.as_deref().unwrap_or(""), errno_str());
                return false;
            }
        }
        if nfd < 0 || unsafe { libc::dup2(nfd, r.fd) } < 0 {
            eprintln!("thesh: dup/dup2: {}", errno_str());
            if nfd >= 0 {
                unsafe { libc::close(nfd) };
            }
            return false;
        }
        if nfd != r.fd {
            unsafe { libc::close(nfd) };
        }
    }
    true
}

/// strerror-style text — Rust's `io::Error` Display would append
/// `" (os error N)"`, which C never emits.
fn errno_str() -> String {
    let e = std::io::Error::last_os_error();
    let code = e.raw_os_error().unwrap_or(0);
    unsafe {
        let p = libc::strerror(code);
        if p.is_null() {
            return e.to_string();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// ── Fork/exec helpers ──────────────────────────────────────────────────

fn flush_stdout() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

fn flush_stderr() {
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
}

fn reset_child_signals() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

fn execvp_argv(argv: &[String]) {
    let cargs: Vec<std::ffi::CString> = argv.iter().map(|a| cstr(a)).collect();
    let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()) };
}

/// Port of `run_child()`: fork, apply redirs, execvp, wait.
fn run_child(argv: &[String], redr: &[Redir]) {
    flush_stdout();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("fork: {}", errno_str()); // C: perror("fork")
        util::set_shell_status(1);
        return;
    }
    if pid == 0 {
        reset_child_signals();
        if !apply_redirs(redr) {
            unsafe { libc::_exit(1) };
        }
        execvp_argv(argv);
        let err = errno_str();
        eprintln!("thesh: {}: {}", argv[0], err);
        unsafe { libc::_exit(127) };
    }
    let mut st: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        break;
    }
    if libc::WIFEXITED(st) {
        util::set_shell_status(libc::WEXITSTATUS(st));
    } else if libc::WIFSIGNALED(st) {
        util::set_shell_status(128 + libc::WTERMSIG(st));
    }
}

/// Port of `expand_aliases()` — rebuild the word list from the alias
/// value, guarded to 10 expansions like C.
fn expand_aliases(argv: Vec<String>) -> Vec<String> {
    let mut argv = argv;
    let mut guard = 0;
    while guard < 10 {
        guard += 1;
        let Some(a) = alias_lookup(&argv[0]) else { break };
        let mut nl = a;
        for arg in argv.iter().skip(1) {
            nl.push(' ');
            nl.push_str(arg);
        }
        argv = tokenize(&nl);
        if argv.is_empty() {
            break;
        }
    }
    argv
}

/// Port of `case_fold_command()`: lowercase ARGV[0] in place when the
/// lowercase form resolves to a known command. Only folds DOWN.
fn case_fold_command(cmd: &mut String) -> bool {
    if cmd.is_empty() || cmd.contains('/') {
        return false;
    }
    if !cmd.bytes().any(|b| b.is_ascii_uppercase()) {
        return false;
    }
    let lw: String = cmd.bytes().map(|b| (b as char).to_ascii_lowercase()).collect();
    if lw == *cmd {
        return false;
    }
    if is_builtin(&lw) || alias_lookup(&lw).is_some() || dict::has(&lw) {
        *cmd = lw;
        true
    } else {
        false
    }
}

/// C's `best_suggestion(clean[0])` = builtins first, then the dict.
fn best_suggestion_for(word: &str) -> Option<String> {
    let dict_names: Vec<String> = dict::with_all(|v| v.to_vec());
    spell::best_suggestion(
        word,
        BUILTIN_LIST.iter().copied().chain(dict_names.iter().map(|s| s.as_str())),
    )
    .map(|s| s.to_string())
}

/// C's `print_suggestion(clean[0])`.
fn print_suggestion(word: &str) {
    let dict_names: Vec<String> = dict::with_all(|v| v.to_vec());
    spell::print_suggestion_from(
        word,
        BUILTIN_LIST.iter().copied().chain(dict_names.iter().map(|s| s.as_str())),
    );
}

// ── Pipeline stages ────────────────────────────────────────────────────

/// Trim ASCII whitespace in place like C's xstrdup+trim in
/// split_pipeline/split_pipeline's PUSH macro; push when non-empty.
fn push_trimmed(out: &mut Vec<String>, seg: &mut Vec<u8>) {
    let mut s = 0;
    let mut e = seg.len();
    while s < e && seg[s].is_ascii_whitespace() {
        s += 1;
    }
    while e > s && seg[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    if e > s {
        out.push(String::from_utf8_lossy(&seg[s..e]).into_owned());
    }
    seg.clear();
}

/// Port of `split_pipeline()` (byte loop; cuts only happen at ASCII
/// bytes, so segments are always valid UTF-8).
fn split_pipeline(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut segs: Vec<String> = Vec::new();
    let mut seg: Vec<u8> = Vec::new();
    let mut q: u8 = 0;
    let mut i = 0;

    while i < b.len() {
        let ch = b[i];
        if q == 0 && (ch == b'\'' || ch == b'"') {
            q = ch;
            seg.push(ch);
            i += 1;
            continue;
        }
        if q != 0 && ch == q {
            q = 0;
            seg.push(ch);
            i += 1;
            continue;
        }
        if q == 0 && ch == b'\\' && i + 1 < b.len() {
            seg.push(ch);
            seg.push(b[i + 1]);
            i += 2;
            continue;
        }
        if q == 0 && ch == b'|' {
            push_trimmed(&mut segs, &mut seg);
            i += 1;
            continue;
        }
        seg.push(ch);
        i += 1;
    }
    push_trimmed(&mut segs, &mut seg);
    segs
}

/// Port of `pipeline_child()` — the child body for a pipeline stage.
/// Every path ends in `_exit`.
fn pipeline_child(seg: &str, in_fd: i32, out_fd: i32) -> ! {
    reset_child_signals();

    if in_fd >= 0 {
        if unsafe { libc::dup2(in_fd, libc::STDIN_FILENO) } < 0 {
            unsafe { libc::_exit(126) };
        }
        unsafe { libc::close(in_fd) };
    }
    if out_fd >= 0 {
        if unsafe { libc::dup2(out_fd, libc::STDOUT_FILENO) } < 0 {
            unsafe { libc::_exit(126) };
        }
        unsafe { libc::close(out_fd) };
    }

    let mut argv = tokenize(seg);
    if argv.is_empty() {
        unsafe { libc::_exit(0) };
    }
    if config::with(|c| c.hybrid) {
        case_fold_command(&mut argv[0]);
    }
    argv = expand_aliases(argv);
    if argv.is_empty() {
        unsafe { libc::_exit(0) };
    }
    if config::with(|c| c.hybrid) {
        case_fold_command(&mut argv[0]);
    }

    let Some((mut clean, redr)) = collect_redirs(&argv) else {
        unsafe { libc::_exit(2) }
    };
    // redirection-only segment: nothing to run (patched C matches)
    if clean.is_empty() {
        unsafe { libc::_exit(0) };
    }

    dict::refresh(path_env().as_deref());

    if !apply_redirs(&redr) {
        unsafe { libc::_exit(1) };
    }

    if run_builtin(&mut clean) {
        unsafe { libc::_exit(util::shell_status()) };
    }

    if clean[0].contains('/') {
        execvp_argv(&clean);
        let err = errno_str();
        eprintln!("thesh: {}: {}", clean[0], err);
        unsafe { libc::_exit(127) };
    }
    if !dict::has(&clean[0]) {
        dict::force_refresh(path_env().as_deref());
        if config::with(|c| c.hybrid) {
            case_fold_command(&mut clean[0]);
        }
        if !dict::has(&clean[0]) {
            let corrector = config::with(|c| c.corrector);
            let best = if corrector == 3 { None } else { best_suggestion_for(&clean[0]) };
            if corrector == 1 {
                match best {
                    Some(b) => clean[0] = b, // auto-correct, run below
                    None => {
                        eprintln!("thesh: no such command: '{}'", clean[0]);
                        unsafe { libc::_exit(127) };
                    }
                }
            } else if best.is_some() {
                print_suggestion(&clean[0]); // passive (consent too)
                unsafe { libc::_exit(127) };
            } else {
                eprintln!("thesh: no such command: '{}'", clean[0]);
                unsafe { libc::_exit(127) };
            }
        }
    }
    execvp_argv(&clean);
    let enoent = std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
    if enoent {
        print_suggestion(&clean[0]);
    } else {
        eprintln!("thesh: {}: {}", clean[0], errno_str());
    }
    unsafe { libc::_exit(127) }
}

/// Port of `run_pipeline()`.
fn run_pipeline(segs: &[String]) -> i32 {
    flush_stdout();

    let n = segs.len();
    let mut pids: Vec<libc::pid_t> = vec![0; n];
    let mut prev: i32 = -1;
    let mut fd: [i32; 2] = [-1, -1];

    let mut i = 0;
    while i < n {
        if i < n - 1 {
            let mut pipes = [0i32; 2];
            if unsafe { libc::pipe(pipes.as_mut_ptr()) } < 0 {
                eprintln!("pipe: {}", errno_str()); // C: perror("pipe")
                for j in 0..i {
                    unsafe { libc::waitpid(pids[j], std::ptr::null_mut(), 0) };
                }
                util::set_shell_status(1);
                return 1;
            }
            fd = pipes;
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            eprintln!("fork: {}", errno_str()); // C: perror("fork")
            if i < n - 1 {
                unsafe {
                    libc::close(fd[0]);
                    libc::close(fd[1]);
                }
            }
            for j in 0..i {
                unsafe { libc::waitpid(pids[j], std::ptr::null_mut(), 0) };
            }
            util::set_shell_status(1);
            return 1;
        }
        if pid == 0 {
            pipeline_child(&segs[i], prev, if i < n - 1 { fd[1] } else { -1 });
        }
        if prev >= 0 {
            unsafe { libc::close(prev) };
        }
        if i < n - 1 {
            unsafe { libc::close(fd[1]) };
            prev = fd[0];
        }
        pids[i] = pid;
        i += 1;
    }
    if prev >= 0 {
        unsafe { libc::close(prev) };
    }

    let mut st = 0;
    for (i, &pid) in pids.iter().enumerate() {
        let mut ws: libc::c_int = 0;
        loop {
            let r = unsafe { libc::waitpid(pid, &mut ws, 0) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
                continue;
            }
            break;
        }
        if i == n - 1 {
            if libc::WIFEXITED(ws) {
                st = libc::WEXITSTATUS(ws);
            } else if libc::WIFSIGNALED(ws) {
                st = 128 + libc::WTERMSIG(ws);
            }
        }
    }
    util::set_shell_status(st);
    st
}

// ── One command unit ───────────────────────────────────────────────────

/// fgets-like consent reader: at most 15 bytes, like C's
/// `fgets(ans, 16, stdin)`. None = EOF before any byte.
fn read_consent() -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let stdin = std::io::stdin();
    let mut h = stdin.lock();
    let mut b = [0u8; 1];
    while buf.len() < 15 {
        match h.read(&mut b) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(b[0]);
                if b[0] == b'\n' {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if buf.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Port of `exec_single()`.
fn exec_single(seg: &str) -> i32 {
    let mut argv = tokenize(seg);
    if argv.is_empty() {
        return 0;
    }
    if config::with(|c| c.hybrid) {
        case_fold_command(&mut argv[0]);
    }
    argv = expand_aliases(argv);
    if argv.is_empty() {
        return 0;
    }
    if config::with(|c| c.hybrid) {
        case_fold_command(&mut argv[0]);
    }

    let Some((mut clean, redr)) = collect_redirs(&argv) else {
        util::set_shell_status(2);
        return 2;
    };
    // redirection-only line (patched C matches): nothing to run
    if clean.is_empty() {
        util::set_shell_status(0);
        return 0;
    }

    dict::refresh(path_env().as_deref());

    let st;
    if is_builtin(&clean[0]) {
        if redr.is_empty() {
            run_builtin(&mut clean);
            st = util::shell_status();
        } else {
            let saved: Vec<i32> = (0..3).map(|i| unsafe { libc::dup(i) }).collect();
            if apply_redirs(&redr) {
                run_builtin(&mut clean);
                st = util::shell_status();
            } else {
                util::set_shell_status(1);
                st = 1;
            }
            for (i, &s) in saved.iter().enumerate() {
                if s >= 0 {
                    unsafe {
                        libc::dup2(s, i as i32);
                        libc::close(s);
                    }
                }
            }
        }
    } else if clean[0].contains('/') {
        if !executable(&clean[0]) {
            eprintln!("thesh: {}: {}", clean[0], errno_str());
            st = 127;
            util::set_shell_status(127);
        } else {
            run_child(&clean, &redr);
            st = util::shell_status();
        }
    } else if !dict::has(&clean[0]) {
        dict::force_refresh(path_env().as_deref());
        if config::with(|c| c.hybrid) {
            case_fold_command(&mut clean[0]);
        }
        if !dict::has(&clean[0]) {
            let corrector = config::with(|c| c.corrector);
            let best = if corrector == 3 { None } else { best_suggestion_for(&clean[0]) };
            if corrector == 1 && best.is_some() {
                clean[0] = best.unwrap(); // auto-correct
                run_child(&clean, &redr);
                st = util::shell_status();
            } else if corrector == 2 && best.is_some() {
                let best = best.unwrap();
                eprint!("thesh: '{}' not found. Use '{}'? [y/N] ", clean[0], best);
                flush_stderr();
                let ans = read_consent();
                let yes = ans.as_deref().map_or(false, |a| {
                    matches!(a.as_bytes().first(), Some(b'y') | Some(b'Y'))
                });
                if yes {
                    clean[0] = best;
                    run_child(&clean, &redr);
                    st = util::shell_status();
                } else {
                    eprintln!();
                    st = 127;
                    util::set_shell_status(127);
                }
            } else if best.is_some() {
                print_suggestion(&clean[0]);
                st = 127;
                util::set_shell_status(127);
            } else {
                eprintln!("thesh: no such command: '{}'", clean[0]);
                st = 127;
                util::set_shell_status(127);
            }
        } else {
            run_child(&clean, &redr);
            st = util::shell_status();
        }
    } else {
        run_child(&clean, &redr);
        st = util::shell_status();
    }
    st
}

/// Port of `exec_unit()`: one `|`-pipeline; brackets a foreground unit
/// in cooked mode while the mux is down.
fn exec_unit(unit: &str, bg: bool) -> i32 {
    let segs = split_pipeline(unit);
    if segs.is_empty() {
        return 0;
    }

    let mut raw_off = false;
    if !bg && !mux::active() {
        term::exit_raw();
        raw_off = true;
    }
    flush_stdout();

    let st = if segs.len() == 1 { exec_single(&segs[0]) } else { run_pipeline(&segs) };

    if raw_off {
        let _ = term::enter_raw();
    }
    st
}

/// Port of `reap_bg()`.
fn reap_bg() -> i32 {
    let mut st = 0;
    let mut ws: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(-1, &mut ws, libc::WNOHANG) };
        if r > 0 {
            if libc::WIFSIGNALED(ws) {
                st = 128 + libc::WTERMSIG(ws);
            }
            continue;
        }
        break;
    }
    st
}

// ── Public entry points ────────────────────────────────────────────────

/// Port of `exec_line()`.
pub fn exec_line(line: &str) -> i32 {
    // Config directives (rc files or typed live) are consumed here.
    if config::apply_line(line) {
        return 0;
    }

    let (units, ops) = split_ops(line);
    if units.is_empty() {
        return 0;
    }

    static BGJOBS: AtomicI32 = AtomicI32::new(0);
    let mut st = 0;

    for i in 0..units.len() {
        if ops[i] == 1 && st != 0 {
            continue; // && short-circuit
        }
        if ops[i] == 2 && st == 0 {
            continue; // || short-circuit
        }

        if ops[i] == 4 {
            // background
            term::exit_raw();
            flush_stdout();
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                eprintln!("fork: {}", errno_str()); // C: perror("fork")
                st = 1;
            } else if pid == 0 {
                reset_child_signals();
                let dn = unsafe { libc::open(cstr("/dev/null").as_ptr(), libc::O_RDONLY) };
                if dn >= 0 {
                    unsafe {
                        libc::dup2(dn, libc::STDIN_FILENO);
                        libc::close(dn);
                    }
                }
                unsafe { libc::_exit(exec_unit(&units[i], true)) };
            } else {
                let n = BGJOBS.fetch_add(1, Ordering::SeqCst) + 1;
                util::out_fmt(format_args!("[{}] {}\n", n, pid));
            }
            let _ = term::enter_raw();
            continue;
        }

        st = exec_unit(&units[i], false);
    }

    let _ = reap_bg();
    st
}

/// Port of `split_ops()`: split on top-level `;`, `&&`, `||` and `&`.
/// ops[i] is the operator preceding units[i]: 0 (first), 1 (`&&`),
/// 2 (`||`), 3 (`;`), 4 (`&` async) — pushed together with the unit,
/// exactly like C's PUSH macro.
fn split_ops(line: &str) -> (Vec<String>, Vec<i32>) {
    let b = line.as_bytes();
    let mut units: Vec<String> = Vec::new();
    let mut ops: Vec<i32> = Vec::new();
    let mut op: i32 = 0;
    let mut seg: Vec<u8> = Vec::new();
    let mut q: u8 = 0;
    let mut i = 0;

    // trim + push unit with its preceding op (C's PUSH macro)
    fn push_unit(units: &mut Vec<String>, ops: &mut Vec<i32>, op: i32, seg: &mut Vec<u8>) {
        let mut s = 0;
        let mut e = seg.len();
        while s < e && seg[s].is_ascii_whitespace() {
            s += 1;
        }
        while e > s && seg[e - 1].is_ascii_whitespace() {
            e -= 1;
        }
        if e > s {
            units.push(String::from_utf8_lossy(&seg[s..e]).into_owned());
            ops.push(op);
        }
        seg.clear();
    }

    while i < b.len() {
        let ch = b[i];
        if q == 0 && (ch == b'\'' || ch == b'"') {
            q = ch;
            seg.push(ch);
            i += 1;
            continue;
        }
        if q != 0 && ch == q {
            q = 0;
            seg.push(ch);
            i += 1;
            continue;
        }
        if q == 0 && ch == b'\\' && i + 1 < b.len() {
            seg.push(ch);
            seg.push(b[i + 1]);
            i += 2;
            continue;
        }
        if q == 0 {
            if i + 1 < b.len() && ch == b'&' && b[i + 1] == b'&' {
                push_unit(&mut units, &mut ops, op, &mut seg);
                op = 1;
                i += 2;
                continue;
            }
            if i + 1 < b.len() && ch == b'|' && b[i + 1] == b'|' {
                push_unit(&mut units, &mut ops, op, &mut seg);
                op = 2;
                i += 2;
                continue;
            }
            if ch == b';' {
                push_unit(&mut units, &mut ops, op, &mut seg);
                op = 3;
                i += 1;
                continue;
            }
            if i + 1 < b.len() && ch == b'&' && b[i + 1] == b'>' {
                seg.push(b'&');
                seg.push(b'>');
                i += 2;
                continue;
            }
            if i + 1 < b.len() && ch == b'>' && b[i + 1] == b'&' {
                seg.push(b'>');
                seg.push(b'&');
                i += 2;
                continue;
            }
            if ch == b'&' {
                push_unit(&mut units, &mut ops, op, &mut seg);
                op = 4;
                i += 1;
                continue;
            }
        }
        seg.push(ch);
        i += 1;
    }
    push_unit(&mut units, &mut ops, op, &mut seg);
    (units, ops)
}

/// Port of `run_file()`. Reads whole lines (getline semantics — script
/// lines may exceed 4096 bytes). DEVIATION: non-UTF-8 lines are
/// converted lossily (C passed raw bytes).
pub fn run_file(path: &str) -> i32 {
    let Ok(f) = std::fs::File::open(path) else { return -1 };
    let mut r = std::io::BufReader::new(f);
    use std::io::BufRead;
    loop {
        let mut line: Vec<u8> = Vec::new();
        match r.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break, // C: fgets error ends the loop
        }
        while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
            line.pop();
        }
        if line.first().map_or(true, |&c| c == b'#') {
            continue;
        }
        let line = String::from_utf8_lossy(&line).into_owned();
        exec_line(&line);
    }
    util::shell_status()
}

/// Port of `run_file_silent()`: re-source without emitting stdout.
pub fn run_file_silent(path: &str) -> i32 {
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    let devnull = unsafe { libc::open(cstr("/dev/null").as_ptr(), libc::O_WRONLY) };
    if saved >= 0 && devnull >= 0 {
        unsafe { libc::dup2(devnull, libc::STDOUT_FILENO) };
    }
    let rc = run_file(path);
    if devnull >= 0 {
        unsafe { libc::close(devnull) };
    }
    if saved >= 0 {
        unsafe {
            libc::dup2(saved, libc::STDOUT_FILENO);
            libc::close(saved);
        }
    }
    rc
}

// ── Builtin dispatch ───────────────────────────────────────────────────

/// Port of `run_builtin()`: returns true when the word was a builtin.
/// `clean` is the non-redirection arg list (mutated in place by `type`'s
/// case folding and the alias/expansion paths, exactly like C).
fn run_builtin(clean: &mut Vec<String>) -> bool {
    let cmd = clean[0].clone();
    let argc = clean.len();

    match cmd.as_str() {
        // ── multiplexer builtins ──
        "split-window-vertical" | "split-window-horizontal" => {
            let vertical = !cmd.contains("horizontal");
            let mut pct = 50;
            if argc > 1 && !clean[1].is_empty() {
                if let Some(n) = config::strtol10(&clean[1]) {
                    if (0..=100).contains(&n) {
                        pct = n as i32;
                    }
                }
            }
            util::set_shell_status(if mux::split(vertical, pct) == 0 { 0 } else { 1 });
            true
        }
        "close-pane" | "kill-curent-window" | "kill-current-window" => {
            util::set_shell_status(if mux::close() == 0 { 0 } else { 1 });
            true
        }
        "kill-all-windows" => {
            util::set_shell_status(if mux::kill_all() == 0 { 0 } else { 1 });
            true
        }
        _ if cmd.starts_with("move-to-") => {
            let d = &cmd["move-to-".len()..];
            let dir = match d {
                "right-window" => 1,
                "left-window" => 0,
                "up-window" => 2,
                "down-window" => 3,
                _ => -1,
            };
            if dir < 0 || !mux::active() {
                eprintln!("thesh: mux: not multiplexed");
                util::set_shell_status(1);
                return true;
            }
            mux::focus_dir(dir);
            util::set_shell_status(0);
            true
        }
        _ if cmd.starts_with("focus-") => {
            if !mux::active() {
                eprintln!("thesh: mux: not multiplexed");
                util::set_shell_status(1);
                return true;
            }
            let d = &cmd["focus-".len()..];
            match d {
                "left" => { mux::focus_dir(0); }
                "right" => { mux::focus_dir(1); }
                "up" => { mux::focus_dir(2); }
                "down" => { mux::focus_dir(3); }
                "next" => { mux::focus_cycle(1); }
                "prev" => { mux::focus_cycle(-1); }
                _ => eprintln!("thesh: focus: unknown direction '{d}'"),
            }
            util::set_shell_status(0);
            true
        }
        "panes" => {
            mux::list();
            util::set_shell_status(if mux::active() { 0 } else { 1 });
            true
        }

        // ── core builtins ──
        "cd" => {
            let tgt: String = if argc > 1 {
                let a = clean[1].as_str();
                if a == "-" {
                    match std::env::var("OLDPWD") {
                        Ok(o) => o,
                        Err(_) => {
                            eprintln!("thesh: cd: no previous directory");
                            util::set_shell_status(1);
                            return true;
                        }
                    }
                } else if a.starts_with('~') && a.as_bytes().get(1).map_or(true, |&c| c == b'/') {
                    let hh = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
                    format!("{hh}{}", &a[1..])
                } else {
                    a.to_string()
                }
            } else {
                std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
            };

            let oldpwd = std::env::var("PWD").ok();
            let ct = cstr(&tgt);
            if unsafe { libc::chdir(ct.as_ptr()) } < 0 {
                eprintln!("thesh: cd: {tgt}: {}", errno_str());
                util::set_shell_status(1);
                return true;
            }
            if let Some(o) = oldpwd {
                unsafe { libc::setenv(cstr("OLDPWD").as_ptr(), cstr(&o).as_ptr(), 1) };
            }
            let mut buf = [0u8; 4096];
            let r = unsafe { libc::getcwd(buf.as_mut_ptr().cast(), buf.len()) };
            if !r.is_null() {
                let cwd = unsafe { std::ffi::CStr::from_ptr(r.cast()) }.to_string_lossy();
                unsafe { libc::setenv(cstr("PWD").as_ptr(), cstr(&cwd).as_ptr(), 1) };
            }
            util::set_shell_status(0);
            true
        }
        "pwd" => {
            let mut buf = [0u8; 4096];
            let r = unsafe { libc::getcwd(buf.as_mut_ptr().cast(), buf.len()) };
            if !r.is_null() {
                let cwd = unsafe { std::ffi::CStr::from_ptr(r.cast()) }.to_string_lossy();
                util::out_fmt(format_args!("{cwd}\n"));
            }
            util::set_shell_status(0);
            true
        }
        "echo" => {
            let nflag = argc > 1 && clean[1] == "-n";
            let start = if nflag { 2 } else { 1 };
            for i in start..argc {
                if i == start {
                    util::out_fmt(format_args!("{}", clean[i]));
                } else {
                    util::out_fmt(format_args!(" {}", clean[i]));
                }
            }
            if !nflag {
                util::out("\n");
            }
            util::set_shell_status(0);
            true
        }
        "export" => {
            if argc == 1 {
                unsafe {
                    let mut e = environ;
                    if !e.is_null() {
                        while !(*e).is_null() {
                            let s = std::ffi::CStr::from_ptr(*e).to_string_lossy();
                            util::out_fmt(format_args!("{s}\n"));
                            e = e.add(1);
                        }
                    }
                }
            } else {
                for i in 1..argc {
                    let a = clean[i].clone();
                    if let Some(eq) = a.find('=') {
                        let (k, v) = (a[..eq].to_string(), a[eq + 1..].to_string());
                        unsafe { libc::setenv(cstr(&k).as_ptr(), cstr(&v).as_ptr(), 1) };
                    } else if let Some(v) = std::env::var_os(&a) {
                        util::out_fmt(format_args!("{a}={}\n", v.to_string_lossy()));
                    }
                }
            }
            util::set_shell_status(0);
            true
        }
        "unset" => {
            for i in 1..argc {
                unsafe { libc::unsetenv(cstr(&clean[i]).as_ptr()) };
            }
            util::set_shell_status(0);
            true
        }
        "alias" => {
            if argc == 1 {
                print_aliases();
            } else {
                let mut i = 1;
                while i < argc {
                    let a = clean[i].clone();
                    if let Some(eq) = a.find('=') {
                        // C: name capped at 127 bytes, value at 4095
                        let nb = &a.as_bytes()[..eq];
                        let name: String =
                            String::from_utf8_lossy(&nb[..nb.len().min(127)]).into_owned();
                        let mut val: Vec<u8> = a.as_bytes()[eq + 1..].to_vec();
                        if val.len() > 4095 {
                            val.truncate(4095);
                        }
                        for j in i + 1..argc {
                            let vl = val.len();
                            if vl > 0 && vl + 1 < 4096 {
                                val.push(b' ');
                            }
                            let avail = 4095 - val.len();
                            let ab = clean[j].as_bytes();
                            let take = avail.min(ab.len());
                            val.extend_from_slice(&ab[..take]);
                            val.truncate(4095);
                        }
                        let val = String::from_utf8_lossy(&val).into_owned();
                        alias_set(&name, &val);
                        break; // remainder consumed into this one alias
                    } else {
                        if let Some(v) = alias_lookup(&a) {
                            util::out_fmt(format_args!("{a}={v}\n"));
                        }
                    }
                    i += 1;
                }
            }
            util::set_shell_status(0);
            true
        }
        "unalias" => {
            if argc > 1 && clean[1] == "-a" {
                aliases().clear();
            } else {
                for i in 1..argc {
                    alias_unset(&clean[i]);
                }
            }
            util::set_shell_status(0);
            true
        }
        "history" => {
            history::with_all(|items| {
                for (i, it) in items.iter().enumerate() {
                    util::out_fmt(format_args!("{:5}  {}\n", i + 1, it));
                }
            });
            util::set_shell_status(0);
            true
        }
        "clearhistory" => {
            history::clear();
            util::set_shell_status(0);
            true
        }
        "source" | "." => {
            if argc > 1 {
                run_file(&clean[1]);
            }
            util::set_shell_status(0);
            true
        }
        "exit" => {
            history::save();
            let code = if argc > 1 {
                config::strtol10(&clean[1]).unwrap_or(0) as i32
            } else {
                util::shell_status()
            };
            std::process::exit(code)
        }
        "type" => {
            for i in 1..argc {
                if config::with(|c| c.hybrid) {
                    case_fold_command(&mut clean[i]);
                }
                let word = clean[i].clone();
                if let Some(a) = alias_lookup(&word) {
                    util::out_fmt(format_args!("{word} is aliased to `{a}'\n"));
                } else if is_builtin(&word) {
                    util::out_fmt(format_args!("{word} is a shell builtin\n"));
                } else if let Some(p) = path_of_command(&word) {
                    util::out_fmt(format_args!("{word} is {p}\n"));
                } else {
                    util::out_fmt(format_args!("{word}: not found\n"));
                }
            }
            util::set_shell_status(0);
            true
        }
        "set" => {
            util::set_shell_status(0);
            true
        }
        "hash" => {
            let force = (1..argc).any(|i| clean[i] == "-r" || clean[i] == "--rehash");
            if force {
                dict::force_refresh(path_env().as_deref());
            }
            util::set_shell_status(0);
            true
        }
        "presets" => {
            util::set_shell_status(presets::cmd_presets(argc as i32, clean));
            true
        }
        "savepreset" => {
            util::set_shell_status(presets::cmd_savepreset(argc as i32, clean));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> Vec<String> {
        tokenize(s)
    }

    #[test]
    fn tokenize_words_quotes_escapes_comments() {
        assert_eq!(tok("echo hello world"), ["echo", "hello", "world"]);
        // no phantom empty words from repeated whitespace (started flag)
        assert_eq!(tok("  a   b "), ["a", "b"]);
        // an explicit empty word survives
        assert_eq!(tok("echo ''"), ["echo", ""]);
        assert_eq!(tok("echo \"\" x"), ["echo", "", "x"]);
        assert_eq!(tok("echo 'a b' \"c d\""), ["echo", "a b", "c d"]);
        assert_eq!(tok("echo a\\ b"), ["echo", "a b"]);
        assert_eq!(tok("echo hi # comment"), ["echo", "hi"]);
        assert_eq!(tok("# full comment"), Vec::<String>::new());
        assert_eq!(tok("echo foo#bar"), ["echo", "foo#bar"]);
        assert_eq!(tok("echo 'unterminated"), ["echo", "unterminated"]);
        assert_eq!(tok("echo \\$literal"), ["echo", "$literal"]);
        // lone trailing backslash: pushed literally, NOT quoted-flagged
        assert_eq!(tok("echo x\\"), ["echo", "x\\"]);
    }

    #[test]
    fn tokenize_expansions() {
        let _g = crate::util::test_lock();
        let home = std::env::var("HOME").ok();
        let expect_home: Vec<String> = match &home {
            Some(h) => vec!["echo".into(), h.clone()],
            None => vec!["echo".into()], // C consumes ~/$ with no HOME…
        };
        assert_eq!(tok("echo ~"), expect_home, "tilde");
        assert_eq!(tok("echo '~'"), ["echo", "~"], "quoted tilde");
        // lone `$` consumes itself and leaves an empty word (C parity)
        assert_eq!(tok("echo $"), ["echo", ""]);
        match &home {
            Some(h) => {
                assert_eq!(tok("echo ${HOME}x"), vec!["echo".to_string(), format!("{h}x")]);
                assert_eq!(tok("echo a${HOME}b"), vec!["echo".to_string(), format!("a{h}b")]);
            }
            None => {}
        }
        // missing var → empty word REMAINS (C sets `started` at `$`,
        // so the flush pushes "" — oracle prints ` z`, i.e. ["", "z"])
        assert_eq!(tok("echo $THESH_NO_SUCH_VAR_XYZ z"), ["echo", "", "z"]);
    }

    #[test]
    fn split_ops_exact_operators() {
        let (units, ops) = split_ops("a; b && c || d & e");
        assert_eq!(units, ["a", "b", "c", "d", "e"]);
        assert_eq!(ops, [0, 3, 1, 2, 4]);
        // operators inside quotes are literal
        let (units, ops) = split_ops("echo 'a;b' \"x&&y\"");
        assert_eq!(units, ["echo 'a;b' \"x&&y\""]);
        assert_eq!(ops, [0]);
        // backslash escapes the operator
        let (units, _) = split_ops("a \\; b");
        assert_eq!(units, ["a \\; b"]);
        // whitespace-only segments drop, like C's PUSH trim
        let (units, ops) = split_ops("  a  ;;  b  ");
        assert_eq!(units, ["a", "b"]);
        assert_eq!(ops, [0, 3]);
        // &> / >& stay in-segment (not operators)
        let (units, _) = split_ops("cmd &>file");
        assert_eq!(units, ["cmd &>file"]);
    }

    #[test]
    fn split_pipeline_quotes_and_trims() {
        assert_eq!(split_pipeline("a | b | c"), ["a", "b", "c"]);
        assert_eq!(split_pipeline("echo 'x|y'"), ["echo 'x|y'"]);
        assert_eq!(split_pipeline("  a  |  b  "), ["a", "b"]);
        assert_eq!(split_pipeline("a|"), ["a"]);
        assert_eq!(split_pipeline(""), Vec::<String>::new());
        assert_eq!(split_pipeline("  "), Vec::<String>::new());
    }

    fn rtok(s: &str) -> Option<(i32, i32, i32, i32, Option<String>)> {
        parse_redir_tok(s).map(|t| (t.kind, t.mode, t.fd, t.dupfd, t.rest))
    }

    #[test]
    fn redir_token_forms() {
        assert_eq!(rtok(">f"), Some((2, 1, 1, -1, Some("f".into()))));
        assert_eq!(rtok(">>f"), Some((2, 2, 1, -1, Some("f".into()))));
        assert_eq!(rtok("2>f"), Some((3, 1, 2, -1, Some("f".into()))));
        assert_eq!(rtok("2>>f"), Some((3, 2, 2, -1, Some("f".into()))));
        assert_eq!(rtok("2>&1"), Some((5, 0, 2, 1, None)));
        assert_eq!(rtok("&>f"), Some((4, 1, 1, -1, Some("f".into()))));
        assert_eq!(rtok("&>>f"), Some((4, 2, 1, -1, Some("f".into()))));
        assert_eq!(rtok("<f"), Some((1, 0, 0, -1, Some("f".into()))));
        // NOT redirections (stay plain words)
        assert_eq!(rtok("echo"), None);
        assert_eq!(rtok("10>f"), None); // two-digit fd not supported by C
        assert_eq!(rtok("a>f"), None);
        assert_eq!(rtok("out"), None);
    }

    #[test]
    fn collect_redirs_clean_split_and_cap() {
        let args: Vec<String> = ["echo", "hi", ">", "f"].iter().map(|s| s.to_string()).collect();
        let (clean, redr) = collect_redirs(&args).expect("ok");
        assert_eq!(clean, ["echo", "hi"]);
        assert_eq!(redr.len(), 1);
        assert_eq!((redr[0].fd, redr[0].mode, redr[0].path.as_deref()), (1, 1, Some("f")));

        // missing target → error
        let args: Vec<String> = ["cmd", ">"].iter().map(|s| s.to_string()).collect();
        assert!(collect_redirs(&args).is_none());

        // C's Redir[16] cap: 17 redirections → 16 kept, token still consumed
        let mut args: Vec<String> = vec!["cmd".to_string()];
        for i in 0..17 {
            args.push(">".to_string());
            args.push(format!("f{i}"));
        }
        let (clean, redr) = collect_redirs(&args).expect("ok");
        assert_eq!(clean, ["cmd"]);
        assert_eq!(redr.len(), 16);
    }

    #[test]
    fn builtin_list_is_fingerprinted() {
        // order matters: golden `sugg` vectors iterate builtins first
        assert_eq!(BUILTIN_LIST[0], "cd");
        assert_eq!(BUILTIN_LIST[2], "echo");
        assert!(BUILTIN_LIST.contains(&"split-window-vertical"));
        assert!(is_builtin("savepreset"));
        // C's list does NOT contain "." — `.` only works via run_builtin's
        // unconditional child path (pinned quirk)
        assert!(!is_builtin("."));
        assert!(!is_builtin("ls"));
        assert!(!is_builtin(""));
    }

    #[test]
    fn case_fold_only_known_lowercase_forms() {
        let mut s = "ECHO".to_string();
        assert!(case_fold_command(&mut s));
        assert_eq!(s, "echo");

        let mut s = "CD".to_string();
        assert!(case_fold_command(&mut s));
        assert_eq!(s, "cd");

        // no uppercase → untouched, no work
        let mut s = "echo".to_string();
        assert!(!case_fold_command(&mut s));
        // explicit paths never fold
        let mut s = "/BIN/LS".to_string();
        assert!(!case_fold_command(&mut s));
        // unknown command → no fold
        let mut s = "ZZUNKNOWNA".to_string();
        assert!(!case_fold_command(&mut s));
        assert_eq!(s, "ZZUNKNOWNA");
    }

    #[test]
    fn alias_lifecycle_and_cap() {
        let _g = crate::util::test_lock();
        alias_set("ll", "ls -la");
        assert_eq!(alias_lookup("ll").as_deref(), Some("ls -la"));
        alias_set("ll", "ls -l"); // update in place
        assert_eq!(alias_lookup("ll").as_deref(), Some("ls -l"));
        alias_unset("ll");
        assert_eq!(alias_lookup("ll"), None);

        // ALIAS_MAX cap: the 129th alias is silently dropped (C quirk)
        for i in 0..128 {
            alias_set(&format!("cap{i}"), "x");
        }
        alias_set("beyond_cap", "y");
        assert_eq!(alias_lookup("beyond_cap"), None);
        for i in 0..128 {
            alias_unset(&format!("cap{i}"));
        }
        assert_eq!(alias_lookup("cap0"), None);
    }

    #[test]
    fn expand_aliases_recursion_guard() {
        let _g = crate::util::test_lock();
        alias_set("a", "b");
        alias_set("b", "a");
        // ping-pong stops at C's guard of 10 expansions
        let out = expand_aliases(vec!["a".to_string()]);
        assert_eq!(out.len(), 1);
        assert!(out[0] == "a" || out[0] == "b", "got {:?}", out);
        alias_unset("a");
        alias_unset("b");
    }
}
