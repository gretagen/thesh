//! Port of `src/mux.c` — the built-in tiling terminal multiplexer
//! (fork-on-split router, pty panes, layout, mouse, scrollback).
//!
//! Locking rules:
//! * `fork_on_split` must run **without** the MUX lock (the child shell
//!   re-enters the editor and would inherit a locked mutex);
//! * `pane_spawn`'s forkpty child only execs — it never takes the lock,
//!   so spawning under the lock is safe;
//! * the router loop calls editor/exec with the MUX lock **released**
//!   (they block on input and re-enter mux helpers);
//! * the SIGWINCH flag is a free-standing [`AtomicBool`] (signal-handler
//!   safety), mirroring C's `volatile sig_atomic_t`.
//!
//! Identity: C compares `MuxPane*`; panes carry a stable uid so `Vec`
//! removal never invalidates references held by the tree/focus/mouse.

use crate::bind::BindKind;
use crate::keys::*;
use crate::{config, editor, exec, term, util};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub const MUX_MAX_PANES: i32 = 128;
const MUX_MIN_W: i32 = 8;
const MUX_MIN_H: i32 = 6;
const NAME: &str = "thesh";

/// SIGWINCH flag (C: `winch_flag`) — signal-safe, lock-free.
static WINCH_FLAG: AtomicBool = AtomicBool::new(false);

/// Port of `mux_notify_winch()` (called from the SIGWINCH handler).
pub fn notify_winch() {
    WINCH_FLAG.store(true, Ordering::SeqCst);
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

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

extern "C" {
    // glibc ships these in libc; declared explicitly because the libc
    // crate doesn't bind openpty/forkpty on every target.
    fn openpty(
        amaster: *mut libc::c_int,
        aslave: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winp: *const libc::winsize,
    ) -> libc::c_int;
    fn forkpty(
        amaster: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winp: *const libc::winsize,
    ) -> libc::pid_t;
}

// ── Panes / tree / state ───────────────────────────────────────────────

struct Pane {
    uid: u32,
    scr: crate::screen::MuxScreen,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    master: i32,
    pid: libc::pid_t,
}

struct Node {
    id: usize,
    ch: [Option<Box<Node>>; 2],
    vertical: bool, // 1 = side-by-side (│ divider)
    pct: i32,
    pane: Option<u32>, // leaf only (pane uid)
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.ch[0].is_none()
    }
}

struct MuxState {
    active: bool, // this process is the router
    root: Option<Box<Node>>,
    panes: Vec<Pane>,
    focus: Option<u32>,
    next_uid: u32,
    next_node_id: usize,
    mouse_fwd_press: bool,   // pass-through click is down
    click_pane: Option<u32>, // pane that received the press
    mouse_sent: i32,         // mouse level on the real tty
    mouse_seq: Vec<u8>,      // translated SGR event
    park_force: bool,        // re-park even at same spot
    seps_dirty: bool,        // separators recolor only
    parked_row: i32,
    parked_col: i32,
    parked_vis: i32,
}

impl MuxState {
    fn new() -> MuxState {
        MuxState {
            active: false,
            root: None,
            panes: Vec::new(),
            focus: None,
            next_uid: 1,
            next_node_id: 1,
            mouse_fwd_press: false,
            click_pane: None,
            mouse_sent: -1,
            mouse_seq: Vec::new(),
            park_force: true,
            seps_dirty: false,
            parked_row: -1,
            parked_col: -1,
            parked_vis: -1,
        }
    }

    fn focus_pane(&self) -> Option<&Pane> {
        let f = self.focus?;
        self.panes.iter().find(|p| p.uid == f)
    }

    fn pane_by_uid(&self, uid: u32) -> Option<&Pane> {
        self.panes.iter().find(|p| p.uid == uid)
    }
}

static MUX: Mutex<Option<MuxState>> = Mutex::new(None);

// The router is single-threaded and the screen's reply closure only
// captures a pty fd — MuxState is Send in practice even though
// `Box<dyn FnMut>` doesn't declare it. (If screen.rs later adds `Send`
// to ReplyFn itself, this can be dropped.)
unsafe impl Send for MuxState {}

fn with<R>(f: impl FnOnce(&mut MuxState) -> R) -> R {
    let mut g = MUX.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(MuxState::new))
}

fn with_ref<R>(f: impl FnOnce(&MuxState) -> R) -> R {
    let mut g = MUX.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(MuxState::new))
}

/// Port of `mux_active()`.
pub fn active() -> bool {
    with_ref(|st| st.active)
}

// ── tree helpers ───────────────────────────────────────────────────────

fn node_new(st: &mut MuxState, pane: Option<u32>) -> Node {
    Node {
        id: {
            st.next_node_id += 1;
            st.next_node_id - 1
        },
        ch: [None, None],
        vertical: false,
        pct: 0,
        pane,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    }
}

/// Port of `node_find_pane()` — id of the leaf holding `uid`.
fn leaf_id_for_pane(n: &Node, uid: u32) -> Option<usize> {
    if n.pane == Some(uid) {
        return Some(n.id);
    }
    for slot in [0usize, 1] {
        if let Some(c) = &n.ch[slot] {
            if let Some(id) = leaf_id_for_pane(c, uid) {
                return Some(id);
            }
        }
    }
    None
}

/// Port of `node_parent()`.
fn parent_id(n: &Node, target: usize) -> Option<usize> {
    for slot in [0usize, 1] {
        if n.ch[slot].as_ref().map_or(false, |c| c.id == target) {
            return Some(n.id);
        }
    }
    for slot in [0usize, 1] {
        if let Some(c) = &n.ch[slot] {
            if let Some(id) = parent_id(c, target) {
                return Some(id);
            }
        }
    }
    None
}

/// Port of `collect_leaves()` — pane uids in tree order.
fn collect_leaves(n: &Node, out: &mut Vec<u32>) {
    if n.is_leaf() {
        if let Some(p) = n.pane {
            if (out.len() as i32) < MUX_MAX_PANES {
                out.push(p);
            }
        }
        return;
    }
    for slot in [0usize, 1] {
        if let Some(c) = &n.ch[slot] {
            collect_leaves(c, out);
        }
    }
}

/// Port of `first_leaf()`.
fn first_leaf(n: &Node) -> &Node {
    let mut n = n;
    while let Some(c) = &n.ch[0] {
        n = c;
    }
    n
}

fn first_leaf_uid(n: &Node) -> Option<u32> {
    first_leaf(n).pane
}

fn node_rect(n: &Node, target: usize) -> Option<(i32, i32)> {
    if n.id == target {
        return Some((n.w, n.h));
    }
    for slot in [0usize, 1] {
        if let Some(c) = &n.ch[slot] {
            if let Some(r) = node_rect(c, target) {
                return Some(r);
            }
        }
    }
    None
}

