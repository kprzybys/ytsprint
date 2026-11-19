//! ytsprint: a fast path for mpv's ytdl_hook (`ytdl_path`) for YouTube: a video (bestaudio), playlists, search,
//! with Firefox Premium cookies. What it deliberately does not do (live streams, other sites,
//! other arguments) goes to yt-dlp. An error on the path it does handle is NOT hidden by yt-dlp:
//! ytsprint exits with an error that mpv shows. See docs/plan.md.
//!
//!   ytsprint <yt-dlp arguments>          as called by mpv: JSON on stdout or exec yt-dlp
//!   ytsprint --url <id|URL> [--cookies]  debugging: the stream URL + timings on stderr
//!   ytsprint search <N> <query>          search: lines "id<TAB>duration_s<TAB>title" (for fzf in `yt`)
//!   ytsprint jsd                          ytsprint-jsd service: deno with the ejs solver and players in memory

mod browse;
mod cookies;
mod extract;
mod jsc;
mod locate;

use anyhow::{Result, bail};
use serde_json::json;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn state_log(line: &str) {
    let dir = std::env::var("XDG_STATE_HOME")
        .unwrap_or_else(|_| format!("{}/.local/state", std::env::var("HOME").unwrap_or_default()))
        + "/ytsprint";
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/log"))
    {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{ts} {line}");
    }
}

enum Target {
    Video(String),
    Playlist(String),
}

/// What a URL points to. Like yt-dlp: watch?v=X&list=Y with --no-playlist is a single video.
fn target(arg: &str, no_playlist: bool) -> Option<Target> {
    let list = url::Url::parse(arg)
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "list").map(|(_, v)| v.into_owned()));
    match (video_id_any(arg), list) {
        (Some(id), Some(_)) if no_playlist => Some(Target::Video(id)),
        (_, Some(list))
            if list
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Some(Target::Playlist(list))
        }
        (Some(id), None) => Some(Target::Video(id)),
        _ => None,
    }
}

/// Video ID from a YouTube URL (or a bare ID). None = not a single video (playlist, search, other site).
fn video_id(arg: &str) -> Option<String> {
    if arg.contains("list=") {
        return None;
    }
    video_id_any(arg)
}

fn video_id_any(arg: &str) -> Option<String> {
    let ok = |s: &str| s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok(arg) {
        return Some(arg.to_string());
    }
    let u = url::Url::parse(arg).ok()?;
    let host = u
        .host_str()?
        .trim_start_matches("www.")
        .trim_start_matches("m.")
        .trim_start_matches("music.");
    let id = match host {
        "youtube.com" if u.path() == "/watch" => u.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
        "youtube.com" => u
            .path()
            .strip_prefix("/shorts/")
            .or(u.path().strip_prefix("/live/"))?
            .to_string(),
        "youtu.be" => u.path().trim_start_matches('/').to_string(),
        _ => return None,
    };
    ok(&id).then_some(id)
}

struct Request {
    target: Target,
    cookies: bool,
}

/// The subset of yt-dlp arguments that mpv (ytdl_hook) passes. Anything unknown → None (fallback).
fn parse_ytdl_args(args: &[String]) -> Option<Request> {
    let (mut json, mut format_ok, mut cookies, mut url, mut no_playlist) = (false, false, false, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-J" | "--dump-single-json" => json = true,
            "--no-warnings" | "--flat-playlist" | "--all-subs" => {}
            "--no-playlist" => no_playlist = true,
            "--sub-format" => {
                it.next()?;
            }
            "--format" | "-f" => format_ok = matches!(it.next()?.as_str(), "bestaudio" | "ba" | "bestaudio/best"),
            "--cookies-from-browser" => {
                if it.next()? != "firefox" {
                    return None;
                }
                cookies = true;
            }
            "--" => url = it.next().cloned(),
            _ => return None,
        }
    }
    if !(json && format_ok) || it.next().is_some() {
        return None;
    }
    Some(Request {
        target: target(&url?, no_playlist)?,
        cookies,
    })
}

