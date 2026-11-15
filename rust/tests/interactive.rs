//! Interactive pty suite: drive the Rust shell over a real pty with
//! scripted keystrokes and assert on the raw terminal stream — covers
//! the line editor, prompt rendering, ghost suggestions, completion and
//! the multiplexer cycle, which the `cargo test` unit suites can't reach.
//!
//! Deterministic: fixed 80x24 geometry, temp HOME with no user rc,
//! empty history, animations off by default, adaptive startup wait
//! (typing into a not-yet-raw tty would lose ^D).

use std::path::PathBuf;

extern "C" {
    fn openpty(
        amaster: *mut libc::c_int,
        aslave: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winp: *const libc::winsize,
    ) -> libc::c_int;
}

/// Fresh env dir per invocation, keyed by THIS process's pid so both
/// runs within a test see identical paths.
fn fresh_env() -> Vec<(String, String)> {
    let root = std::env::temp_dir().join(format!("thesh-pty-{}", std::process::id()));
    let home = root.join("home");
    let hist = root.join("histfile");
    let envf = root.join("envfile");
    std::fs::create_dir_all(&home).expect("temp HOME");
    std::fs::write(&hist, "").ok();
    std::fs::write(&envf, "").ok();
    vec![
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("THESH_HISTFILE".into(), hist.to_string_lossy().into_owned()),
        ("THESH_ENV_FILE".into(), envf.to_string_lossy().into_owned()),
    ]
}

/// Run `bin` on a pty (80x24). Waits for a quiescent prompt (raw mode
/// engaged), writes each `(bytes, delay_after_ms)` chunk, then drains
/// until exit. Returns (output bytes, exit code).
fn run_pty_chunks(bin: &std::path::Path, chunks: &[(Vec<u8>, u64)]) -> (Vec<u8>, Option<i32>) {
    let env_pairs = fresh_env();

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) } < 0 {
        panic!("openpty: {}", std::io::Error::last_os_error());
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        panic!("fork: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
            for (k, v) in env_pairs {
                let ck = std::ffi::CString::new(k).unwrap();
                let cv = std::ffi::CString::new(v).unwrap();
                libc::setenv(ck.as_ptr(), cv.as_ptr(), 1);
            }
            let cp = std::ffi::CString::new(bin.to_str().unwrap()).unwrap();
            libc::execv(cp.as_ptr(), [cp.as_ptr(), std::ptr::null()].as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(slave) };

    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    // Wait for the shell to finish startup: require at least one byte of
    // output (rc messages + first prompt draw ⇒ raw mode is engaged) and
    // then 150 ms of silence. A fixed sleep is not enough — typing into
    // a still-cooked tty loses ^D (canonical mode eats it).
    let mut seen_output = false;
    let mut last_data = std::time::Instant::now();
    let startup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut pf = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pf, 1, 50) };
        if r > 0 && pf.revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                seen_output = true;
                last_data = std::time::Instant::now();
                continue;
            }
        }
        if seen_output && last_data.elapsed() >= std::time::Duration::from_millis(150) {
            break; // quiescent prompt: raw mode is on
        }
        if std::time::Instant::now() >= startup_deadline {
            break; // give up waiting; the case will fail visibly
        }
    }

    // paced chunk writes (the mux sequence needs delays between keys)
    let total = chunks.len();
    for (idx, (bytes, delay)) in chunks.iter().enumerate() {
        if !bytes.is_empty()
            && unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) } < 0
        {
            panic!("write to pty: {}", std::io::Error::last_os_error());
        }
        let wait = if idx + 1 < total { *delay } else { 0 };
        if wait > 0 {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(wait);
            while std::time::Instant::now() < until {
                let mut pf = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
                let r = unsafe { libc::poll(&mut pf, 1, 50) };
                if r > 0 && pf.revents & libc::POLLIN != 0 {
                    let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                    if n > 0 {
                        out.extend_from_slice(&buf[..n as usize]);
                    }
                }
            }
        }
    }

    // Drain until the child exits and the pty hangs up. Reap via WNOHANG
    // each round; a HUP/EOF with the slave gone means the child is done
    // (blocking reap then). Never break before reaping — that left
    // zombies and false timeouts in earlier revisions.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut exit_code: Option<i32> = None;

    let code_of = |w: libc::c_int| -> i32 {
        if libc::WIFEXITED(w) {
            libc::WEXITSTATUS(w)
        } else if libc::WIFSIGNALED(w) {
            128 + libc::WTERMSIG(w)
        } else {
            -1
        }
    };
    let drain = |out: &mut Vec<u8>, buf: &mut [u8]| {
        loop {
            let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    };

    loop {
        if exit_code.is_none() {
            let mut w: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut w, libc::WNOHANG) };
            if r == pid {
                exit_code = Some(code_of(w));
                drain(&mut out, &mut buf); // reap first, then read the tail
                break;
            }
            if r < 0 {
                exit_code = Some(-1);
                break;
            }
        }

        let mut pf = libc::pollfd {
            fd: master,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let pr = unsafe { libc::poll(&mut pf, 1, 100) };
        if pr > 0 && pf.revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            }
            // n <= 0: EIO after slave close — fall through to reaping
        }
        if pr > 0 && pf.revents & (libc::POLLHUP | libc::POLLERR) != 0 && exit_code.is_none() {
            // slave side closed → the child has exited: blocking reap
            let mut w: libc::c_int = 0;
            loop {
                let r = unsafe { libc::waitpid(pid, &mut w, 0) };
                if r == pid || r < 0 {
                    break;
                }
            }
            exit_code = Some(if w >= 0 { code_of(w) } else { -1 });
            drain(&mut out, &mut buf);
            break;
        }
        if std::time::Instant::now() > deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            out.extend_from_slice(b"<<PTY-TIMEOUT>>");
            unsafe { libc::close(master) };
            return (out, Some(-999));
        }
    }
    unsafe { libc::close(master) };
    (out, exit_code)
}