/// Substitute node-id `old_id` with `sub` (old dropped with its
/// subtree) — C's `replace_node()`. Returns false if not found (sub is
/// then dropped by the caller's choice — we panic-free: keep sub).
fn replace_node(n: &mut Node, old_id: usize, sub: Box<Node>) -> Result<(), Box<Node>> {
    for slot in [0usize, 1] {
        if n.ch[slot].as_ref().map_or(false, |c| c.id == old_id) {
            n.ch[slot] = Some(sub);
            return Ok(());
        }
    }
    for slot in [0usize, 1] {
        if n.ch[slot].is_some() {
            let mut child = n.ch[slot].take().unwrap();
            let r = replace_node(&mut child, old_id, sub);
            n.ch[slot] = Some(child);
            match r {
                Ok(()) => return Ok(()),
                Err(s) => return Err(s),
            }
        }
    }
    Err(sub)
}

fn subtree_has(n: Option<&Node>, target: usize) -> bool {
    match n {
        None => false,
        Some(n) => n.id == target
            || subtree_has(n.ch[0].as_deref(), target)
            || subtree_has(n.ch[1].as_deref(), target),
    }
}

/// Port of the split graft: put `mk(target_node)` where node-id
/// `target` stood, moving the target subtree into the new node.
fn wrap_node(n: &mut Node, target: usize, mk: impl FnOnce(Node) -> Node) -> bool {
    for slot in [0usize, 1] {
        if n.ch[slot].as_ref().map_or(false, |c| c.id == target) {
            let old = n.ch[slot].take().unwrap();
            n.ch[slot] = Some(Box::new(mk(*old)));
            return true;
        }
    }
    // descend into the single child that holds the target (mk is moved
    // exactly once — FnOnce)
    for slot in [0usize, 1] {
        if subtree_has(n.ch[slot].as_deref(), target) {
            let mut child = n.ch[slot].take().unwrap();
            let done = wrap_node(&mut child, target, mk);
            n.ch[slot] = Some(child);
            return done;
        }
    }
    false
}

/// Take the *sibling* of the leaf `leaf_id` out of its parent (the leaf
/// node itself stays, to be dropped with the parent). C: reading
/// `par->ch[..]` before `replace_node(par, sib)`.
fn take_sibling(n: &mut Node, leaf_id: usize) -> Option<Node> {
    for slot in [0usize, 1] {
        if n.ch[slot].as_ref().map_or(false, |c| c.id == leaf_id) {
            let other = 1 - slot;
            return n.ch[other].take().map(|b| *b);
        }
    }
    for slot in [0usize, 1] {
        if n.ch[slot].is_some() {
            let mut child = n.ch[slot].take().unwrap();
            let r = take_sibling(&mut child, leaf_id);
            n.ch[slot] = Some(child);
            if r.is_some() {
                return r;
            }
        }
    }
    None
}

fn mark_all_panes(st: &mut MuxState) {
    for p in &mut st.panes {
        p.scr.all_dirty = 1;
        for d in p.scr.dirty.iter_mut() {
            *d = 1;
        }
    }
}

// ── layout ─────────────────────────────────────────────────────────────

fn pane_set_rect(p: &mut Pane, x: i32, y: i32, w: i32, h: i32) {
    p.x = x;
    p.y = y;
    p.w = w;
    p.h = h;
    let iw = if w > 0 { w } else { 1 };
    let ih = if h > 0 { h } else { 1 };
    if p.scr.w != iw || p.scr.h != ih {
        util::muxdbg(&format!(
            "rect: pane uid={} -> {},{} {}x{} (scr {},{} -> {},{}; cx,cy={},{})\n",
            p.uid, x, y, iw, ih, p.scr.w, p.scr.h, iw, ih, p.scr.cx, p.scr.cy
        ));
    }
    if p.scr.cells.is_empty() {
        p.scr.init(iw, ih);
    } else {
        p.scr.resize(iw, ih);
    }
    if p.master >= 0 {
        let ws = libc::winsize {
            ws_row: ih.clamp(0, u16::MAX as i32) as u16,
            ws_col: iw.clamp(0, u16::MAX as i32) as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(p.master, libc::TIOCSWINSZ, &ws) };
    }
}

fn layout_rec(st: &mut MuxState, n: &mut Node, x: i32, y: i32, w: i32, h: i32) {
    n.x = x;
    n.y = y;
    n.w = w;
    n.h = h;
    if n.is_leaf() {
        let uid = n.pane.unwrap_or(0);
        if let Some(idx) = st.panes.iter().position(|p| p.uid == uid) {
            let mut p = st.panes.remove(idx); // order-preserving
            pane_set_rect(&mut p, x, y, w, h);
            st.panes.insert(idx, p);
        }
        return;
    }
    // children + the 1-cell divider between them = the region
    let vertical = n.vertical;
    let pct = n.pct;
    let mut c0 = n.ch[0].take().expect("layout: child 0");
    let mut c1 = n.ch[1].take().expect("layout: child 1");
    if vertical {
        let mut l = (w as i64 * pct as i64 / 100) as i32;
        if l < 1 {
            l = 1;
        }
        if w >= 3 && l > w - 2 {
            l = w - 2; // both sides >= 1 cell
        }
        layout_rec(st, &mut c0, x, y, l, h);
        layout_rec(st, &mut c1, x + l + 1, y, w - l - 1, h);
    } else {
        let mut t = (h as i64 * pct as i64 / 100) as i32;
        if t < 1 {
            t = 1;
        }
        if h >= 3 && t > h - 2 {
            t = h - 2;
        }
        layout_rec(st, &mut c0, x, y, w, t);
        layout_rec(st, &mut c1, x, y + t + 1, w, h - t - 1);
    }
    n.ch[0] = Some(c0);
    n.ch[1] = Some(c1);
}

fn layout_apply(st: &mut MuxState, x: i32, y: i32, w: i32, h: i32) {
    let Some(mut root) = st.root.take() else { return };
    layout_rec(st, &mut root, x, y, w, h);
    st.root = Some(root);
}

/// Port of `term_size()` — ioctl on stdout (right fd in every role).
pub fn term_size(cols: &mut i32, rows: &mut i32) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    *cols = if ws.ws_col == 0 { 80 } else { ws.ws_col as i32 };
    *rows = if ws.ws_row == 0 { 24 } else { ws.ws_row as i32 };
}

// ── pane reply (DSR/DA from children) ──────────────────────────────────

fn write_fd(fd: i32, mut b: &[u8]) {
    while !b.is_empty() {
        let r = unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        b = &b[r as usize..];
    }
}

// ── activation: fork-on-split (tmux process model) ─────────────────────

