//! Finding external dependencies without hard-coded paths: yt-dlp (with the ejs solver) and deno.
//!
//! yt-dlp: $YTSPRINT_YTDLP, otherwise the first `yt-dlp` in PATH that ships ejs — the onedir build
//! (`_internal/yt_dlp_ejs` next to the binary) or a pip install (the `yt_dlp_ejs` module of the script's interpreter).
//! ejs: $YTSPRINT_EJS_DIR, otherwise from that yt-dlp. The location is cached (it is checked on every play).
//! Player JS variant: whatever that yt-dlp uses (asked once per yt-dlp version), because ejs is only known to work
//! with the variant yt-dlp picks. No built-in default: when yt-dlp cannot tell, it is an error.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SOLVER_SUBDIR: &str = "yt_dlp_ejs/yt/solver";
const PROBE_SOURCE: &str = include_str!("ytdlp_probe.py");
/// Starting yt-dlp takes ~0.6 s (no network); anything near this limit means it is stuck.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct YtDlp {
    pub bin: PathBuf,
    pub ejs_dir: PathBuf,
}

impl YtDlp {
    /// Path of the player JS under `/s/player/<id>/`, as this yt-dlp would download it. Cached until the yt-dlp
    /// file changes: an update swaps the symlink (onedir) or rewrites the script (pip).
    pub fn player_js(&self) -> Result<String> {
        let real = std::fs::canonicalize(&self.bin).with_context(|| format!("resolve {}", self.bin.display()))?;
        let m = std::fs::metadata(&real)?;
        let id = json!([real, m.size(), m.mtime(), m.mtime_nsec()]);
        let cache = crate::fsutil::cache_dir()?.join("ytdlp-player-js.json");
        if let Some(v) = std::fs::read_to_string(&cache)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            && v["ytdlp"] == id
            && let Some(p) = v["player_js"].as_str()
        {
            return Ok(p.to_string());
        }
        let path = probe_player_js(&self.bin)?;
        let v = json!({"ytdlp": id, "player_js": path});
        crate::fsutil::write_atomic(&cache, v.to_string().as_bytes())?;
        Ok(path)
    }
}

/// Asks yt-dlp itself, through a plugin that prints the setting: the onedir build has no .py files to read.
fn probe_player_js(bin: &Path) -> Result<String> {
    let plugins = crate::fsutil::cache_dir()?.join("ytdlp-plugin");
    let dir = plugins.join("ytsprint/yt_dlp_plugins/extractor");
    crate::fsutil::private_dir(&dir)?;
    crate::fsutil::write_atomic(&dir.join("ytsprint_probe.py"), PROBE_SOURCE.as_bytes())?;
    let mut child = Command::new(bin)
        .args(["--ignore-config", "--no-plugin-dirs", "--plugin-dirs"])
        .arg(&plugins)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", bin.display()))?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() > PROBE_TIMEOUT {
            child.kill()?;
            child.wait()?;
            bail!("{}: no answer about the player JS variant", bin.display());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut s) = child.stdout.take() {
        s.read_to_string(&mut out)?;
    }
    if let Some(mut s) = child.stderr.take() {
        s.read_to_string(&mut err)?;
    }
    parse_probe(&out).with_context(|| {
        format!(
            "{}: cannot read the player JS variant (incompatible yt-dlp?): {}",
            bin.display(),
            err.trim().lines().last().unwrap_or("no output")
        )
    })
}

fn parse_probe(out: &str) -> Option<String> {
    let json = out.lines().find_map(|l| l.strip_prefix("YTSPRINT "))?;
    let v: Value = serde_json::from_str(json).ok()?;
    let p = v["player_js"].as_str()?;
    // it becomes part of a URL
    let ok = p.ends_with(".js")
        && !p.starts_with('/')
        && !p.contains("..")
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b));
    ok.then(|| p.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output() {
        let line = |p: &str| format!("noise\nYTSPRINT {{\"player_js\": \"{p}\"}}\n");
        let tv = "tv-player-ias.vflset/tv-player-ias.js";
        assert_eq!(parse_probe(&line(tv)).as_deref(), Some(tv));
        for bad in ["", "../x.js", "/x.js", "x.js?a=1", "x.css"] {
            assert_eq!(parse_probe(&line(bad)), None, "{bad}");
        }
        assert_eq!(parse_probe("yt-dlp: error: You must provide at least one URL."), None);
    }
}
