//! Port of `src/history.c` — command history with file persistence.
//!
//! C keeps this in the global `History H`; mirrored here with a
//! process-wide mutex. `Cfg.history_enabled` lives *in this struct* as
//! [`enabled`] (C reads the global config — M2's config module must call
//! [`set_enabled`] *before* [`clear`] when `history = no`, matching
//! config.c: `Cfg.history_enabled = 0; hist_clear();`).

use std::sync::Mutex;

pub const HIST_NAME: &str = ".thesh_history";
pub const HIST_MAX: i32 = 500;

struct History {
    items: Vec<String>,
    max: i32,
    path: String,
    dirty: bool,
    loaded: bool,
    /// Cfg.history_enabled (config.c default: 1)
    enabled: bool,
}

static HISTORY: Mutex<History> = Mutex::new(History {
    items: Vec::new(),
    max: HIST_MAX,
    path: String::new(),
    dirty: false,
    loaded: false,
    enabled: true,
});

fn lock() -> std::sync::MutexGuard<'static, History> {
    HISTORY.lock().unwrap_or_else(|p| p.into_inner())
}

/// Port of `hist_setup()`: resolve the history file path from
/// `$THESH_HISTFILE`, else `$HOME/.thesh_history`, else
/// `/tmp/.thesh_history`, and reset the in-memory state.
///
/// Like C's `memset(&H, …)` this does **not** touch [`enabled`]
/// (the C memset only covers `H`, not `Cfg`).
pub fn setup() {
    let path = std::env::var_os("THESH_HISTFILE")
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|p| !p.is_empty())
                .map(|p| format!("{}/{}", p.to_string_lossy(), HIST_NAME))
        })
        .unwrap_or_else(|| "/tmp/.thesh_history".to_string());
    setup_with(path);
}

/// Internal counterpart of [`setup`] with an explicit path (test hook —
/// production code uses `setup()`).
fn setup_with(path: String) {
    let mut h = lock();
    h.items.clear();
    h.max = HIST_MAX;
    h.path = path;
    h.dirty = false;
    h.loaded = false;
}

/// Port of `hist_load()`. Reads whole lines (C uses `getline` since the
/// M0 fix — entries may exceed 4096 bytes), trims trailing `\r`/`\n`
/// like the C loop, skips empty lines, stops entirely when `max <= 0`,
/// and trims the oldest entries down to `max` while loading.
///
/// Once-only via the `loaded` flag — even when disabled or unreadable,
/// exactly like C (a later `set_enabled(true)` does not re-load).
pub fn load() {
    let mut h = lock();
    if h.loaded {
        return;
    }
    h.loaded = true;
    if !h.enabled {
        return;
    }
    let Ok(data) = std::fs::read(&h.path) else { return };
    // C stores arbitrary bytes; history is written by us from editor input
    // (always UTF-8), so lossy conversion only kicks in for corrupt files.
    let data = String::from_utf8_lossy(&data);
    for seg in data.split('\n') {
        // C: getline splits on \n, then trims any trailing \r/\n run
        let line = seg.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if h.max <= 0 {
            break;
        }
        if h.items.len() as i32 >= h.max {
            h.items.remove(0);
        }
        h.items.push(line.to_string());
    }
}

/// Port of `hist_set_limit()`: clamp to 0..=1_000_000, then drop oldest
/// entries beyond the new limit (0 clears everything).
pub fn set_limit(n: i32) {
    let mut h = lock();
    let n = n.clamp(0, 1_000_000); // C: if (n<0) n=0; if (n>1000000) n=1000000;
    h.max = n;
    while (h.items.len() as i32) > h.max && !h.items.is_empty() {
        h.items.remove(0);
    }
}