fn self_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut buf = vec![0u8; 4096];
        let n = unsafe {
            libc::readlink(
                b"/proc/self/exe\0".as_ptr().cast(),
                buf.as_mut_ptr().cast(),
                buf.len() - 1,
            )
        };
        if n > 0 {
            buf.truncate(n as usize);
            String::from_utf8_lossy(&buf).into_owned()
        } else {
            NAME.to_string()
        }
    })
}

/// Port of `fork_on_split()`: 1 in the parent (router), 0 in the child
/// (pane-backed shell), −1 on error. NO mux lock is held here — the
/// child re-enters the editor and would deadlock on an inherited lock.
fn fork_on_split() -> i32 {
    let path = self_path();

    let mut cols = 0;
    let mut rows = 0;
    term_size(&mut cols, &mut rows);

    let ws = libc::winsize {
        ws_row: (if rows > 0 { rows } else { 24 }).clamp(0, u16::MAX as i32) as u16,
        ws_col: (if cols > 0 { cols } else { 80 }).clamp(0, u16::MAX as i32) as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    if unsafe { openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) } < 0 {
        eprintln!("{NAME}: mux: openpty: {}", errno_str());
        return -1;
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("{NAME}: mux: fork: {}", errno_str());
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return -1;
    }

    if pid == 0 {
        // child: continue as the interactive shell, now pane-backed
        unsafe {
            libc::close(master);
            libc::setsid();
            if libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
                util::muxdbg(&format!("fork-on-split: TIOCSCTTY: {}\n", errno_str()));
            }
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
            libc::setenv(cstr("THESH_MUX").as_ptr(), cstr("1").as_ptr(), 1);
        }
        let _ = term::enter_raw(); // the pane pty starts cooked
        editor::reset_anchor(); // old screen coords are void
        util::muxdbg(&format!(
            "fork-on-split: child shell on pane pty (pid={})\n",
            unsafe { libc::getpid() }
        ));
        return 0;
    }

    unsafe { libc::close(slave) };

    // The router owns the real terminal from here. Activation usually
    // happens inside exec_unit's cooked bracket — re-enter raw ourselves
    // (idempotent).
    let _ = term::enter_raw();

    with(|st| {
        let uid = st.next_uid;
        st.next_uid += 1;
        let mut scr = crate::screen::MuxScreen::default();
        scr.init(ws.ws_col as i32, ws.ws_row as i32);
        let m = master;
        scr.reply = Some(Box::new(move |b: &[u8]| write_fd(m, b)));
        util::muxdbg(&format!("pane-map uid={uid} pid={pid} (forked shell)\n"));

        let mut rootn = node_new(st, Some(uid));
        rootn.x = 0;
        rootn.y = 0;
        rootn.w = cols;
        rootn.h = rows;
        st.root = Some(Box::new(rootn));
        st.panes.clear();
        st.panes.push(Pane {
            uid,
            scr,
            x: 0,
            y: 0,
            w: cols,
            h: rows,
            master,
            pid,
        });
        st.focus = Some(uid);
        st.active = true;
        let _ = path;
        util::muxdbg(&format!(
            "router: fork-on-split, original shell = pane pid={pid}\n"
        ));
    });
    1
}

// ── children ───────────────────────────────────────────────────────────

/// Port of `pane_spawn()`. The forkpty child only sets signals/env and
/// execs — it never touches the mux state, so holding the lock across
/// this fork is safe.
fn pane_spawn(w: i32, h: i32) -> Option<Pane> {
    let mut master: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: (if h > 0 { h } else { 1 }).clamp(0, u16::MAX as i32) as u16,
        ws_col: (if w > 0 { w } else { 1 }).clamp(0, u16::MAX as i32) as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let pid = unsafe { forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if pid < 0 {
        return None;
    }
    let mut scr = crate::screen::MuxScreen::default();
    scr.init(ws.ws_col as i32, ws.ws_row as i32);
    let m = master;
    scr.reply = Some(Box::new(move |b: &[u8]| write_fd(m, b)));
    if pid > 0 {
        util::muxdbg(&format!(
            "pane-map scr={}x{} pid={pid} (spawned)\n",
            ws.ws_col, ws.ws_row
        ));
    }
    if pid == 0 {
        // child → new thesh on this pane's pty
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGWINCH, libc::SIG_DFL);
            libc::setenv(cstr("THESH_MUX").as_ptr(), cstr("1").as_ptr(), 1);
            // verify /dev/tty the way child apps will
            let t = libc::open(cstr("/dev/tty").as_ptr(), libc::O_RDWR);
            if t < 0 {
                libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0);
                let t2 = libc::open(cstr("/dev/tty").as_ptr(), libc::O_RDWR);
                if t2 < 0 {
                    util::muxdbg(&format!(
                        "pane_spawn: no /dev/tty (ctty missing): {}\n",
                        errno_str()
                    ));
                } else {
                    libc::close(t2);
                }
            } else {
                libc::close(t);
            }
            let p = cstr(self_path());
            libc::execl(p.as_ptr(), p.as_ptr(), std::ptr::null::<libc::c_char>());
            libc::_exit(127);
        }
    }
    Some(Pane {
        uid: 0, // assigned by the caller
        scr,
        x: 0,
        y: 0,
        w,
        h,
        master,
        pid,
    })
}

