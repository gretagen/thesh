//! Port of `src/dict.c` — the $PATH command dictionary and completion
//! file matching.
//!
//! C keeps this in file-static globals; we mirror that with a process-wide
//! `Mutex<DictState>` (the shell is single-threaded — panics aside, no
//! contention). Callers that need to *borrow* the list use [`with_all`];
//! random access uses [`count`]/[`get`].

use std::path::Path;
use std::sync::Mutex;

struct DictState {
    /// sorted, deduplicated command names (C's `cmds`)
    cmds: Vec<String>,
    /// the PATH string `cmds` was built from (C's `path_cache`)
    path_cache: Option<String>,
}

static DICT: Mutex<DictState> = Mutex::new(DictState {
    cmds: Vec::new(),
    path_cache: None,
});

fn lock() -> std::sync::MutexGuard<'static, DictState> {
    DICT.lock().unwrap_or_else(|p| p.into_inner())
}

/// C checks X_OK with access(3) — keep the exact same permission test
/// (real-uid semantics, ACLs included) instead of std's metadata flags.
fn executable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Port of `add_cmd()`: linear dedupe, keep insertion order (sorting
/// happens once after the whole PATH is scanned).
fn add_cmd(d: &mut DictState, name: &str) {
    if d.cmds.iter().any(|c| c == name) {
        return;
    }
    d.cmds.push(name.to_string());
}

/// Port of `dict_refresh()`: rebuild only when the PATH string changed.
pub fn refresh(pathstr: Option<&str>) {
    let mut d = lock();
    if let (Some(cache), Some(path)) = (d.path_cache.as_deref(), pathstr) {
        if cache == path {
            return;
        }
    }
    force_refresh_locked(&mut d, pathstr);
}

/// Port of `dict_force_refresh()`: always rebuild.
pub fn force_refresh(pathstr: Option<&str>) {
    let mut d = lock();
    force_refresh_locked(&mut d, pathstr);
}

fn force_refresh_locked(d: &mut DictState, pathstr: Option<&str>) {
    d.path_cache = pathstr.map(|s| s.to_string());
    d.cmds.clear();

    let Some(pathstr) = pathstr else { return };
    if pathstr.is_empty() {
        return;
    }

    // C uses strtok_r(dup, ":") — which skips empty segments entirely, so
    // "::/bin" never scans "" (opendir("") would fail anyway; we just make
    // the skip explicit).
    for seg in pathstr.split(':') {
        if seg.is_empty() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(seg) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "." || name == ".." {
                continue;
            }
            if executable(&e.path()) {
                add_cmd(d, &name);
            }
        }
    }

    // C: insertion sort with strcmp > 0 — ascending byte order. `sort` is
    // stable and String Ord is byte-wise, so identical result (no dups to
    // disturb anyway).
    d.cmds.sort();
}

/// Port of `dict_count()`.
pub fn count() -> usize {
    lock().cmds.len()
}

/// Port of `dict_get(i)` — C has UB out of range; we panic instead.
pub fn get(i: usize) -> String {
    lock().cmds[i].clone()
}

/// Borrow the whole list for iteration (spell's suggestion scan etc.).
/// The C original returns raw pointers into globals; a lock guard can't
/// escape, so callers iterate inside this callback.
pub fn with_all<R>(f: impl FnOnce(&[String]) -> R) -> R {
    f(&lock().cmds)
}

/// Port of `dict_has()`: binary search over the sorted list.
pub fn has(name: &str) -> bool {
    let d = lock();
    d.cmds.binary_search_by(|c| c.as_str().cmp(name)).is_ok()
}

