//! Finding external dependencies without hard-coded paths: yt-dlp (with the ejs solver) and deno.
//!
//! yt-dlp: $YTSPRINT_YTDLP, otherwise the first `yt-dlp` in PATH that ships ejs — the onedir build
//! (`_internal/yt_dlp_ejs` next to the binary) or a pip install (the `yt_dlp_ejs` module of the script's interpreter).
//! ejs: $YTSPRINT_EJS_DIR, otherwise from that yt-dlp. The location is cached (it is checked on every play).
//! Player JS variant and InnerTube clients: whatever that yt-dlp uses (asked once per yt-dlp version), so that its
//! fixes for YouTube changes arrive with its update. No built-in defaults: when yt-dlp cannot tell, it is an error.

use crate::fsutil::env_path;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SOLVER_SUBDIR: &str = "yt_dlp_ejs/yt/solver";
const PROBE_SOURCE: &str = include_str!("ytdlp_probe.py");
/// Starting yt-dlp takes ~0.6 s (no network); anything near this limit means it is stuck.
const RUN_TIMEOUT: Duration = Duration::from_secs(15);

pub struct YtDlp {
    pub bin: PathBuf,
    pub ejs_dir: PathBuf,
}

/// What ytsprint takes from the installed yt-dlp instead of keeping its own copy.
#[derive(Clone, Debug)]
pub struct Settings {
    /// path of the player JS under `/s/player/<id>/`
    pub player_js: String,
    pub web: InnertubeClient,
    pub web_creator: InnertubeClient,
    pub tv_samsung: InnertubeClient,
}

#[derive(Clone, Debug)]
pub struct InnertubeClient {
    /// X-Youtube-Client-Name
    pub id: String,
    pub version: String,
    pub user_agent: Option<String>,
    /// `context.client` of a request
    pub client: Value,
}

impl Settings {
    fn parse(v: &Value) -> Option<Settings> {
        let p = v["player_js"].as_str()?;
        // it becomes part of a URL
        let ok = p.ends_with(".js")
            && !p.starts_with('/')
            && !p.contains("..")
            && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b));
        if !ok {
            return None;
        }
        let client = |name: &str| InnertubeClient::parse(&v["clients"][name]);
        Some(Settings {
            player_js: p.to_string(),
            web: client("web")?,
            web_creator: client("web_creator")?,
            tv_samsung: client("tv_samsung")?,
        })
    }
}

impl InnertubeClient {
    fn parse(v: &Value) -> Option<InnertubeClient> {
        let mut client = v["client"].clone();
        let version = client["clientVersion"].as_str()?.to_string();
        let user_agent = client["userAgent"].as_str().map(str::to_string);
        client["clientName"].as_str()?;
        // as yt-dlp does in _extract_context
        client["hl"] = json!("en");
        client["timeZone"] = json!("UTC");
        client["utcOffsetMinutes"] = json!(0);
        Some(InnertubeClient {
            id: v["id"].as_u64()?.to_string(),
            version,
            user_agent,
            client,
        })
    }
}

impl YtDlp {
    /// The settings of this yt-dlp. Cached until the yt-dlp file changes: an update swaps the symlink (onedir)
    /// or rewrites the script (pip).
    pub fn settings(&self) -> Result<Settings> {
        let real = std::fs::canonicalize(&self.bin).with_context(|| format!("resolve {}", self.bin.display()))?;
        let m = std::fs::metadata(&real)?;
        // the probe is part of the identity: a new ytsprint may ask for more
        let id = json!([real, m.size(), m.mtime(), m.mtime_nsec(), PROBE_SOURCE]);
        let cache = crate::fsutil::cache_dir()?.join("ytdlp-settings.json");
        if let Some(v) = std::fs::read_to_string(&cache)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            && v["ytdlp"] == id
            && let Some(s) = Settings::parse(&v["settings"])
        {
            return Ok(s);
        }
        let (raw, settings) = probe(&self.bin)?;
        let v = json!({"ytdlp": id, "settings": raw});
        crate::fsutil::write_atomic(&cache, v.to_string().as_bytes())?;
        Ok(settings)
    }
}