/// Port of `pane_free()`: SIGHUP, wait up to 500 ms, SIGKILL. Dropping
/// the Pane frees its MuxScreen (Rust Drop ≙ mux_screen_free).
fn pane_free(p: Pane) {
    if p.master >= 0 {
        unsafe { libc::close(p.master) };
    }
    if p.pid > 0 {
        unsafe { libc::kill(p.pid, libc::SIGHUP) };
        for _ in 0..50 {
            let r = unsafe { libc::waitpid(p.pid, std::ptr::null_mut(), libc::WNOHANG) };
            if r == p.pid {
                return; // drop(p) frees the screen
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe {
            libc::kill(p.pid, libc::SIGKILL);
            libc::waitpid(p.pid, std::ptr::null_mut(), 0);
        }
    }
}

// ── split / close ──────────────────────────────────────────────────────

/// Port of `mux_split()`.
pub fn split(vertical: bool, pct: i32) -> i32 {
    if !config::with(|c| c.multiplexer) {
        eprintln!("{NAME}: mux: disabled in config");
        return -1;
    }
    let mut activated = false;
    if !active() {
        let r = fork_on_split(); // no lock held
        if r < 0 {
            return -1;
        }
        if r == 0 {
            return 0; // child: pane shell, nothing to split
        }
        activated = true; // parent: router, shell pane = #1
    }

    let rc = with(|st| {
        let lim = {
            let l = config::with(|c| c.mux_window_limit);
            if l != 0 {
                l
            } else {
                MUX_MAX_PANES
            }
        };
        let lim = lim.min(MUX_MAX_PANES);
        if (st.panes.len() as i32) >= lim {
            eprintln!("{NAME}: mux: too many panes ({lim})");
            return -1;
        }
        let pct = pct.clamp(10, 90);

        let f_uid = match st.focus {
            Some(u) => u,
            None => return -1,
        };
        let (f_id, fw, fh) = {
            let root = match st.root.as_deref() {
                Some(r) => r,
                None => return -1,
            };
            let Some(f_id) = leaf_id_for_pane(root, f_uid) else { return -1 };
            let Some((fw, fh)) = node_rect(root, f_id) else { return -1 };
            (f_id, fw, fh)
        };
        // both sides + the divider cell must fit
        if vertical && fw < 2 * MUX_MIN_W + 1 {
            eprintln!("{NAME}: mux: pane too small to split");
            return -1;
        }
        if !vertical && fh < 2 * MUX_MIN_H + 1 {
            eprintln!("{NAME}: mux: pane too small to split");
            return -1;
        }

        let nw = if vertical { fw * pct / 100 } else { fw };
        let nh = if vertical { fh } else { fh * pct / 100 };
        let mut np = match pane_spawn(nw, nh) {
            Some(p) => p,
            None => {
                eprintln!("{NAME}: mux: fork failed: {}", errno_str());
                return -1;
            }
        };
        let uid = st.next_uid;
        st.next_uid += 1;
        np.uid = uid;
        st.panes.push(np);

        let mut leaf = node_new(st, Some(uid));
        leaf.w = nw;
        leaf.h = nh;

        // put `par` where f stood — f stays alive as par->ch[0]
        // (C: `Node *fp = node_parent(f); if (!fp) root = par;` — when the
        // focus leaf IS the root, the new parent replaces it directly)
        let mut root = st.root.take().unwrap();
        if root.id == f_id {
            let mut par = node_new(st, None);
            par.vertical = vertical;
            par.pct = pct;
            par.ch = [Some(root), Some(Box::new(leaf))];
            st.root = Some(Box::new(par));
        } else {
            let mut par = node_new(st, None);
            par.vertical = vertical;
            par.pct = pct;
            let ok = wrap_node(&mut root, f_id, |f| {
                par.ch = [Some(Box::new(f)), Some(Box::new(leaf))];
                par
            });
            st.root = Some(root);
            if !ok {
                // unreachable with a consistent tree — the pane is already
                // spawned, so keep it and let the next op reconcile
                util::muxdbg("split: graft target vanished\n");
            }
        }

        let mut cols = 0;
        let mut rows = 0;
        term_size(&mut cols, &mut rows);
        layout_apply(st, 0, 0, cols, rows);
        mark_all_panes(st);
        0
    });

    if activated {
        router_loop(); // never returns
    }
    rc
}

/// Port of `close_pane()`: splice its parent out of the tree, fix focus.
fn close_pane(st: &mut MuxState, victim_uid: u32) -> i32 {
    let Some(mut root) = st.root.take() else { return -1 };
    let Some(n_id) = leaf_id_for_pane(&root, victim_uid) else {
        st.root = Some(root);
        return -1;
    };

    if root.id == n_id {
        // the only pane: kill it — the router loop's top sees 0 panes
        // and leaves the mux (C keeps the node; shutdown frees the tree)
        let pos = st.panes.iter().position(|p| p.uid == victim_uid);
        st.focus = None;
        if let Some(i) = pos {
            let p = st.panes.remove(i);
            pane_free(p);
        }
        st.root = Some(root);
        util::muxdbg("close: last pane gone, router will leave\n");
        return 0;
    }

    let par_id = match parent_id(&root, n_id) {
        Some(p) => p,
        None => {
            st.root = Some(root);
            return -1;
        }
    };
    let sib = match take_sibling(&mut root, n_id) {
        Some(s) => s,
        None => {
            st.root = Some(root);
            return -1;
        }
    };
    let was_focus = st.focus == Some(victim_uid);
    let sib_focus = first_leaf_uid(&sib);

    // splice par out: root itself may BE par
    if par_id == root.id {
        root = Box::new(sib);
    } else if let Err(back) = replace_node(&mut root, par_id, Box::new(sib)) {
        // not found (shouldn't happen): restore and abort
        let _ = back;
        st.root = Some(root);
        return -1;
    }

    let pos = st.panes.iter().position(|p| p.uid == victim_uid);
    if let Some(i) = pos {
        let p = st.panes.remove(i);
        if was_focus {
            st.focus = sib_focus;
        }
        if st.focus.is_none() {
            st.focus = st.root.as_deref().and_then(first_leaf_uid);
        }
        pane_free(p);
    } else if was_focus {
        st.focus = sib_focus;
    }

    st.root = Some(root);
    let mut cols = 0;
    let mut rows = 0;
    term_size(&mut cols, &mut rows);
    layout_apply(st, 0, 0, cols, rows);
    mark_all_panes(st);
    0
}

/// Port of `mux_close()`.
pub fn close() -> i32 {
    if !active() {
        eprintln!("{NAME}: mux: not multiplexed");
        return -1;
    }
    let f = with_ref(|st| st.focus);
    let Some(f) = f else { return -1 };
    with(|st| close_pane(st, f))
}

/// Port of `mux_kill_all()`.
pub fn kill_all() -> i32 {
    if !active() {
        eprintln!("{NAME}: mux: not multiplexed");
        return -1;
    }
    with(|st| {
        let mut result = 0;
        loop {
            let first = st.panes.first().map(|p| p.uid);
            let Some(uid) = first else { break };
            let rc = close_pane(st, uid);
            if rc != 0 {
                result = rc;
                break;
            }
        }
        result
    })
}

// ── focus ──────────────────────────────────────────────────────────────

fn focus_set(st: &mut MuxState, uid: u32) {
    if st.focus == Some(uid) {
        return;
    }
    st.focus = Some(uid);
    st.mouse_fwd_press = false; // a new pane needs a new press
    st.click_pane = None;
    // Only the separator colours change with focus
    st.seps_dirty = true;
    if let Some(p) = st.pane_by_uid(uid) {
        util::muxdbg(&format!(
            "focus -> pane uid={uid} at {},{} {}x{}\n",
            p.x, p.y, p.w, p.h
        ));
    }
}

/// Port of `mux_focus_cycle()`.
pub fn focus_cycle(delta: i32) -> i32 {
    if !active() {
        return -1;
    }
    let (cnt, idx, target) = with_ref(|st| {
        let mut lv = Vec::new();
        if let Some(r) = st.root.as_deref() {
            collect_leaves(r, &mut lv);
        }
        if lv.len() < 2 {
            return (lv.len(), 0, None);
        }
        let mut idx = 0;
        for (i, &u) in lv.iter().enumerate() {
            if Some(u) == st.focus {
                idx = i;
            }
        }
        let cnt = lv.len() as i32;
        let mut i = (idx as i32 + delta) % cnt;
        if i < 0 {
            i += cnt;
        }
        (lv.len(), i as usize, Some(lv[i as usize]))
    });
    if cnt < 2 {
        return 0;
    }
    if let Some(u) = target {
        with(|st| focus_set(st, u));
    }
    let _ = idx;
    0
}

/// Port of `mux_focus_dir()`.
pub fn focus_dir(dir: i32) -> i32 {
    if !active() {
        return -1;
    }
    let target = with_ref(|st| {
        let f = st.focus_pane()?;
        let mut lv = Vec::new();
        if let Some(r) = st.root.as_deref() {
            collect_leaves(r, &mut lv);
        }
        let mut best: Option<u32> = None;
        let mut best_dist = i32::MAX;
        for &q in &lv {
            let Some(qp) = st.pane_by_uid(q) else { continue };
            if qp.uid == f.uid {
                continue;
            }
            let overlap = if dir <= 1 {
                qp.y < f.y + f.h && qp.y + qp.h > f.y
            } else {
                qp.x < f.x + f.w && qp.x + qp.w > f.x
            };
            if !overlap {
                continue;
            }
            let dist = match dir {
                0 => {
                    if qp.x + qp.w <= f.x {
                        f.x - (qp.x + qp.w)
                    } else {
                        -1
                    }
                }
                1 => {
                    if qp.x >= f.x + f.w {
                        qp.x - (f.x + f.w)
                    } else {
                        -1
                    }
                }
                2 => {
                    if qp.y + qp.h <= f.y {
                        f.y - (qp.y + qp.h)
                    } else {
                        -1
                    }
                }
                _ => {
                    if qp.y >= f.y + f.h {
                        qp.y - (f.y + f.h)
                    } else {
                        -1
                    }
                }
            };
            if dist >= 0 && dist < best_dist {
                best_dist = dist;
                best = Some(q);
            }
        }
        best
    });
    match target {
        Some(u) => {
            with(|st| focus_set(st, u));
            0
        }
        None => 0, // edge: no-op
    }
}

// ── scrollback view ────────────────────────────────────────────────────

fn scroll_pane(st: &mut MuxState, delta: i32) {
    let Some(f) = st.focus else { return };
    let Some(idx) = st.panes.iter().position(|p| p.uid == f) else { return };
    let s = &mut st.panes[idx].scr;
    if delta == 0 {
        if s.view != 0 {
            s.view = 0;
            s.all_dirty = 1;
        }
        return;
    }
    s.view += delta;
    if s.view > s.sb_count {
        s.view = s.sb_count;
    }
    if s.view < 0 {
        s.view = 0;
    }
    s.all_dirty = 1;
}

/// Port of `mux_scroll()`.
pub fn scroll(delta: i32) {
    if active() && with_ref(|st| st.focus.is_some()) {
        with(|st| scroll_pane(st, delta));
    }
}

// ── input forwarding ───────────────────────────────────────────────────

/// Port of `mux_forward()`.
pub fn forward(b: &[u8]) {
    with(|st| {
        if !st.active {
            return;
        }
        let Some(f) = st.focus else { return };
        let Some(p) = st.pane_by_uid(f) else { return };
        if p.master >= 0 {
            write_fd(p.master, b);
        }
    });
}

// ── resize ─────────────────────────────────────────────────────────────

fn do_resize(st: &mut MuxState) {
    let Some(root) = st.root.as_deref() else { return };
    let (rw, rh) = (root.w, root.h);
    let mut cols = 0;
    let mut rows = 0;
    term_size(&mut cols, &mut rows);
    if rw == cols && rh == rows {
        return;
    }
    layout_apply(st, 0, 0, cols, rows);
    mark_all_panes(st);
    st.park_force = true; // the terminal clamped too
    st.mouse_sent = -1; // re-assert mouse on next pump
}

fn resize_check_impl(st: &mut MuxState) {
    if st.active && WINCH_FLAG.swap(false, Ordering::SeqCst) {
        do_resize(st);
    }
}

/// Port of `mux_resize_check()` — consume the SIGWINCH flag.
pub fn resize_check() {
    with(resize_check_impl);
}

fn on_cont_impl(st: &mut MuxState) {
    if !crate::run::conted() {
        return;
    }
    let _ = term::enter_raw(); // idempotent
    if st.active {
        st.mouse_sent = -1; // terminal dropped our modes
        st.park_force = true; // cursor state is unknown now
    }
    crate::run::WINCHED.store(true, Ordering::SeqCst); // re-layout + redraw
    notify_winch();
}

/// Port of `thesh_on_cont()` — re-assert raw/mouse after SIGCONT.
pub fn on_cont() {
    with(on_cont_impl);
}

// ── tmux-style separators (crosshair) ──────────────────────────────────

/// Port of `tf()` — batched terminal write (C caps at 640; our strings
/// are format results far below that).
fn tf(s: &str) {
    util::tty_buf_write(s.as_bytes());
}

/// Connectivity bits: 1 up, 2 down, 4 left, 8 right.
fn sep_bits(map: &[u8], cols: i32, rows: i32, x: i32, y: i32) -> u8 {
    let mut b = 0u8;
    if y > 0 && map[((y - 1) * cols + x) as usize] != 0 {
        b |= 1;
    }
    if y + 1 < rows && map[((y + 1) * cols + x) as usize] != 0 {
        b |= 2;
    }
    if x > 0 && map[(y * cols + (x - 1)) as usize] != 0 {
        b |= 4;
    }
    if x + 1 < cols && map[(y * cols + (x + 1)) as usize] != 0 {
        b |= 8;
    }
    b
}

/// tmux crosshair glyphs by connectivity bits:
/// 1 up, 2 down, 4 left, 8 right.
fn sep_glyph_exact(b: u8) -> Option<&'static str> {
    match b {
        1 | 2 | 3 => Some("\u{2502}"),   // │
        4 | 8 | 12 => Some("\u{2500}"),  // ─
        10 => Some("\u{250c}"),          // ┌
        6 => Some("\u{2510}"),           // ┐
        9 => Some("\u{2514}"),           // └
        5 => Some("\u{2518}"),           // ┘
        11 => Some("\u{251c}"),          // ├
        7 => Some("\u{2524}"),           // ┤
        14 => Some("\u{252c}"),          // ┬
        13 => Some("\u{2534}"),          // ┴
        15 => Some("\u{253c}"),          // ┼
        _ => None,
    }
}

/// Collect separator positions in tree order (C's sep_collect).
fn sep_collect(n: &Node, out: &mut Vec<(i32, i32)>) {
    if n.is_leaf() {
        return;
    }
    let c0 = n.ch[0].as_deref().expect("sep: child 0");
    if n.vertical {
        let cx = c0.x + c0.w; // divider column
        let mut r = n.y;
        while r < n.y + n.h {
            out.push((cx, r));
            r += 1;
        }
    } else {
        let cy = c0.y + c0.h; // divider row
        let mut c = n.x;
        while c < n.x + n.w {
            out.push((c, cy));
            c += 1;
        }
    }
    for slot in [0usize, 1] {
        if let Some(c) = &n.ch[slot] {
            sep_collect(c, out);
        }
    }
}

/// Port of `pane_at()` — exact hit, then snap to nearest (wheel).
fn pane_at(st: &MuxState, x: i32, y: i32) -> Option<u32> {
    for p in &st.panes {
        if x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h {
            return Some(p.uid);
        }
    }
    let mut best: Option<u32> = None;
    let mut best_d = i32::MAX;
    for p in &st.panes {
        let (mut dx, mut dy) = (0, 0);
        if x < p.x {
            dx = p.x - x;
        } else if x >= p.x + p.w {
            dx = x - (p.x + p.w) + 1;
        }
        if y < p.y {
            dy = p.y - y;
        } else if y >= p.y + p.h {
            dy = y - (p.y + p.h) + 1;
        }
        let d = dx * dx + dy * dy;
        if d < best_d {
            best_d = d;
            best = Some(p.uid);
        }
    }
    best
}

/// Exact pane hit: None on a divider (button events never snap).
fn pane_at_exact(st: &MuxState, x: i32, y: i32) -> Option<u32> {
    st.panes
        .iter()
        .find(|p| x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h)
        .map(|p| p.uid)
}

/// `neighbour pane is focused?` (uses the snapping pane_at, like C).
fn sep_hot(st: &MuxState, x: i32, y: i32) -> bool {
    let Some(f) = st.focus else { return false };
    pane_at(st, x - 1, y) == Some(f)
        || pane_at(st, x + 1, y) == Some(f)
        || pane_at(st, x, y - 1) == Some(f)
        || pane_at(st, x, y + 1) == Some(f)
}

fn sep_color(_st: &MuxState, hot: bool) -> String {
    let c = config::with(|cfg| {
        if hot {
            cfg.col_mux_active.clone()
        } else {
            cfg.col_mux_inactive.clone()
        }
    });
    match c {
        Some(s) if !s.is_empty() => s,
        _ => {
            if hot {
                "34".to_string() // tmux: blue
            } else {
                "39".to_string() // default
            }
        }
    }
}

/// Port of `draw_separators()`.
fn draw_separators(st: &MuxState) {
    let Some(root) = st.root.as_deref() else { return };
    if st.panes.is_empty() {
        return;
    }
    let (mut cols, mut rows) = (0, 0);
    term_size(&mut cols, &mut rows);

    let mut cells: Vec<(i32, i32)> = Vec::new();
    sep_collect(root, &mut cells);
    if cells.is_empty() {
        return;
    }

    let mut map = vec![0u8; (cols * rows).max(0) as usize];
    for &(x, y) in &cells {
        if x >= 0 && x < cols && y >= 0 && y < rows {
            map[(y * cols + x) as usize] = 1;
        }
    }

    let mut i = 0;
    while i < cells.len() {
        let c0 = cells[i];
        if c0.0 < 0 || c0.0 >= cols || c0.1 < 0 || c0.1 >= rows {
            i += 1;
            continue;
        }
        let Some(g) = sep_glyph_exact(sep_bits(&map, cols, rows, c0.0, c0.1)) else {
            i += 1;
            continue;
        };
        let col = sep_color(st, sep_hot(st, c0.0, c0.1));

        // run direction within the segment
        let (mut dx, mut dy) = (0, 0);
        if i + 1 < cells.len() {
            let n = cells[i + 1];
            if n.0 == c0.0 && n.1 == c0.1 + 1 {
                dy = 1;
            } else if n.1 == c0.1 && n.0 == c0.0 + 1 {
                dx = 1;
            }
        }
        let mut j = i + 1;
        while dx != 0 || dy != 0 {
            if j >= cells.len() {
                break;
            }
            let c = cells[j];
            let pv = cells[j - 1];
            if c.0 != pv.0 + dx || c.1 != pv.1 + dy {
                break;
            }
            let Some(g2) = sep_glyph_exact(sep_bits(&map, cols, rows, c.0, c.1)) else {
                break;
            };
            if g2 != g {
                break;
            }
            if sep_color(st, sep_hot(st, c.0, c.1)) != col {
                break;
            }
            j += 1;
        }

        tf("\x1b[0m");
        // Per-cell positioning: a vertical run must step DOWN each row.
        for k in i..j {
            let (x, y) = cells[k];
            tf(&format!("\x1b[{};{}H\x1b[{}m{}", y + 1, x + 1, col, g));
        }
        tf("\x1b[0m");
        i = j;
    }
}

// ── mouse ──────────────────────────────────────────────────────────────

fn alive(st: &MuxState, uid: Option<u32>) -> bool {
    match uid {
        Some(u) => st.panes.iter().any(|p| p.uid == u),
        None => false,
    }
}

/// SGR event with pane-relative coordinates (tmux cmd_mouse_at).
fn build_mouse_seq(st: &mut MuxState, pane_uid: u32, px: i32, py: i32, button: i32, release: bool) {
    let Some(p) = st.pane_by_uid(pane_uid) else { return };
    let lx = (px - p.x + 1).clamp(1, p.w);
    let ly = (py - p.y + 1).clamp(1, p.h);
    st.mouse_seq = format!(
        "\x1b[<{};{};{}{}",
        button,
        lx,
        ly,
        if release { 'm' } else { 'M' }
    )
    .into_bytes();
}

/// Port of `mux_mouse_seq()`.
pub fn mouse_seq() -> Option<Vec<u8>> {
    with_ref(|st| Some(st.mouse_seq.clone()))
}

/// Port of `mux_mouse()`: 1 = forward to the pane under the pointer.
pub fn mouse(button: i32, x: i32, y: i32, release: i32) -> i32 {
    with(|st| {
        if !st.active {
            return 0;
        }
        let px = x - 1;
        let py = y - 1;
        if button == 64 || button == 65 {
            // wheel up/down
            let target = pane_at(st, px, py).or(st.focus);
            if let Some(t) = target {
                scroll_pane_pane(st, t, if button == 64 { 3 } else { -3 });
            }
            return 0;
        }
        if release != 0 {
            // SGR 'm'
            let f = if button == 0 { st.mouse_fwd_press } else { false };
            st.mouse_fwd_press = false;
            let p = if alive(st, st.click_pane) { st.click_pane } else { None };
            let Some(p) = p else {
                st.click_pane = None;
                return 0;
            };
            if !f {
                return 0;
            }
            build_mouse_seq(st, p, px, py, button, true);
            util::muxdbg(&format!(
                "mouse: release -> pane uid={p} seq={}\n",
                String::from_utf8_lossy(&st.mouse_seq)
            ));
            return 1; // pair with a pass-through press
        }
        if button == 0 || button == 32 {
            // left press or held drag
            let p;
            if button == 32 {
                // drag continues the press
                let Some(cp) = st.click_pane.filter(|c| alive(st, Some(*c))) else {
                    return 0;
                };
                if !st.mouse_fwd_press {
                    return 0;
                }
                let needs = st
                    .pane_by_uid(cp)
                    .map(|q| q.scr.mouse & 7)
                    .unwrap_or(0);
                if needs < 2 {
                    return 0; // needs 1002
                }
                p = cp;
            } else {
                let Some(cp) = pane_at_exact(st, px, py) else {
                    st.mouse_fwd_press = false;
                    st.click_pane = None;
                    return 0;
                };
                let q = st.pane_by_uid(cp).unwrap();
                if q.scr.view > 0 {
                    // divider/scrollback
                    st.mouse_fwd_press = false;
                    st.click_pane = None;
                    return 0;
                }
                if q.scr.mouse & 7 == 0 {
                    return 0; // never asked mouse
                }
                st.click_pane = Some(cp);
                st.mouse_fwd_press = true;
                p = cp;
            }
            build_mouse_seq(st, p, px, py, button, false);
            util::muxdbg(&format!(
                "mouse: {} {},{} -> pane uid={p} seq={}\n",
                if button == 32 { "drag" } else { "press" },
                px,
                py,
                String::from_utf8_lossy(&st.mouse_seq)
            ));
            return 1; // to that pane's program
        }
        0 // other buttons: swallow
    })
}

fn scroll_pane_pane(st: &mut MuxState, uid: u32, delta: i32) {
    let Some(idx) = st.panes.iter().position(|p| p.uid == uid) else { return };
    let s = &mut st.panes[idx].scr;
    if delta == 0 {
        if s.view != 0 {
            s.view = 0;
            s.all_dirty = 1;
        }
        return;
    }
    s.view += delta;
    if s.view > s.sb_count {
        s.view = s.sb_count;
    }
    if s.view < 0 {
        s.view = 0;
    }
    s.all_dirty = 1;
}

/// Port of `mux_forward_mouse()`.
pub fn forward_mouse(b: &[u8]) {
    with(|st| {
        let p = st.click_pane.filter(|c| alive(st, Some(*c))).or(st.focus);
        let Some(p) = p else { return };
        let Some(pane) = st.pane_by_uid(p) else { return };
        if pane.master < 0 {
            return;
        }
        util::muxdbg(&format!("forward_mouse: {} bytes -> pane uid={p}\n", b.len()));
        write_fd(pane.master, b);
    });
}

// ── pump ───────────────────────────────────────────────────────────────

/// Port of `mouse_mode_apply()`: union of pane mouse levels,
/// clear-then-set on the real tty.
fn mouse_mode_apply(st: &mut MuxState) {
    let mut lv = 1;
    for p in &st.panes {
        let m = p.scr.mouse & 7;
        if m > lv {
            lv = m;
        }
    }
    if lv == st.mouse_sent {
        return;
    }
    util::mux_term_write(b"\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l");
    if lv >= 4 {
        util::mux_term_write(b"\x1b[?1006h\x1b[?1000h\x1b[?1002h\x1b[?1003h");
    } else if lv >= 2 {
        util::mux_term_write(b"\x1b[?1006h\x1b[?1000h\x1b[?1002h");
    } else {
        util::mux_term_write(b"\x1b[?1006h\x1b[?1000h");
    }
    st.mouse_sent = lv;
    util::muxdbg(&format!("mouse-mode: level {lv} on the real terminal\n"));
}

/// drain a child's pty fully (non-blocking) so its last output shows
fn drain_dead_pane(p: &mut Pane) {
    if p.master < 0 {
        return;
    }
    let fl = unsafe { libc::fcntl(p.master, libc::F_GETFL) };
    unsafe { libc::fcntl(p.master, libc::F_SETFL, fl | libc::O_NONBLOCK) };
    let mut buf = [0u8; 8192];
    loop {
        let r = unsafe { libc::read(p.master, buf.as_mut_ptr().cast(), buf.len()) };
        if r <= 0 {
            break;
        }
        p.scr.feed(&buf[..r as usize]);
    }
    unsafe { libc::fcntl(p.master, libc::F_SETFL, fl) };
}

fn pump_impl(st: &mut MuxState) {
    if !st.active {
        return;
    }
    on_cont_impl(st);
    resize_check_impl(st);

    // pane ptys: read what's ready, detect dead children
    let mut dead: Vec<u32> = Vec::new();
    for idx in 0..st.panes.len() {
        let (master, pid) = {
            let p = &st.panes[idx];
            (p.master, p.pid)
        };
        if master >= 0 {
            let mut pf = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pf, 1, 0) };
            let ev = pf.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR);
            if ready > 0 && ev != 0 {
                let mut buf = [0u8; 8192];
                let r = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if r > 0 {
                    st.panes[idx].scr.feed(&buf[..r as usize]);
                } else if r == 0 {
                    dead.push(st.panes[idx].uid);
                    continue;
                } else {
                    let e = std::io::Error::last_os_error().raw_os_error();
                    if e != Some(libc::EAGAIN) && e != Some(libc::EINTR) {
                        dead.push(st.panes[idx].uid);
                        continue;
                    }
                }
            }
        }
        if pid > 0 {
            let r = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if r == pid {
                dead.push(st.panes[idx].uid);
            }
        }
    }
    for (i, &uid) in dead.iter().enumerate() {
        if dead[..i].contains(&uid) {
            continue;
        }
        let node_id = st
            .root
            .as_deref()
            .and_then(|r| leaf_id_for_pane(r, uid));
        let is_root = st.root.as_deref().map_or(false, |r| Some(r.id) == node_id);
        if let Some(idx) = st.panes.iter().position(|p| p.uid == uid) {
            drain_dead_pane(&mut st.panes[idx]);
        }
        if node_id.is_some() && !is_root {
            close_pane(st, uid);
        }
    }

    // repaint panes; separators only when layout/focus changed
    let layout_dirty = st.panes.iter().any(|p| p.scr.all_dirty != 0);

    let mut painted = false;
    for idx in 0..st.panes.len() {
        let (x, y, w, h, uid) = {
            let p = &st.panes[idx];
            (p.x, p.y, p.w, p.h, p.uid)
        };
        let s = &mut st.panes[idx].scr;
        let mut dirty = s.all_dirty != 0 || s.view > 0;
        if !dirty {
            dirty = s.dirty.iter().any(|&d| d != 0);
        }
        if util::muxdbg_on() {
            util::muxdbg(&format!(
                "repaint? pane uid={uid} at {x},{y} {w}x{h} all={} dirty={}\n",
                s.all_dirty, dirty as i32
            ));
        }
        if dirty {
            s.repaint(x, y, w, h);
            painted = true;
        }
    }
    if layout_dirty || st.seps_dirty {
        draw_separators(st);
        painted = true;
        st.seps_dirty = false;
    }
    util::tty_buf_flush(); // one write() per pump, before the park
    mouse_mode_apply(st);

    // Park the real cursor on the focused pane's cursor — only when
    // something changed (repeating made terminals flicker).
    let want_vis = st
        .focus_pane()
        .map_or(false, |f| f.scr.view == 0 && f.scr.cur_vis != 0);
    let (mut r, mut c) = (0, 0);
    if want_vis {
        if let Some(f) = st.focus_pane() {
            r = f.scr.cy + 1 + f.y;
            c = f.scr.cx + 1 + f.x;
        }
    }
    if st.parked_row != r
        || st.parked_col != c
        || st.parked_vis != want_vis as i32
        || painted
        || st.park_force
    {
        let cb = if want_vis {
            format!("\x1b[{r};{c}H\x1b[?25h")
        } else {
            "\x1b[?25l".to_string()
        };
        util::mux_term_write(cb.as_bytes());
        util::muxdbg(&format!(
            "park: scr=({},{}) pane@({},{}) -> ({},{}) vis={} painted={}\n",
            st.focus_pane().map_or(-1, |f| f.scr.cx),
            st.focus_pane().map_or(-1, |f| f.scr.cy),
            st.focus_pane().map_or(-1, |f| f.x),
            st.focus_pane().map_or(-1, |f| f.y),
            r,
            c,
            want_vis as i32,
            painted as i32
        ));
        st.parked_row = r;
        st.parked_col = c;
        st.parked_vis = want_vis as i32;
        st.park_force = false;
    }
}

