//! Port of `src/main.c` — process startup, run modes, the REPL loop,
//! rc loading with auto-reload, environment loading, and `build_prompt`.

use crate::{config, dict, editor, exec, history, mux, term, util};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const THESH_NAME: &str = "thesh";

/// SIGWINCH: redraw needed (C: `volatile sig_atomic_t thesh_winched`).
pub static WINCHED: AtomicBool = AtomicBool::new(false);
/// SIGCONT: re-assert state.
pub static CONTED: AtomicBool = AtomicBool::new(false);

pub fn winched() -> bool {
    WINCHED.swap(false, Ordering::SeqCst)
}

pub fn conted() -> bool {
    CONTED.swap(false, Ordering::SeqCst)
}

// ── rc file auto-reload ────────────────────────────────────────────────

struct RcWatch {
    path: String,
    mtime: Option<(i64, i64)>, // (sec, nsec) since the epoch; None = missing
    size: i64,
    exists: bool,
}

static RC_WATCH: Mutex<Vec<RcWatch>> = Mutex::new(Vec::new());

fn rc_watch_add(path: &str) {
    let st = std::fs::metadata(path).ok();
    let watch = RcWatch {
        path: path.to_string(),
        mtime: st.as_ref().and_then(|m| mtime_parts(m)),
        size: st.as_ref().map_or(0, |m| m.len() as i64),
        exists: st.is_some(),
    };
    let mut w = RC_WATCH.lock().unwrap_or_else(|p| p.into_inner());
    if w.len() >= 4 {
        return;
    }
    w.push(watch);
}

fn mtime_parts(m: &std::fs::Metadata) -> Option<(i64, i64)> {
    let t = m.modified().ok()?;
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => Some((d.as_secs() as i64, d.subsec_nanos() as i64)),
        Err(e) => {
            let d = e.duration();
            Some((-(d.as_secs() as i64), -(d.subsec_nanos() as i64)))
        }
    }
}

/// Port of `maybe_reload_config()`: re-source changed rc files quietly.
/// Gated by `Cfg.autoreload`.
fn maybe_reload_config() {
    if !config::with(|c| c.autoreload) {
        return;
    }
    let mut changed_paths: Vec<String> = Vec::new();
    {
        let mut w = RC_WATCH.lock().unwrap_or_else(|p| p.into_inner());
        for watch in w.iter_mut() {
            if watch.path.is_empty() {
                continue;
            }
            let st = std::fs::metadata(&watch.path).ok();
            let changed;
            let ok = st.is_some();
            match &st {
                None => {
                    changed = watch.exists; // file removed
                    watch.exists = false;
                }
                Some(m) => {
                    let mt = mtime_parts(m);
                    changed =
                        !watch.exists || mt != watch.mtime || (m.len() as i64) != watch.size;
                    watch.exists = true;
                    watch.mtime = mt;
                    watch.size = m.len() as i64;
                }
            }
            if changed && ok {
                changed_paths.push(watch.path.clone());
            }
        }
    } // watch lock released before re-sourcing (C's reload never touches
      // the watch list either — same observable order)
    for p in changed_paths {
        exec::run_file_silent(&p);
    }
}

/// Port of `thesh_config_check()` — rc auto-reload tick (called by the
/// REPL and the mux router loop).
pub fn config_check() {
    maybe_reload_config();
}

// ── /etc/environment ───────────────────────────────────────────────────

