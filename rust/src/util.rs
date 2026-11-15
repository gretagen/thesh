//! Port of `src/util.c` — minus the `xmalloc` family (Rust's allocator
//! aborts on OOM for us) and printf-style `outf` (replaced by
//! `out_fmt(format_args!(…))`).

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

/// Port of the global `shell_status` — `$?` in expansions, set by exec.
pub static SHELL_STATUS: AtomicI32 = AtomicI32::new(0);

pub fn shell_status() -> i32 {
    SHELL_STATUS.load(Ordering::SeqCst)
}

pub fn set_shell_status(v: i32) {
    SHELL_STATUS.store(v, Ordering::SeqCst);
}

/// Port of `outn()`: write to stdout with EINTR retry and **no buffering**.
///
/// The C shell writes raw so its output interleaves correctly with
/// children's stdout; Rust must not route this through std's `LineWriter`
/// or ordering would change.
pub fn outn(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let r = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        bytes = &bytes[r as usize..];
    }
}

/// Port of `out()`.
pub fn out(s: &str) {
    outn(s.as_bytes());
}

/// Rust replacement for printf-style `outf()`: `out_fmt(format_args!("…"))`.
///
/// (The C `outf` needed a 4096-byte buffer + heap fallback after the
/// truncation fix; `format!` allocates only as needed.)
pub fn out_fmt(args: std::fmt::Arguments<'_>) {
    outn(args.to_string().as_bytes());
}

// ── batched terminal output (util.c tty_buf_*) ──────────────────────────

const TTY_BUF_CAP: usize = 16384;
static TTY_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn lock_buf() -> std::sync::MutexGuard<'static, Vec<u8>> {
    TTY_BUF.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `mux_term_write()` (mux.c): any pending batched repaint must
/// reach the terminal first, then write straight to stdout.
pub fn mux_term_write(b: &[u8]) {
    tty_buf_flush();
    outn(b);
}

/// Port of `tty_buf_flush()`.
pub fn tty_buf_flush() {
    let pending = std::mem::take(&mut *lock_buf());
    if !pending.is_empty() {
        outn(&pending);
    }
}

/// Port of `tty_buf_write()`: batch heavy repaint output; payloads larger
/// than the whole buffer bypass the batch and write directly.
pub fn tty_buf_write(b: &[u8]) {
    if b.len() >= TTY_BUF_CAP {
        tty_buf_flush();
        mux_term_write(b);
        return;
    }
    let mut buf = lock_buf();
    if buf.len() + b.len() > TTY_BUF_CAP {
        let pending = std::mem::take(&mut *buf);
        outn(&pending);
    }
    buf.extend_from_slice(b);
}

// ── optional debug log (util.c muxdbg*) ─────────────────────────────────

/// `-2 unchecked → checked once`, like the C `dbg_fd`: the path is read
/// from `THESH_MUX_DEBUG` on first use and the fd stays open for the
/// process lifetime (as in C — one shared O_APPEND file for router + panes).
static DBG: OnceLock<Option<std::os::fd::RawFd>> = OnceLock::new();

fn dbg_fd() -> Option<std::os::fd::RawFd> {
    *DBG.get_or_init(|| {
        let path = std::env::var_os("THESH_MUX_DEBUG")?;
        if path.is_empty() {
            return None;
        }
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_bytes()).ok()?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            None
        } else {
            Some(fd)
        }
    })
}

pub fn muxdbg_on() -> bool {
    dbg_fd().is_some()
}

fn dbg_write(bytes: &[u8]) {
    let Some(fd) = dbg_fd() else { return };
    let mut off = 0;
    while off < bytes.len() {
        let r = unsafe { libc::write(fd, bytes[off..].as_ptr().cast(), bytes.len() - off) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        off += r as usize;
    }
}

/// Port of `muxdbg()` — callers pass pre-formatted text
/// (`muxdbg(&format!(…))` instead of printf style).
///
/// Deviation: the C version truncates at 1024 bytes (stack buf); we write
/// the whole message. Debug-log content only.
pub fn muxdbg(msg: &str) {
    dbg_write(msg.as_bytes());
}

/// Port of `muxdbg_bytes()`' escaping (factor'd out for unit testing):
/// cap the shown bytes at 160, escape control chars, clamp near 400 bytes.
///
/// Note the C original can overrun its 400-byte `esc` buffer when the
/// `...(+N)` suffix is appended near the limit (UB); we clamp instead.
pub fn escape_bytes(b: &[u8]) -> Vec<u8> {
    let shown = if b.len() < 160 { b.len() } else { 160 };
    let mut esc: Vec<u8> = Vec::new();
    for &c in &b[..shown] {
        if esc.len() + 6 >= 400 {
            break; // mirrors `e + 6 < sizeof esc`
        }
        match c {
            0x1b => esc.extend_from_slice(b"\\e"),
            b'\r' => esc.extend_from_slice(b"\\r"),
            b'\n' => esc.extend_from_slice(b"\\n"),
            b'\t' => esc.extend_from_slice(b"\\t"),
            0x00..=0x1f | 0x7f => esc.extend_from_slice(format!("\\x{c:02x}").as_bytes()),
            _ => esc.push(c), // raw byte, exactly like C's esc[e++] = c
        }
    }
    if b.len() > shown {
        esc.extend_from_slice(format!("...(+{})", b.len() - shown).as_bytes());
    }
    esc
}

/// Port of `muxdbg_bytes()`: escape raw terminal bytes for the debug log.
pub fn muxdbg_bytes(b: &[u8]) {
    if !muxdbg_on() {
        return;
    }
    let mut line = b"  bytes: ".to_vec();
    line.extend_from_slice(&escape_bytes(b));
    line.extend_from_slice(b"\n");
    dbg_write(&line);
}

/// Cross-module test serialization: every test that touches process
/// global state (config, history, dict, bind, presets, …) must hold this
/// for its duration so parallel test threads can't interleave.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `get_home()`: `$HOME` if set and non-empty, else the passwd
/// database's home dir, else `"/"`.
pub fn get_home() -> String {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return h.to_string_lossy().into_owned();
        }
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() {
            let dir = (*pw).pw_dir;
            if !dir.is_null() {
                // like C: an empty pw_dir is returned as-is (no "/" fallback)
                return std::ffi::CStr::from_ptr(dir).to_string_lossy().into_owned();
            }
        }
    }
    "/".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_ctrl_and_specials() {
        assert_eq!(escape_bytes(b"\x1b\r\n\t"), b"\\e\\r\\n\\t");
        assert_eq!(escape_bytes(b"abc"), b"abc");
        assert_eq!(escape_bytes(&[0x01, 0x7f]), b"\\x01\\x7f");
        // high bytes pass through raw (C writes the byte, not a char)
        assert_eq!(escape_bytes(&[0xe9]), vec![0xe9]);
    }

    #[test]
    fn escape_caps_at_160_shown() {
        let big = vec![b'a'; 200];
        let esc = escape_bytes(&big);
        assert_eq!(esc, format!("{}...(+40)", "a".repeat(160)).into_bytes());
    }

    #[test]
    fn escape_never_exceeds_400() {
        // worst case: 160 control bytes → 4 chars each = 640 > 400
        let ctrls = vec![0x01u8; 160];
        assert!(escape_bytes(&ctrls).len() < 400);
    }

    #[test]
    fn shell_status_roundtrip() {
        let old = shell_status();
        set_shell_status(42);
        assert_eq!(shell_status(), 42);
        set_shell_status(old);
    }
}