/// Port of `hist_add()`: ignore empty lines, disabled state and
/// `max <= 0`; collapse consecutive duplicates (still marking the history
/// dirty); evict the oldest entry at the limit.
pub fn add(line: &str) {
    let mut h = lock();
    if line.is_empty() || !h.enabled || h.max <= 0 {
        return;
    }
    if h.items.last().map_or(false, |l| l == line) {
        h.dirty = true;
        return;
    }
    if (h.items.len() as i32) >= h.max {
        h.items.remove(0);
    }
    h.items.push(line.to_string());
    h.dirty = true;
}

/// Port of `hist_clear()`: drop everything in memory and persist that
/// (save is a no-op when already disabled, like C).
pub fn clear() {
    {
        let mut h = lock();
        h.items.clear();
        h.dirty = true;
    }
    save();
}

/// Port of `hist_save()`: only when dirty and enabled; writes the last
/// `max` entries, one per line. A failed write keeps `dirty` (like C).
pub fn save() {
    let mut h = lock();
    if !h.dirty || !h.enabled {
        return;
    }
    let start = if (h.items.len() as i32) > h.max {
        h.items.len() - h.max as usize
    } else {
        0
    };
    let mut out = String::new();
    for item in &h.items[start..] {
        out.push_str(item);
        out.push('\n');
    }
    if std::fs::write(&h.path, out).is_ok() {
        h.dirty = false;
    }
}

/// Port of `hist_find()`: **prefix** match walking backward/forward from
/// `start`. Empty needle matches immediately (C: `strncmp(…, 0) == 0`).
///
/// Deviations from C's undefined behaviour: `start` is clamped into
/// range (C would read `items[-1]` / past the end) and a negative start
/// yields −1.
pub fn find(needle: &str, start: i32, backward: bool) -> i32 {
    let h = lock();
    let n = h.items.len() as i32;
    if backward {
        let start = start.min(n - 1);
        for i in (0..=start).rev() {
            if h.items[i as usize].starts_with(needle) {
                return i;
            }
        }
    } else {
        let start = start.max(0);
        for i in start..n {
            if h.items[i as usize].starts_with(needle) {
                return i;
            }
        }
    }
    -1
}

/// Port of `hist_find_substr()`: substring match; empty needle → −1.
/// Same start-clamping deviation as [`find`].
pub fn find_substr(sub: &str, start: i32, backward: bool) -> i32 {
    if sub.is_empty() {
        return -1;
    }
    let h = lock();
    let n = h.items.len() as i32;
    if backward {
        let start = start.min(n - 1);
        for i in (0..=start).rev() {
            if h.items[i as usize].contains(sub) {
                return i;
            }
        }
    } else {
        let start = start.max(0);
        for i in start..n {
            if h.items[i as usize].contains(sub) {
                return i;
            }
        }
    }
    -1
}

// ── accessors used by later milestones and the tests ───────────────────

pub fn count() -> usize {
    lock().items.len()
}

pub fn with_all<R>(f: impl FnOnce(&[String]) -> R) -> R {
    f(&lock().items)
}

pub fn set_enabled(v: bool) {
    lock().enabled = v;
}

pub fn enabled() -> bool {
    lock().enabled
}

pub fn max() -> i32 {
    lock().max
}