/// Port of `load_environment()`: pam_env-style KEY=value lines loaded
/// before anything else. THESH_ENV_FILE overrides the path (test hook).
fn load_environment() {
    let path = std::env::var_os("THESH_ENV_FILE")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/etc/environment".into());
    let Ok(data) = std::fs::read(&path) else { return };

    // C reads with fgets(4096): a line longer than 4095 bytes without a
    // newline is discarded up to the next '\n' — emulate byte-exactly,
    // including the tail chunk being parsed as its own line.
    let mut pos = 0usize;
    while pos < data.len() {
        // fgets: at most 4095 bytes or up to and including '\n'
        let chunk_end = data[pos..]
            .iter()
            .take(4095)
            .position(|&c| c == b'\n')
            .map(|i| pos + i + 1)
            .unwrap_or_else(|| (pos + 4095).min(data.len()));
        let mut line = &data[pos..chunk_end];
        pos = chunk_end;

        let had_nl = line.last() == Some(&b'\n');
        if !had_nl && pos < data.len() {
            // oversized: C discards the remainder of the physical line
            // (until '\n' or EOF) — this chunk itself was already read by
            // fgets but fails the strchr('\n') check → skipped entirely
            while pos < data.len() && data[pos] != b'\n' {
                pos += 1;
            }
            if pos < data.len() {
                pos += 1;
            }
            continue;
        }
        if !had_nl && pos >= data.len() {
            // final chunk without newline: C's strchr finds none but
            // feof… fgets returns the partial line only when EOF hits
            // after it — treated as a normal line (C: !feof was false
            // only for full-buffer cases; a tail partial IS parsed)
        }

        // trim trailing whitespace/newline (C: isspace loop)
        while !line.is_empty() && matches!(line.last(), Some(c) if (b" \t\n\r\x0b\x0c").contains(c))
        {
            line = &line[..line.len() - 1];
        }
        // trim leading whitespace, skip empty/#
        let mut i = 0;
        while i < line.len() && (b" \t\n\r\x0b\x0c").contains(&line[i]) {
            i += 1;
        }
        let line = &line[i..];
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let Some(eq) = line.iter().position(|&c| c == b'=') else { continue };
        if eq == 0 {
            continue;
        }
        // KEY: trailing spaces trimmed before '=', valid identifier
        let mut key_end = eq;
        while key_end > 0 && matches!(line[key_end - 1], c if (b" \t\r\x0b\x0c").contains(&c)) {
            key_end -= 1;
        }
        let key = &line[..key_end];
        if key.is_empty() {
            continue;
        }
        let mut ok = true;
        for (k, &c) in key.iter().enumerate() {
            let good = c.is_ascii_alphabetic() || c == b'_' || (k > 0 && c.is_ascii_digit());
            if !good {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }

        // VALUE: strip one layer of matching quotes
        let mut val = &line[eq + 1..];
        if val.len() >= 2
            && ((val[0] == b'"' && val[val.len() - 1] == b'"')
                || (val[0] == b'\'' && val[val.len() - 1] == b'\''))
        {
            val = &val[1..val.len() - 1];
        }

        let key = String::from_utf8_lossy(key).into_owned();
        let val = String::from_utf8_lossy(val).into_owned();
        unsafe {
            libc::setenv(
                std::ffi::CString::new(key.as_str()).unwrap_or_default().as_ptr(),
                std::ffi::CString::new(val.as_str()).unwrap_or_default().as_ptr(),
                1,
            );
        }
    }
}

// ── build_prompt ───────────────────────────────────────────────────────

/// Port of `build_prompt()`: `looks` template > `$PS1` > default.
///
/// DEVIATION: C fixed user[64]/host[128] buffers truncate; Rust keeps
/// the full strings (usernames/hostnames never approach those limits).
pub fn build_prompt() -> String {
    let mut user = String::from("?");
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            user = std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }

    let mut host = String::from("?");
    let mut hbuf = [0u8; 128];
    let r = unsafe { libc::gethostname(hbuf.as_mut_ptr().cast(), hbuf.len()) };
    if r == 0 {
        let end = hbuf.iter().position(|&c| c == 0).unwrap_or(hbuf.len());
        host = String::from_utf8_lossy(&hbuf[..end]).into_owned();
    }

    let mut dir = String::from("/");
    if let Ok(d) = std::env::current_dir() {
        dir = d.to_string_lossy().into_owned();
    }

    // collapse $HOME prefix to ~
    let home = std::env::var("HOME").ok();
    if let Some(home) = home.as_deref().filter(|h| !h.is_empty() && *h != "/") {
        if dir.starts_with(home) {
            let rest = &dir[home.len()..];
            if rest.is_empty() {
                dir = "~".to_string();
            } else if rest.starts_with('/') {
                dir = format!("~{rest}");
            }
        }
    }

    let cfg = config::snapshot();
    if let Some(looks) = cfg.looks.as_deref().filter(|s| !s.is_empty()) {
        return config::render_looks_str(looks, &user, &host, &dir);
    }
    if let Some(ps1) = std::env::var_os("PS1") {
        let ps1 = ps1.to_string_lossy();
        if !ps1.is_empty() {
            return config::render_ps1(&ps1, &user, &host, &dir);
        }
    }
    config::render_looks_str(config::default_looks(), &user, &host, &dir)
}

