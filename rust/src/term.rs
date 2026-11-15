//! Port of `src/term.c` — raw-mode terminal handling.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static ORIG: Mutex<Option<libc::termios>> = Mutex::new(None);
static RAW_ON: AtomicBool = AtomicBool::new(false);

fn lock_orig() -> std::sync::MutexGuard<'static, Option<libc::termios>> {
    ORIG.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `term_enter_raw()`: strip ICANON/ECHO/ISIG/IEXTEN and IXON/ICRNL,
/// VMIN=1, VTIME=0. Idempotent — a second call is a no-op (like C).
///
/// Like the C original this fails (as C: returns −1) when stdin isn't a
/// tty, which is how the non-interactive paths stay in cooked mode.
pub fn enter_raw() -> io::Result<()> {
    if RAW_ON.load(Ordering::SeqCst) {
        crate::util::muxdbg(&format!(
            "term: enter-raw pid={} (already on)\n",
            unsafe { libc::getpid() }
        ));
        return Ok(());
    }

    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut t = orig;
    t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    t.c_iflag &= !(libc::IXON | libc::ICRNL);
    t.c_cc[libc::VMIN as usize] = 1;
    t.c_cc[libc::VTIME as usize] = 0;

    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &t) } < 0 {
        return Err(io::Error::last_os_error());
    }

    *lock_orig() = Some(orig);
    RAW_ON.store(true, Ordering::SeqCst);
    crate::util::muxdbg(&format!(
        "term: enter-raw pid={} fd={}\n",
        unsafe { libc::getpid() },
        libc::STDIN_FILENO
    ));
    Ok(())
}

/// Port of `term_exit_raw()`: restore the saved termios. No-op when raw
/// mode isn't active.
pub fn exit_raw() {
    if !RAW_ON.load(Ordering::SeqCst) {
        return;
    }
    crate::util::muxdbg(&format!(
        "term: EXIT-raw pid={} fd={}\n",
        unsafe { libc::getpid() },
        libc::STDIN_FILENO
    ));
    if let Some(orig) = *lock_orig() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &orig) };
    }
    RAW_ON.store(false, Ordering::SeqCst);
}

/// Port of `term_raw_active()`.
pub fn raw_active() -> bool {
    RAW_ON.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_mode_roundtrip() {
        let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if is_tty {
            assert!(!raw_active());
            enter_raw().expect("enter raw on tty");
            assert!(raw_active());
            enter_raw().expect("second enter is a no-op");
            exit_raw();
            assert!(!raw_active());
            exit_raw(); // second exit is a no-op
        } else {
            // off-tty: C's tcgetattr fails → we return Err, state unchanged
            assert!(enter_raw().is_err());
            assert!(!raw_active());
            exit_raw(); // no-op
        }
    }
}