/// Runs a program to the end with RUN_TIMEOUT. Both pipes are read while it runs (a full pipe would block it).
fn run(cmd: &mut Command) -> Result<(ExitStatus, String, String)> {
    let what = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {what}"))?;
    let deadline = Instant::now() + RUN_TIMEOUT;
    fn reader(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![];
            let _ = pipe.read_to_end(&mut buf); // what was read before an error is still useful
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        rx
    }
    let out = reader(child.stdout.take().context("stdout")?);
    let err = reader(child.stderr.take().context("stderr")?);
    // the pipes close when the program ends; a timeout is measured for all three together
    let left = || deadline.saturating_duration_since(Instant::now());
    let result = out
        .recv_timeout(left())
        .and_then(|o| Ok((o, err.recv_timeout(left())?)));
    let Ok((out, err)) = result else {
        child.kill()?;
        child.wait()?;
        bail!("{what}: no answer in {} s", RUN_TIMEOUT.as_secs());
    };
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, out, err));
        }
        if left().is_zero() {
            child.kill()?;
            child.wait()?;
            bail!("{what}: did not exit in {} s", RUN_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Asks yt-dlp itself, through a plugin that prints the settings: the onedir build has no .py files to read.
fn probe(bin: &Path) -> Result<(Value, Settings)> {
    let plugins = crate::fsutil::cache_dir()?.join("ytdlp-plugin");
    let dir = plugins.join("ytsprint/yt_dlp_plugins/extractor");
    crate::fsutil::private_dir(&dir)?;
    crate::fsutil::write_atomic(&dir.join("ytsprint_probe.py"), PROBE_SOURCE.as_bytes())?;
    let (_, out, err) = run(Command::new(bin)
        .args(["--ignore-config", "--no-plugin-dirs", "--plugin-dirs"])
        .arg(&plugins))?;
    parse_probe(&out).with_context(|| {
        format!(
            "{}: cannot read its settings (incompatible yt-dlp?): {}",
            bin.display(),
            err.trim().lines().last().unwrap_or("no output")
        )
    })
}

fn parse_probe(out: &str) -> Option<(Value, Settings)> {
    let json = out.lines().find_map(|l| l.strip_prefix("YTSPRINT "))?;
    let v: Value = serde_json::from_str(json).ok()?;
    let settings = Settings::parse(&v)?;
    Some((v, settings))
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
/// An interpreter that hangs is an error.
fn ejs_for(bin: &Path) -> Result<Option<PathBuf>> {
    let Some((interp, onedir)) = ejs_candidates(bin) else {
        return Ok(None);
    };
    if has_solver(&onedir) {
        return Ok(Some(onedir));
    }
    let Some(interp) = interp else { return Ok(None) };
    // pip / pipx: a Python script; ask its interpreter (from the shebang) where yt_dlp_ejs is
    let (status, out, _) = run(Command::new(interp).args([
        "-c",
        "import os, yt_dlp_ejs; print(os.path.dirname(yt_dlp_ejs.__file__))",
    ]))?;
    let dir = PathBuf::from(out.trim()).join("yt").join("solver");
    Ok((status.success() && has_solver(&dir)).then_some(dir))
}

/// The Python interpreter of a script (None for other files) and where the onedir build keeps ejs.
fn ejs_candidates(bin: &Path) -> Option<(Option<String>, PathBuf)> {
    let real = std::fs::canonicalize(bin).ok()?;
    // onedir build: <dir>/yt-dlp + <dir>/_internal/yt_dlp_ejs/...
    let onedir = real.parent()?.join("_internal").join(SOLVER_SUBDIR);
    Some((python_of(&real), onedir))
}

fn python_of(real: &Path) -> Option<String> {
    let head = std::fs::read(real).ok()?;
    let first = head.split(|b| *b == b'\n').next()?;
    let shebang = std::str::from_utf8(first).ok()?.strip_prefix("#!")?.trim();
    let mut parts = shebang.split_whitespace();
    let mut interp = parts.next()?.to_string();
    if interp.ends_with("/env") {
        interp = parts.next()?.to_string();
    }
    interp.contains("python").then_some(interp)
}

pub fn yt_dlp() -> Result<YtDlp> {
    // candidates: $YTSPRINT_YTDLP alone, otherwise every yt-dlp in PATH in order
    let candidates = match env_path("YTSPRINT_YTDLP") {
        Some(bin) => vec![bin],
        None => in_path("yt-dlp"),
    };
    let env_ejs = env_path("YTSPRINT_EJS_DIR");
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
        if let Some(ejs) = ejs_for(bin)? {
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
         (onedir build or `pip install \"yt-dlp[default]\"`)",
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
            let first = env_path("YTSPRINT_YTDLP").or_else(|| in_path("yt-dlp").into_iter().next());
            first.with_context(|| format!("no yt-dlp to route to: {e:#}"))
        }
    }
}

/// deno: $YTSPRINT_DENO or the first one in PATH.
pub fn deno() -> Result<PathBuf> {
    if let Some(d) = env_path("YTSPRINT_DENO") {
        return Ok(d);
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
        let client = json!({"id": 7, "client": {"clientName": "TVHTML5", "clientVersion": "5.2", "userAgent": "TV", "hl": "en"}});
        let line = |path: &str, clients: Value| {
            format!("noise\nYTSPRINT {}\n", json!({"player_js": path, "clients": clients}))
        };
        let all = json!({"web": client, "web_creator": client, "tv_samsung": client});
        let tv = "tv-player-ias.vflset/tv-player-ias.js";
        let (_, s) = parse_probe(&line(tv, all.clone())).unwrap();
        assert_eq!(s.player_js, tv);
        let c = &s.tv_samsung;
        assert_eq!(
            (c.id.as_str(), c.version.as_str(), c.user_agent.as_deref()),
            ("7", "5.2", Some("TV"))
        );
        assert_eq!(c.client["timeZone"], "UTC");
        for bad in ["", "../x.js", "/x.js", "x.js?a=1", "x.css"] {
            assert!(parse_probe(&line(bad, all.clone())).is_none(), "{bad}");
        }
        assert!(parse_probe(&line(tv, json!({"web": client}))).is_none()); // a client is missing
        assert!(parse_probe("yt-dlp: error: You must provide at least one URL.").is_none());
    }
}
