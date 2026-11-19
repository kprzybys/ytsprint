//! Finding external dependencies without hard-coded paths: yt-dlp (with the ejs solver) and deno.
//!
//! yt-dlp: $YTSPRINT_YTDLP, otherwise the first `yt-dlp` in PATH that ships ejs — the onedir build
//! (`_internal/yt_dlp_ejs` next to the binary) or a pip install (the `yt_dlp_ejs` module of the script's interpreter).
//! ejs: $YTSPRINT_EJS_DIR, otherwise from that yt-dlp. The location is cached (it is checked on every play).

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const SOLVER_SUBDIR: &str = "yt_dlp_ejs/yt/solver";

pub struct YtDlp {
    pub bin: PathBuf,
    pub ejs_dir: PathBuf,
}

/// All executable files named `name` in PATH, in order.
pub fn in_path(name: &str) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.join(name))
        .filter(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .collect()
}

fn has_solver(dir: &Path) -> bool {
    dir.join("lib.min.js").is_file() && dir.join("core.min.js").is_file()
}

/// ejs for a given yt-dlp binary, or None when that yt-dlp does not ship it (e.g. an old distro package).
fn ejs_for(bin: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(bin).ok()?;
    // onedir build: <dir>/yt-dlp + <dir>/_internal/yt_dlp_ejs/...
    let onedir = real.parent()?.join("_internal").join(SOLVER_SUBDIR);
    if has_solver(&onedir) {
        return Some(onedir);
    }
    // pip / pipx: a Python script; ask its interpreter (from the shebang) where yt_dlp_ejs is
    let head = std::fs::read(&real).ok()?;
    let first = head.split(|b| *b == b'\n').next()?;
    let shebang = std::str::from_utf8(first).ok()?.strip_prefix("#!")?.trim();
    let mut parts = shebang.split_whitespace();
    let mut interp = parts.next()?.to_string();
    if interp.ends_with("/env") {
        interp = parts.next()?.to_string();
    }
    if !interp.contains("python") {
        return None;
    }
    let out = Command::new(interp)
        .args([
            "-c",
            "import os, yt_dlp_ejs; print(os.path.dirname(yt_dlp_ejs.__file__))",
        ])
        .output()
        .ok()?;
    let pkg = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    let dir = pkg.join("yt").join("solver");
    (out.status.success() && has_solver(&dir)).then_some(dir)
}

pub fn yt_dlp() -> Result<YtDlp> {
    // candidates: $YTSPRINT_YTDLP alone, otherwise every yt-dlp in PATH in order
    let candidates = match std::env::var("YTSPRINT_YTDLP") {
        Ok(bin) => vec![PathBuf::from(bin)],
        Err(_) => in_path("yt-dlp"),
    };
    let env_ejs = std::env::var("YTSPRINT_EJS_DIR").ok().map(PathBuf::from);
    let cache_file = crate::fsutil::cache_dir()?.join("ytdlp-location.json");
    // cache: valid while the candidate list is unchanged, the chosen binary still points to the same file
    // (a yt-dlp update swaps the symlink to a new version) and the ejs directory exists
    if let Some(v) = std::fs::read_to_string(&cache_file)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        let same_candidates = serde_json::to_value(&candidates).ok().as_ref() == Some(&v["candidates"]);
        if let (true, Some(bin), Some(ejs), Some(real)) = (
            same_candidates,
            v["bin"].as_str(),
            v["ejs_dir"].as_str(),
            v["real"].as_str(),
        ) {
            let same_target = std::fs::canonicalize(bin).ok().as_deref() == Some(Path::new(real));
            if same_target && has_solver(Path::new(ejs)) {
                return Ok(YtDlp {
                    bin: bin.into(),
                    ejs_dir: env_ejs.unwrap_or_else(|| ejs.into()),
                });
            }
        }
    }
    for bin in &candidates {
        if let Some(ejs) = ejs_for(bin) {
            let real = std::fs::canonicalize(bin).with_context(|| format!("resolve {}", bin.display()))?;
            let v = json!({"candidates": candidates, "bin": bin, "real": real, "ejs_dir": ejs});
            crate::fsutil::write_atomic(&cache_file, v.to_string().as_bytes())?;
            return Ok(YtDlp {
                bin: bin.clone(),
                ejs_dir: env_ejs.unwrap_or(ejs),
            });
        }
    }
    if candidates.is_empty() {
        bail!("yt-dlp not found in PATH (set YTSPRINT_YTDLP)");
    }
    bail!(
        "no yt-dlp with the ejs solver ({}); install a current yt-dlp \
         (onedir build or `pip install \"yt-dlp[default]\"`) or set YTSPRINT_EJS_DIR",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// yt-dlp for requests ytsprint routes onward (live streams, other sites): the one with ejs when there is one,
/// otherwise the first in PATH — those requests do not need ytsprint's solver.
pub fn yt_dlp_for_routing() -> Result<PathBuf> {
    match yt_dlp() {
        Ok(y) => Ok(y.bin),
        Err(e) => {
            let first = std::env::var("YTSPRINT_YTDLP")
                .map(PathBuf::from)
                .ok()
                .or_else(|| in_path("yt-dlp").into_iter().next());
            first.with_context(|| format!("no yt-dlp to route to: {e:#}"))
        }
    }
}

/// deno: $YTSPRINT_DENO or the first one in PATH.
pub fn deno() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("YTSPRINT_DENO") {
        return Ok(PathBuf::from(d));
    }
    in_path("deno")
        .into_iter()
        .next()
        .context("deno not found in PATH (set YTSPRINT_DENO)")
}