pub fn path() -> String {
    lock().path.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The tests share process-global state (HISTORY and anything the
    /// other modules touch) — serialize via the shared crate lock.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::util::test_lock()
    }

    fn tmpfile(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("thesh-hist-{tag}-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    fn items() -> Vec<String> {
        with_all(|v| v.to_vec())
    }

    #[test]
    fn roundtrip_save_load() {
        let _g = test_lock();
        let p = tmpfile("round");
        setup_with(p.clone());
        add("echo one");
        add("echo two");
        save();
        // fresh session over the same file
        setup_with(p.clone());
        load();
        assert_eq!(items(), ["echo one", "echo two"]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        let _g = test_lock();
        setup_with(tmpfile("dup"));
        add("same");
        add("same");
        add("other");
        add("same"); // non-consecutive → re-added
        assert_eq!(items(), ["same", "other", "same"]);
        let _ = fs::remove_file(path());
    }

    #[test]
    fn add_evicts_oldest_at_limit() {
        let _g = test_lock();
        setup_with(tmpfile("evict"));
        set_limit(2);
        add("a");
        add("b");
        add("c");
        assert_eq!(items(), ["b", "c"]);
        // max <= 0 disables adding entirely
        set_limit(0);
        assert!(items().is_empty());
        add("d");
        assert!(items().is_empty());
    }

    #[test]
    fn set_limit_clamps() {
        let _g = test_lock();
        setup_with(tmpfile("clamp"));
        set_limit(-5);
        assert_eq!(max(), 0);
        set_limit(999_999_999);
        assert_eq!(max(), 1_000_000); // C caps at 1e6
    }

    #[test]
    fn load_trims_to_limit_and_skips_empty_lines() {
        let _g = test_lock();
        let p = tmpfile("load");
        // CRLF, blank lines, spaces preserved (only \r\n are trimmed)
        fs::write(&p, "one\r\ntwo\n\n   spaced   \nfour\nfive\n").unwrap();
        setup_with(p.clone());
        set_limit(2);
        load();
        assert_eq!(items(), ["four", "five"]); // oldest trimmed to max
        // max=0 stops the read at the first non-empty line
        fs::write(&p, "a\nb\n").unwrap();
        setup_with(p.clone());
        set_limit(0);
        load();
        assert!(items().is_empty());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_is_once_even_across_enable_toggle() {
        let _g = test_lock();
        let p = tmpfile("once");
        fs::write(&p, "early\n").unwrap();
        setup_with(p.clone());
        set_enabled(false);
        load(); // disabled: marks loaded, reads nothing
        set_enabled(true);
        load(); // loaded flag set → still nothing (C quirk, pinned)
        assert!(items().is_empty());
        set_enabled(true);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn disabled_blocks_add_and_save() {
        let _g = test_lock();
        let p = tmpfile("dis");
        fs::write(&p, "existing\n").unwrap();
        setup_with(p.clone());
        set_enabled(false);
        load();
        add("nope");
        save();
        assert_eq!(fs::read_to_string(&p).unwrap(), "existing\n"); // untouched
        assert!(items().is_empty());
        set_enabled(true);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn clear_drops_memory_and_persists_empty_file() {
        let _g = test_lock();
        let p = tmpfile("clear");
        setup_with(p.clone());
        add("gone");
        add("gone too");
        clear();
        assert!(items().is_empty());
        assert_eq!(fs::read_to_string(&p).unwrap(), "");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn find_prefix_forward_and_backward() {
        let _g = test_lock();
        setup_with(tmpfile("find"));
        add("echo hi");
        add("exit");
        add("cat");
        // backward from the end
        assert_eq!(find("ec", 2, true), 0); // cat✗ exit✗ echo✓
        assert_eq!(find("e", 2, true), 1); // exit (nearest backward)
        // forward
        assert_eq!(find("ex", 0, false), 1);
        assert_eq!(find("zz", 0, false), -1);
        // empty needle: matches the start index immediately (C strncmp 0)
        assert_eq!(find("", 2, true), 2);
        // clamped out-of-range start (C would be UB)
        assert_eq!(find("c", 99, true), 2);
        assert_eq!(find("e", -5, false), 0);
    }

    #[test]
    fn find_substr_scans_anywhere() {
        let _g = test_lock();
        setup_with(tmpfile("sub"));
        add("make release");
        add("ls -la");
        assert_eq!(find_substr("release", 1, true), 0);
        assert_eq!(find_substr("-l", 0, false), 1);
        assert_eq!(find_substr("", 1, true), -1); // empty → -1
        assert_eq!(find_substr("nope", 1, true), -1);
    }

    #[test]
    fn empty_history_search_is_safe() {
        let _g = test_lock();
        setup_with(tmpfile("empty"));
        assert_eq!(find("x", -1, true), -1);
        assert_eq!(find_substr("x", -1, false), -1);
    }
}