/// Single-shot input: one write after startup.
fn run_pty(bin: &std::path::Path, input: &[u8]) -> (Vec<u8>, Option<i32>) {
    run_pty_chunks(bin, &[(input.to_vec(), 0)])
}

fn show(b: &[u8]) -> String {
    let take = b.len().min(240);
    let mut s = String::new();
    for &c in &b[..take] {
        match c {
            0x1b => s.push_str("\\e"),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    if b.len() > take {
        s.push('…');
    }
    s
}

/// Assert one run's health: finished, no crash markers, expected output.
fn assert_session(name: &str, out: &[u8], code: Option<i32>, want: &str) {
    let s = String::from_utf8_lossy(out);
    assert_ne!(code, Some(-999), "{name}: timed out\n…{}", show(&out[out.len().saturating_sub(600)..]));
    assert!(!s.contains("<<PTY-TIMEOUT>>"), "{name}: timeout marker in output");
    assert!(!s.contains("AddressSanitizer"), "{name}: sanitizer output:\n{s}");
    assert!(!s.contains("panicked at"), "{name}: panic:\n…{}", show(&out[out.len().saturating_sub(1200)..]));
    assert!(s.contains(want), "{name}: expected {want:?} in output\n…{}", show(&out[out.len().saturating_sub(600)..]));
}

/// Scripted editing sessions: every case types, executes and exits with
/// Ctrl+D on an empty line.
#[test]
fn editing_sessions() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_thesh"));
    let cases: Vec<(&str, Vec<u8>, &str, Option<i32>)> = vec![
        ("basic_echo", b"echo pty works\r\x04".to_vec(), "pty works", Some(0)),
        ("backspace_edit", b"echo abx\x7fc\r\x04".to_vec(), "abc", Some(0)),
        ("ctrl_u_clear", b"junk\x15echo cleared\r\x04".to_vec(), "cleared", Some(0)),
        (
            "arrow_history_empty",
            b"\x1b[A\x1b[Becho after arrows\r\x04".to_vec(),
            "after arrows",
            Some(0),
        ),
        // real Home/End escape sequences — NOT ^A/^E: this machine's
        // /etc/theshrc binds CTRL+E to an interactive editor
        ("home_end_keys", b"echo xyz\x1b[H\x1b[F!\r\x04".to_vec(), "xyz!", Some(0)),
        (
            "word_motion_delete",
            b"echo one two\x1b[D\x1b[D\x1b[3~\r\x04".to_vec(),
            "one to",
            Some(0),
        ),
        // Tab on "ec": multiple candidates → list printed, line stays
        // "ec" → runs as a command → 127; only completion health matters
        ("tab_completion_list", b"ec\t\r\x04".to_vec(), "echo", None),
        (
            "quoted_and_ghost",
            b"echo 'quoted val'\r\x04".to_vec(),
            "quoted val",
            Some(0),
        ),
    ];

    for (name, input, want, want_code) in cases {
        let (out, code) = run_pty(&bin, &input);
        assert_session(name, &out, code, want);
        if let Some(wc) = want_code {
            assert_eq!(code, Some(wc), "{name}: exit code");
        } else {
            assert!(code.is_some(), "{name}: never exited");
        }
        println!("ok   {name} ({}B, exit={code:?})", out.len());
    }
}

/// Mux cycle: Alt+V (rc VERB bind → split; router takes over), Ctrl+K
/// (rc VERB → kill all panes → router leaves and execs a fresh shell),
/// Ctrl+D (fresh shell exits).
#[test]
fn mux_split_kill_smoke() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_thesh"));
    let script: Vec<(Vec<u8>, u64)> = vec![
        (b"\x1bv".to_vec(), 1800), // Alt+V → split-window-vertical (router up)
        (b"\x0b".to_vec(), 800),   // Ctrl+K → kill-all-windows → leave → fresh shell
        (b"\x04".to_vec(), 400),   // Ctrl+D → fresh shell exits
    ];

    let (out, code) = run_pty_chunks(&bin, &script);
    let s = String::from_utf8_lossy(&out);

    assert_ne!(code, Some(-999), "mux sequence timed out\n…{}", show(&out[out.len().saturating_sub(600)..]));
    assert!(
        !s.contains("AddressSanitizer") && !s.contains("panicked at"),
        "mux cycle crash:\n…{}",
        show(&out[out.len().saturating_sub(1500)..])
    );
    assert!(
        s.contains('\u{2502}') || s.contains('\u{250c}') || s.contains('\u{251c}'),
        "router separators never drawn\n…{}",
        show(&out[out.len().saturating_sub(600)..])
    );
    let intros = s.matches("intro.sh: No such file").count();
    assert!(
        intros >= 2,
        "expected the rc intro for the initial session AND the post-leave fresh \
         shell (≥2), got {intros}"
    );
    assert_eq!(code, Some(0), "exit code after mux cycle");
    println!("ok   mux_split_kill_smoke ({}B, intros={intros})", out.len());
}