fn wait_until(available_at: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let left = available_at as f64 - now;
    if left > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(left));
    }
}

fn timings(a: &extract::Audio) -> String {
    a.timings
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn native(req: &Request) -> Result<()> {
    match &req.target {
        Target::Video(id) => native_video(id, req.cookies),
        Target::Playlist(list) => native_playlist(list, req.cookies),
    }
}

fn native_playlist(list: &str, cookies: bool) -> Result<()> {
    let t = Instant::now();
    let (title, entries) = browse::playlist(list, cookies)?;
    let n = entries.len();
    let entries: Vec<_> = entries
        .into_iter()
        .map(|e| {
            json!({
                "_type": "url", "ie_key": "Youtube", "id": e.id, "title": e.title, "duration": e.duration,
                "url": format!("https://www.youtube.com/watch?v={}", e.id),
            })
        })
        .collect();
    let out = json!({
        "_type": "playlist", "id": list, "title": title,
        "extractor": "youtube:tab", "extractor_key": "YoutubeTab",
        "webpage_url": format!("https://www.youtube.com/playlist?list={list}"),
        "entries": entries,
    });
    println!("{out}");
    state_log(&format!(
        "ok playlist {list} cookies={cookies} entries={n} total={}ms",
        t.elapsed().as_millis()
    ));
    Ok(())
}

fn native_video(id: &str, cookies: bool) -> Result<()> {
    let t = Instant::now();
    let a = extract::bestaudio(id, cookies)?;
    let ext = if a.mime.contains("webm") { "webm" } else { "m4a" };
    let acodec = if a.mime.contains("opus") { "opus" } else { "mp4a.40.2" };
    let out = json!({
        "_type": "video",
        "id": a.id,
        "title": a.title,
        "duration": a.duration,
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "webpage_url": format!("https://www.youtube.com/watch?v={}", a.id),
        "format_id": a.itag.to_string(),
        "url": a.url,
        "protocol": "https",
        "ext": ext,
        "acodec": acodec,
        "vcodec": "none",
        "abr": a.bitrate as f64 / 1000.0,
        "tbr": a.bitrate as f64 / 1000.0,
        "asr": a.sample_rate,
        "is_live": false,
        "available_at": a.available_at,
        "http_headers": {
            "User-Agent": extract::WEB_UA,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": "en-us,en;q=0.5",
            "Sec-Fetch-Mode": "navigate",
        },
    });
    // mpv ignores available_at, so we wait ourselves (with Premium cookies there is no ad: available_at = 0).
    wait_until(a.available_at);
    println!("{out}");
    state_log(&format!(
        "ok {} cookies={} total={}ms ad_wait={} {}",
        a.id,
        cookies,
        t.elapsed().as_millis(),
        a.available_at != 0,
        timings(&a)
    ));
    Ok(())
}

fn debug_url(args: &[String]) -> Result<()> {
    let Some(target) = args.get(1) else {
        bail!("usage: ytsprint --url <id|URL> [--cookies]")
    };
    let Some(id) = video_id(target) else {
        bail!("not a single YouTube video: {target}")
    };
    let t = Instant::now();
    let a = extract::bestaudio(&id, args.iter().any(|a| a == "--cookies"))?;
    eprintln!(
        "[ytsprint] {} | itag {} {} | ad wait until {} | {}",
        a.title,
        a.itag,
        a.mime,
        a.available_at,
        timings(&a)
    );
    if std::env::var("YTSPRINT_NO_WAIT").is_err() {
        wait_until(a.available_at);
    }
    eprintln!("[ytsprint] total {} ms", t.elapsed().as_millis());
    println!("{}", a.url);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--url") => return debug_url(&args),
        Some("search") => return search(&args),
        Some("jsd") => return jsc::run_daemon(),
        Some("install-service") => return jsc::install_service(),
        _ => {}
    }
    if std::env::var("YTSPRINT_FORCE_YTDLP").is_err() {
        if let Some(req) = parse_ytdl_args(&args) {
            let what = match &req.target {
                Target::Video(id) => id.clone(),
                Target::Playlist(l) => format!("playlist {l}"),
            };
            match native(&req) {
                Ok(()) => return Ok(()),
                Err(e) if e.is::<extract::Unsupported>() => state_log(&format!("yt-dlp {what}: {e}")),
                Err(e) => {
                    state_log(&format!("ERROR {what} cookies={}: {e:#}", req.cookies));
                    eprintln!("ytsprint: {e:#}");
                    std::process::exit(1);
                }
            }
        } else {
            state_log(&format!("yt-dlp (arguments not handled): {}", args.join(" ")));
        }
    }
    // Requests ytsprint does not handle → yt-dlp with the same arguments.
    let bin = locate::yt_dlp()?.bin;
    let err = Command::new(&bin).args(&args).exec();
    bail!("exec {}: {err}", bin.display())
}