/// Port of `mux_pump()`: pane ptys → repaint → park.
pub fn pump() {
    with(pump_impl);
}

// ── teardown / leaving ─────────────────────────────────────────────────

/// Port of `mux_shutdown()` (atexit path): restore tty, kill panes.
pub fn shutdown() {
    with(|st| {
        if !st.active {
            return;
        }
        let panes = std::mem::take(&mut st.panes);
        for p in panes {
            pane_free(p);
        }
        st.root = None; // ≙ free_tree(root)
        st.focus = None;

        util::mux_term_write(b"\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l");
        util::mux_term_write(b"\x1b[?25h");
        st.mouse_sent = -1;
        st.active = false;
        st.mouse_fwd_press = false;
        st.click_pane = None;
        st.parked_row = -1;
        st.parked_col = -1;
        st.parked_vis = -1;
    });
}

/// Port of `mux_leave()`: shut down, hand the terminal a fresh plain
/// shell. Never returns.
pub fn leave() {
    if !active() {
        return;
    }
    shutdown();
    util::mux_term_write(b"\x1b[H\x1b[2J"); // panes gone: clean slate
    term::exit_raw();
    unsafe { libc::unsetenv(cstr("THESH_MUX").as_ptr()) };
    let p = self_path();
    util::muxdbg(&format!("leave: exec a fresh plain shell ({p})\n"));
    if !p.is_empty() {
        let cp = cstr(p);
        unsafe {
            libc::execv(cp.as_ptr(), [cp.as_ptr(), std::ptr::null()].as_ptr());
        }
    }
    unsafe { libc::_exit(127) }; // no exe: just end
}

