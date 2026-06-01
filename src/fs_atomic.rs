//! Atomic file write — write-temp-then-rename so a crash mid-write
//! leaves the temp behind but never a truncated target file.
//!
//! Used by:
//!   - Session storage (JSON snapshots — `session/storage.rs`)
//!   - File-mutating tools — `write` / `edit` / `apply_patch`
//!
//! All three tool paths previously called `tokio::fs::write(path,
//! content)` directly, which opens the file with `O_TRUNC` and
//! writes in-place: if the process crashes between the truncation
//! and the final byte, the file is corrupted with no recovery.
//! The `session/storage.rs` save path already had the safe
//! pattern; this module extracts it so the tools share one
//! implementation.
//!
//! POSIX `rename(2)` is atomic on the same filesystem; the helper
//! always places the temp file in the SAME parent dir as the
//! target to preserve that invariant. Cross-fs writes would
//! degrade to copy+delete, which is NOT atomic — callers that
//! cross filesystem boundaries should be aware.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-call counter so two concurrent atomic writes to the same
/// target don't collide on the temp filename. PID + nanos alone
/// isn't enough — two threads firing in the same nanosecond would
/// pick the same name. Counter is the load-bearing piece.
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Build a hidden sibling temp path: `.<stem>.<pid>-<nanos>-<n>.tmp`.
/// Always in the target's parent dir so the eventual rename stays
/// on one filesystem. Falls back to `.` if the target has no
/// parent (degenerate path like just a filename).
fn next_temp(target: &Path) -> PathBuf {
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
    let stem = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "atomic".to_string());
    let nonce = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        NONCE.fetch_add(1, Ordering::Relaxed),
    );
    let name = format!(".{}.{}.tmp", stem, nonce);
    match parent {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// On Unix, capture the existing file's mode so the atomic rename
/// preserves permissions. Without this, replacing an executable
/// shell script via the atomic path would silently drop the +x bit
/// because the freshly-created temp file uses default perms
/// (0644 minus umask). Returns None if the target doesn't exist
/// yet (fresh write) or stat fails.
#[cfg(unix)]
fn existing_mode(target: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(target)
        .ok()
        .map(|m| m.permissions().mode())
}

#[cfg(not(unix))]
fn existing_mode(_target: &Path) -> Option<u32> {
    None
}

/// Synchronous atomic write. Returns `io::Result` so callers can
/// use the existing `From<io::Error>` conversions in their error
/// types (e.g. `ToolError`).
pub fn atomic_write_sync(target: &Path, content: &[u8]) -> io::Result<()> {
    // Existing direct callers (session / memory / skills state) keep the
    // owner-only (0600) hardening for new files.
    atomic_write_inner(target, content, /* private */ true)
}

/// dirge-i5pu: shared impl. `private` controls the new-file mode — `true`
/// forces owner-only 0600 (session/memory state); `false` lets new files
/// inherit the umask (≈0644), which is correct for agent-created PROJECT
/// files written by the `write`/`edit`/`apply_patch` tools (forcing 0600
/// on those was a regression — they became un-readable group/other).
fn atomic_write_inner(target: &Path, content: &[u8], private: bool) -> io::Result<()> {
    let prev_mode = existing_mode(target);
    let tmp = next_temp(target);
    let result: io::Result<()> = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        // SESS-3: session/memory files contain user prompts, file contents,
        // and command outputs — restrict to owner-only (0600). Only on new
        // private files; existing file mode is preserved below.
        #[cfg(unix)]
        if private && prev_mode.is_none() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(content)?;
        // Best-effort fsync — non-fatal on filesystems that
        // don't support it (e.g. some networked mounts).
        let _ = f.sync_all();
        // Restore the prior mode BEFORE renaming so the target
        // inode lands with the right perms in one swap.
        #[cfg(unix)]
        if let Some(mode) = prev_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        }
        // On non-unix, file modes don't apply: `prev_mode` and the
        // `private` (0600) request are both unused. Consume them so
        // `-D warnings` doesn't fail the Windows build.
        #[cfg(not(unix))]
        let _ = (prev_mode, private);
        std::fs::rename(&tmp, target)?;
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup; the temp file is harmless if it
        // lingers (it's hidden and a follow-up write will produce
        // a fresh nonce'd name).
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Async atomic write. Used by file-mutating tools (`write`,
/// `edit`, `apply_patch`) which run inside the tokio agent loop.
/// Delegates to `spawn_blocking` since the underlying file ops
/// are synchronous — same pattern `tokio::fs::write` uses
/// internally. We don't use `tokio::fs` here because we want the
/// create + fsync + chmod + rename sequence to happen in one
/// blocking task without yielding between steps.
///
/// dirge-i5pu: these are PROJECT files the user edits/reads outside
/// dirge, so new files inherit the umask (`private = false`) rather than
/// the owner-only 0600 used for session/memory state.
pub async fn atomic_write(target: &Path, content: &[u8]) -> io::Result<()> {
    let target = target.to_path_buf();
    let bytes = content.to_vec();
    tokio::task::spawn_blocking(move || {
        atomic_write_inner(&target, &bytes, /* private */ false)
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking join failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Per-test dir under the system temp root; cleaned up on
    /// Drop. dirge doesn't pull in `tempfile`, so this matches the
    /// shape used by `tests/edit_tests.rs`.
    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "dirge_atomic_{}_{}_{}",
                tag,
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_write_creates_new_file() {
        let dir = TestDir::new("new");
        let target = dir.path().join("new.txt");
        atomic_write_sync(&target, b"hello").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = TestDir::new("overwrite");
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, b"old").unwrap();
        atomic_write_sync(&target, b"new content").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
    }

    /// Temp file goes in the SAME directory as the target so
    /// rename stays on one filesystem (POSIX rename atomicity).
    /// Also hidden (`.`-prefix) so a crash doesn't litter the
    /// directory with visible junk.
    #[test]
    fn temp_is_hidden_sibling() {
        let dir = TestDir::new("sibling");
        let target = dir.path().join("foo.txt");
        let tmp = next_temp(&target);
        assert_eq!(tmp.parent().unwrap(), dir.path());
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".foo.txt."), "got {name}");
        assert!(name.ends_with(".tmp"));
    }

    /// 1000 consecutive `next_temp` calls all distinct — the
    /// counter is the load-bearing piece against same-nanosecond
    /// collisions.
    #[test]
    fn next_temp_is_unique() {
        let target = std::path::Path::new("/tmp/sample.txt");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let t = next_temp(target);
            assert!(seen.insert(t), "collision in 1000 calls");
        }
    }

    /// Existing perms are preserved across an atomic overwrite.
    /// Without `chmod`-on-tmp before rename, replacing an
    /// executable script would silently drop the +x bit.
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("mode");
        let target = dir.path().join("script.sh");
        std::fs::write(&target, b"#!/bin/sh\necho hi").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write_sync(&target, b"#!/bin/sh\necho new").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable bit dropped");
    }

    /// dirge-i5pu: private writes (session/memory state) force a new
    /// file to owner-only 0600.
    #[cfg(unix)]
    #[test]
    fn private_write_new_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("priv");
        let target = dir.path().join("session.json");
        atomic_write_sync(&target, b"secret").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "session/memory files must be owner-only");
    }

    /// dirge-i5pu: agent-authored PROJECT files (write/edit/apply_patch,
    /// via the async `atomic_write` → `private = false`) must NOT be
    /// forced to 0600 — they inherit the umask exactly like a plain
    /// `File::create`, so the user can read them outside dirge. Compared
    /// against a reference create to stay deterministic under any umask.
    #[cfg(unix)]
    #[test]
    fn project_write_matches_umask_like_plain_create() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("proj");
        let reference = dir.path().join("reference");
        std::fs::File::create(&reference).unwrap();
        let want = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777;

        let target = dir.path().join("src.rs");
        atomic_write_inner(&target, b"fn main() {}", false).unwrap();
        let got = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            got, want,
            "project file mode should follow the umask (like a plain create), not be forced to 0600"
        );
    }

    /// Crash simulation: write a temp manually but skip the
    /// rename. The target should still read the ORIGINAL
    /// content — atomicity guarantee.
    #[test]
    fn target_untouched_on_failed_rename() {
        let dir = TestDir::new("crash");
        let target = dir.path().join("durable.txt");
        std::fs::write(&target, b"original").unwrap();
        let tmp = next_temp(&target);
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"corrupt half-write").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }
}