fn search(args: &[String]) -> Result<()> {
    let (Some(n), Some(query)) = (args.get(1).and_then(|n| n.parse().ok()), args.get(2)) else {
        bail!("usage: ytsprint search <N> <query> [--cookies]")
    };
    // Without the account by default: results with the account vary a lot from run to run (personalisation),
    // and before ytsprint, `yt` searched without the account too.
    let cookies = args.iter().any(|a| a == "--cookies");
    let t = Instant::now();
    let entries = browse::search(query, n, cookies)?;
    let mut out = std::io::stdout().lock();
    for e in &entries {
        let title = e.title.replace(['\t', '\n'], " ");
        writeln!(
            out,
            "{}\t{}\t{title}",
            e.id,
            e.duration.map(|d| d.to_string()).unwrap_or_default()
        )?;
    }
    state_log(&format!(
        "ok search n={n} cookies={cookies} got={} total={}ms",
        entries.len(),
        t.elapsed().as_millis()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn video_ids() {
        for u in [
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=10",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(u).as_deref(), Some("dQw4w9WgXcQ"), "{u}");
        }
        assert_eq!(video_id("https://vimeo.com/123"), None);
        assert_eq!(video_id("https://www.youtube.com/watch?v=short"), None);
        assert_eq!(video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=LL"), None);
    }

    #[test]
    fn targets_follow_yt_dlp_no_playlist_semantics() {
        let both = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc_-1";
        assert!(matches!(target(both, true), Some(Target::Video(id)) if id == "dQw4w9WgXcQ"));
        assert!(matches!(target(both, false), Some(Target::Playlist(l)) if l == "PLabc_-1"));
        assert!(
            matches!(target("https://www.youtube.com/playlist?list=LL", true), Some(Target::Playlist(l)) if l == "LL")
        );
        assert!(target("https://www.youtube.com/playlist?list=bad%20id", true).is_none());
    }

    #[test]
    fn mpv_arguments() {
        // exactly what mpv 0.37 ytdl_hook passes (with the [yt] profile)
        let a = args(
            "--no-warnings -J --flat-playlist --sub-format ass/srt/best --format bestaudio --cookies-from-browser firefox --all-subs --no-playlist -- https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        );
        let r = parse_ytdl_args(&a).unwrap();
        assert!(r.cookies && matches!(r.target, Target::Video(_)));
        // anything else goes to yt-dlp
        assert!(parse_ytdl_args(&args("-J --format bestvideo -- dQw4w9WgXcQ")).is_none());
        assert!(
            parse_ytdl_args(&args(
                "-J --format bestaudio --cookies-from-browser chrome -- dQw4w9WgXcQ"
            ))
            .is_none()
        );
        assert!(parse_ytdl_args(&args("-J --format bestaudio --write-subs -- dQw4w9WgXcQ")).is_none());
        assert!(parse_ytdl_args(&args("--format bestaudio -- dQw4w9WgXcQ")).is_none());
        assert!(parse_ytdl_args(&args("-J --format bestaudio -- https://vimeo.com/1")).is_none());
    }
}