/// Port of `file_matches()`: entries in `dir` starting with `stem`,
/// sorted, as full paths.
///
/// Quirks preserved: dotfiles are hidden unless the stem starts with `.`,
/// an empty/NULL `dir` lists `"."` but returns bare names (no `./`
/// prefix), and `dir == "/"` yields `"/name"`.
///
/// Deviation: C truncates results at PATH_MAX (4096); we don't.
pub fn file_matches(dir: Option<&str>, stem: &str) -> Vec<String> {
    let dir_is_root = dir == Some("/");
    let scan = match dir {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => ".".to_string(),
    };

    let Ok(entries) = std::fs::read_dir(&scan) else {
        return Vec::new();
    };

    let sl = stem.len();
    let mut v: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let nm = e.file_name();
        let nm = nm.to_string_lossy();
        if nm == "." || nm == ".." {
            continue;
        }
        let first = nm.as_bytes().first().copied();
        if first == Some(b'.') && (sl == 0 || !stem.starts_with('.')) {
            continue;
        }
        if sl > 0 && !nm.starts_with(stem) {
            continue;
        }
        let path = if dir_is_root {
            format!("/{nm}")
        } else if let Some(d) = dir {
            if !d.is_empty() {
                format!("{d}/{nm}")
            } else {
                nm.to_string()
            }
        } else {
            nm.to_string()
        };
        v.push(path);
    }
    v.sort(); // C: qsort with strcmp — byte order, same as String Ord
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// The tests share process-global state — serialize via the shared
    /// crate lock (config/presets tests touch the same globals).
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::util::test_lock()
    }

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("thesh-dict-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        fn file(&self, name: &str, executable: bool) -> std::path::PathBuf {
            let p = self.dir.join(name);
            fs::write(&p, "#!/bin/sh\n").unwrap();
            let mut perm = fs::metadata(&p).unwrap().permissions();
            perm.set_mode(if executable { 0o755 } else { 0o644 });
            fs::set_permissions(&p, perm).unwrap();
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn refresh_scans_sorts_dedupes_and_skips() {
        let _g = test_lock();
        let d1 = Fixture::new("d1");
        let d2 = Fixture::new("d2");
        d1.file("beta", true);
        d1.file("alpha", true);
        d1.file(".hidden", true);
        d1.file("notexec", false);
        d2.file("beta", true); // duplicate name in second dir → one entry
        d2.file("gamma", true);

        let path = format!("{}:{}:{}:{}", d1.dir.display(), d2.dir.display(),
                           d1.dir.join("missing").display(), "");
        force_refresh(Some(&path));
        with_all(|cmds| {
            // C's dict scan skips only "."/".." — unlike file_matches,
            // executables named .hidden DO become commands (pinned quirk)
            assert_eq!(cmds, &[".hidden", "alpha", "beta", "gamma"]);
        });
        assert!(has("beta"));
        assert!(has(".hidden"));
        assert!(!has("notexec"));
        assert!(!has("missing"));
        assert_eq!(count(), 4);
        assert_eq!(get(1), "alpha");
    }

    #[test]
    fn empty_or_null_path_clears() {
        let _g = test_lock();
        force_refresh(Some(""));
        assert_eq!(count(), 0);
        force_refresh(None);
        assert_eq!(count(), 0);
        assert!(!has("anything"));
    }

    #[test]
    fn refresh_with_same_path_is_a_noop() {
        let _g = test_lock();
        let d1 = Fixture::new("noop");
        d1.file("onlycmd", true);
        let path = d1.dir.display().to_string();
        force_refresh(Some(&path));
        assert_eq!(count(), 1);
        // remove the file behind the dict's back; refresh(same PATH)
        // must NOT rescan (path_cache hit), like C
        fs::remove_file(d1.dir.join("onlycmd")).unwrap();
        refresh(Some(&path));
        assert_eq!(count(), 1);
        // force does rescan
        force_refresh(Some(&path));
        assert_eq!(count(), 0);
    }

    #[test]
    fn file_matches_prefix_and_dotfile_rules() {
        let fx = Fixture::new("fm");
        for n in ["apple", "apricot", "banana", ".hidden", ".hid2"] {
            fx.file(n, false);
        }
        let d = fx.dir.display().to_string();

        // nonempty dir → full paths (C: "%s/%s"), sorted by basename
        assert_eq!(
            file_matches(Some(&d), ""),
            vec![format!("{d}/apple"), format!("{d}/apricot"), format!("{d}/banana")]
        );
        assert_eq!(
            file_matches(Some(&d), "a"),
            vec![format!("{d}/apple"), format!("{d}/apricot")]
        );
        assert_eq!(file_matches(Some(&d), "app"), vec![format!("{d}/apple")]);
        assert_eq!(
            file_matches(Some(&d), "."),
            vec![format!("{d}/.hid2"), format!("{d}/.hidden")]
        );
        assert_eq!(file_matches(Some(&d), "ban"), vec![format!("{d}/banana")]);
        assert!(file_matches(Some(&d), "zzz").is_empty());
        // missing directory → empty
        assert!(file_matches(Some("/no/such/dir"), "").is_empty());
    }

    #[test]
    fn file_matches_path_construction() {
        let fx = Fixture::new("pc");
        fx.file("solo", false);
        let d = fx.dir.display().to_string();

        // dir given → dir/name
        assert_eq!(file_matches(Some(&d), "solo"), vec![format!("{d}/solo")]);
        // root dir special case → /name
        assert_eq!(file_matches(Some("/"), "solo-bin-does-not-exist"), Vec::<String>::new());
        // empty dir → bare name (no ./ prefix), scanned from cwd
        // (bare-name construction verified via the root/empty branches;
        // "."-scan against cwd is environment-dependent, asserted loosely)
        let bare = file_matches(None, "");
        assert!(bare.iter().all(|p| !p.contains('/')));
    }
}