// ── signals ────────────────────────────────────────────────────────────

extern "C" fn on_winch(_sig: libc::c_int) {
    WINCHED.store(true, Ordering::SeqCst); // redraw the edit line
    mux::notify_winch(); // and let the mux re-layout
}

extern "C" fn on_cont(_sig: libc::c_int) {
    CONTED.store(true, Ordering::SeqCst);
}

extern "C" fn on_exit_cleanup() {
    // C: atexit(thesh_on_exit) → term_exit_raw + mux_shutdown
    term::exit_raw();
    mux::shutdown();
}

// ── main ───────────────────────────────────────────────────────────────

/// Port of `main()`: returns the process exit status.
pub fn run_main() -> i32 {
    unsafe {
        libc::setlocale(libc::LC_ALL, b"\0".as_ptr().cast());
        libc::signal(libc::SIGINT, libc::SIG_IGN); // shell never dies on ^C
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        // fn → integer needs the pointer hop (rustc function_casts_as_integer)
        libc::signal(
            libc::SIGWINCH,
            on_winch as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGCONT, on_cont as *const () as libc::sighandler_t);
    }

    util::set_shell_status(0);
    config::init();
    load_environment();
    dict::refresh(std::env::var("PATH").ok().as_deref());
    history::setup();
    unsafe { libc::atexit(on_exit_cleanup) };

    let args: Vec<String> = std::env::args().collect();

    // `-c 'cmd'` (the C library convention `sh -c -- cmd` swallows `--`)
    if args.len() >= 3 && args[1] == "-c" {
        let mut ai = 2;
        if args.len() >= 4 && args[2] == "--" {
            ai = 3;
        }
        history::load();
        let st = exec::exec_line(&args[ai]);
        history::save();
        return st;
    }

    // version flags — printed before any rc loading
    if args.len() == 2 && matches!(args[1].as_str(), "--version" | "-v" | "-V") {
        println!("{THESH_NAME} {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    if args.len() == 2 {
        history::load();
        let st = exec::run_file(&args[1]);
        history::save();
        return if st >= 0 { st } else { 1 };
    }

    // interactive
    exec::run_file("/etc/theshrc");
    let mut user_rc: Option<String> = None;
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let rc = format!("{}/.theshrc", home.to_string_lossy());
        let rc = if access_r_ok(&rc) {
            rc
        } else {
            format!("{}/.therc", home.to_string_lossy()) // legacy
        };
        exec::run_file(&rc);
        user_rc = Some(rc);
    }

    // Watch the rc files we just loaded (creation is detected too)
    rc_watch_add("/etc/theshrc");
    if let Some(rc) = &user_rc {
        rc_watch_add(rc);
    }

    // History after the rc files so historylimit/history from the
    // config already apply.
    history::load();

    loop {
        config_check();
        let prompt = build_prompt();
        let mut cancelled = false;
        let line = editor::edit_line(&prompt, &mut cancelled);
        let Some(line) = line else { break }; // EOF
        if cancelled {
            continue;
        }
        if !line.is_empty() {
            history::add(&line);
            exec::exec_line(&line);
        }
    }

    history::save();
    util::shell_status()
}

/// C: access(path, R_OK).
fn access_r_ok(p: &str) -> bool {
    let c = std::ffi::CString::new(p).unwrap_or_default();
    unsafe { libc::access(c.as_ptr(), libc::R_OK) == 0 }
}