// ── the router ─────────────────────────────────────────────────────────

/// Port of `mux_router_loop()`: never returns — the last pane's death
/// execs a fresh shell.
fn router_loop() -> ! {
    util::muxdbg(&format!(
        "router: loop start, {} pane(s)\n",
        with_ref(|st| st.panes.len())
    ));
    loop {
        crate::run::config_check(); // rc auto-reload
        let np = with_ref(|st| st.panes.len() as i32);
        if np <= 0 {
            leave(); // never returns
        }
        pump(); // ptys -> repaint -> park

        let mut cp = 0u32;
        let mut mods = 0;
        let key = editor::read_key(&mut cp, &mut mods);
        if key == K_EOF {
            // terminal gone
            shutdown();
            std::process::exit(util::shell_status());
        }

        if key == K_NONE && cp == 0 {
            // mouse event or DSR reply
            let raw = editor::raw();
            let is_dsr =
                raw.len() >= 3 && raw[0] == 0x1b && raw[1] == b'[' && raw[raw.len() - 1] == b'R';
            if is_dsr {
                continue; // nothing here probes the tty
            }
            if editor::mouse_forward() {
                if let Some(seq) = mouse_seq() {
                    forward_mouse(&seq);
                }
            }
            continue;
        }

        // mux VERB binds act on the tree; everything else goes to the
        // focused pane (tmux forward_key)
        if let Some(bd) = editor::bind_lookup(key, cp, mods) {
            if bd.kind == BindKind::Verb {
                util::muxdbg(&format!(
                    "router: key={key} cp={cp} mods={mods:x} VERB '{}'\n",
                    bd.cmd
                ));
                exec::exec_line(&bd.cmd);
                continue;
            }
            let raw = editor::raw();
            util::muxdbg(&format!(
                "router: key={key} cp={cp} mods={mods:x} bound->pane ({} bytes)\n",
                raw.len()
            ));
            if !raw.is_empty() {
                forward(&raw);
            }
            continue;
        }

        let nd = editor::mux_nav(key, mods); // Ctrl+arrows: move focus
        if nd >= 0 {
            focus_dir(nd);
            continue;
        }
        if key == K_PGUP {
            scroll(20);
            continue;
        }
        if key == K_PGDN {
            scroll(-20);
            continue;
        }

        scroll(0); // any key returns to live
        let raw = editor::raw();
        util::muxdbg(&format!(
            "router: key={key} cp={cp} mods={mods:x} ->pane ({} bytes)\n",
            raw.len()
        ));
        if !raw.is_empty() {
            forward(&raw);
        }
    }
}

// ── `panes` listing ────────────────────────────────────────────────────

/// Port of `mux_list()`.
pub fn list() {
    if !active() {
        eprintln!("{NAME}: mux: not multiplexed");
        return;
    }
    with_ref(|st| {
        for (i, p) in st.panes.iter().enumerate() {
            util::out_fmt(format_args!(
                "pane {}: {}x{} at {},{}{}\n",
                i + 1,
                p.w,
                p.h,
                p.x,
                p.y,
                if Some(p.uid) == st.focus { " [focused]" } else { "" }
            ));
        }
    });
}
