//! Cross-process advisory write lock.
//!
//! Both Claude Code and Claude Desktop (and Codex, and any other MCP client)
//! spawn their own `mcp-librarian.exe` over stdio. They share the same data
//! files on disk. Without coordination, two concurrent write-class tool calls
//! can corrupt state — most importantly the load-modify-save cycle around
//! `index.json`, the read-check-write cycle in note dedup, and the
//! copy-then-write sequence in manifest writes.
//!
//! This module provides a single helper, `with_write_lock`, that grants an
//! exclusive file lock for the duration of a closure. Lock granularity is
//! intentionally global (one lock for all write paths): held times are sub-
//! millisecond in practice, and the simplicity beats per-server granularity.
//!
//! Reads do NOT take the lock. Atomic-rename writes mean readers always see
//! either fully-old or fully-new content.

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use std::fs::OpenOptions;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::config::Paths;

const LOCK_FILE_NAME: &str = ".librarian.lock";

/// Total time we'll wait for the lock before giving up. A normal write holds
/// the lock for sub-millisecond; 10s is a generous bound that should only be
/// reached if something is genuinely wedged.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for try-lock. 25ms is short enough to feel instant on the
/// happy path and infrequent enough that contention doesn't spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Run `f` while holding an exclusive cross-process file lock on the
/// librarian's lock sentinel. Use this around every write-class operation:
/// note appends (including the dedup check), manifest writes, manifest
/// restores, and index saves. Read-only paths must not take the lock.
pub fn with_write_lock<F, R>(paths: &Paths, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("creating {}", paths.config_dir.display()))?;
    let lock_path = paths.config_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => break,
            Ok(false) | Err(_) if Instant::now() < deadline => {
                sleep(POLL_INTERVAL);
            }
            Ok(false) => {
                anyhow::bail!(
                    "Error: could not acquire librarian write lock at `{}` within {:?}. \
                     Action: another librarian process is holding it. If you're sure no \
                     concurrent write is in progress, delete the lock file and retry.",
                    lock_path.display(),
                    LOCK_TIMEOUT,
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "Error: lock acquisition failed at `{}`: {e}. \
                     Action: ensure the parent directory is writable.",
                    lock_path.display(),
                );
            }
        }
    }

    // Run the closure. Lock is released when `file` drops at end of scope,
    // whether `f` returns Ok or Err. Explicit drop after for clarity.
    let result = f();
    let _ = FileExt::unlock(&file);
    drop(file);
    result
}

#[cfg(test)]
pub(crate) fn lock_path_for(paths: &Paths) -> std::path::PathBuf {
    paths.config_dir.join(LOCK_FILE_NAME)
}

/// Convenience: ensure a path's parent directory exists. Used by atomic-write
/// helpers in `playbook` / `index` so callers don't repeat the boilerplate.
#[allow(dead_code)]
pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}
