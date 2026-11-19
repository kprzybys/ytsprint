//! Private files and directories (0700 / 0600), atomic writes, the state log.
//! The cache holds account session data and code that ytsprint-jsd runs, so nothing here is world-readable.

use anyhow::{Context, Result, bail};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_MAX_BYTES: u64 = 1 << 20;

fn home() -> Result<String> {
    std::env::var("HOME").context("HOME is not set")
}

/// Creates `dir` (and parents) and makes sure the final directory is 0700.
pub fn private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let perms = std::fs::metadata(dir)?.permissions();
    if perms.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 {}", dir.display()))?;
    }
    Ok(())
}

/// Writes `data` to a temporary file next to `path` (0600) and renames it into place,
/// so readers never see a half-written file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_data())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// ~/.cache/ytsprint (0700).
pub fn cache_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(home()?).join(".cache"),
    };
    let dir = base.join("ytsprint");
    private_dir(&dir)?;
    Ok(dir)
}

/// $XDG_RUNTIME_DIR, required: the socket and the service script live there (a per-user 0700 directory).
/// No fallback to /tmp, where another local user could plant a fake socket.
pub fn runtime_dir() -> Result<PathBuf> {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => Ok(PathBuf::from(d)),
        _ => bail!("XDG_RUNTIME_DIR is not set (it is needed for the ytsprint-jsd socket)"),
    }
}

fn state_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(home()?).join(".local/state"),
    };
    let dir = base.join("ytsprint");
    private_dir(&dir)?;
    Ok(dir)
}

/// One line in ~/.local/state/ytsprint/log, written with a single write (concurrent processes do not
/// interleave). Rotates to log.1 above 1 MiB. Logging must not break playback: a failure goes to stderr.
pub fn log(line: &str) {
    if let Err(e) = try_log(line) {
        eprintln!("ytsprint: cannot write the log: {e:#}");
    }
}

fn try_log(line: &str) -> Result<()> {
    let path = state_dir()?.join("log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        std::fs::rename(&path, path.with_extension("1"))?;
    }
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut f = OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
    f.write_all(format!("{ts} {line}\n").as_bytes())?;
    Ok(())
}
