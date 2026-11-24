//! Video ID → bestaudio. Path: session (cached; from a YouTube page) → player JS (sts, cached)
//! → /youtubei/v1/player as `web_creator` (Premium) or `tv_samsung` → n/sig challenges via ytsprint-jsd (jsc.rs).

use crate::cookies::{self, Cookie};
use crate::fsutil;
use crate::jsc;
use crate::locate::{InnertubeClient, Settings};
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::cookie::Jar;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

pub const WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const ORIGIN: &str = "https://www.youtube.com";
/// A pre-roll wait longer than this is treated as an error rather than a silent long sleep.
const MAX_PREROLL_WAIT_SECS: f64 = 60.0;
const TITLE_TIMEOUT: Duration = Duration::from_secs(2);

/// A case ytsprint deliberately does not handle (it goes to yt-dlp), as opposed to an error (which is reported).
#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "not handled by ytsprint: {}", self.0)
    }
}
impl std::error::Error for Unsupported {}

/// Marks errors that suggest the cached session is no longer valid (HTTP 4xx, logged out, page without ytcfg).
/// Only these drop the session cache; an unplayable video or jsd being down does not.
#[derive(Debug)]
pub struct SessionError;
impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "session rejected")
    }
}

pub fn is_session_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<SessionError>().is_some()
}

pub struct Audio {
    pub id: String,
    pub title: String,
    pub duration: Option<u64>,
    pub url: String,
    pub itag: u64,
    pub mime: String,
    pub bitrate: u64,
    pub sample_rate: Option<u64>,
    /// Unix time from which the URL works (pre-roll ad); 0 = right away.
    pub available_at: u64,
    pub timings: Vec<(String, u128)>,
}

fn capture<'a>(re: &str, text: &'a str) -> Option<&'a str> {
    Regex::new(re)
        .expect("valid regex")
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn bestaudio(video_id: &str, use_cookies: bool) -> Result<Audio> {
    let ctx = Ctx::new(
        use_cookies,
        &format!("/watch?v={video_id}&bpctr=9999999999&has_verified=1"),
    )?;
    let r = bestaudio_with(&ctx, video_id);
    if let Err(e) = &r {
        ctx.drop_session_if_rejected(e);
    }
    r
}

fn bestaudio_with(ctx: &Ctx, video_id: &str) -> Result<Audio> {
    let mut timings = ctx.timings.clone();
    let mut t = Instant::now();
    let mut lap = |what: &str, t: &mut Instant| {
        timings.push((what.to_string(), t.elapsed().as_millis()));
        *t = Instant::now();
    };

    let player_js = &ctx.settings.player_js;
    let ejs = jsc::Ejs::at(ctx.ejs_dir.clone(), player_js)?;
    let player = player(&ctx.http, &ctx.sess.player_id, player_js, &ejs)?;
    lap(
        if player.raw.is_none() {
            "player (cache)"
        } else {
            "player JS"
        },
        &mut t,
    );

    // The tv_samsung client returns no title: oEmbed in the background, alongside player API + jsc.
    let title_job = {
        let http = ctx.http.clone();
        let id = video_id.to_string();
        std::thread::spawn(move || -> Result<String> {
            let url = format!("{ORIGIN}/oembed?format=json&url=https://www.youtube.com/watch?v={id}");
            let v: Value = http
                .get(url)
                .header("User-Agent", WEB_UA)
                .timeout(TITLE_TIMEOUT)
                .send()?
                .error_for_status()?
                .json()?;
            v["title"]
                .as_str()
                .map(str::to_string)
                .context("oEmbed without a title")
        })
    };

    // Player API (+ account headers).
    // web_creator only for Premium accounts: without Premium its streams need a PO token. tv_samsung is not used for
    // them, because it shows a pre-roll ad (and the stream is 403 until it ends) on some videos even with Premium.
    let client = if ctx.sess.premium {
        &ctx.settings.web_creator
    } else {
        &ctx.settings.tv_samsung
    };
    let body = json!({
        "context": {"client": client.client},
        "videoId": video_id,
        "playbackContext": {"contentPlaybackContext": {"html5Preference": "HTML5_PREF_WANTS", "signatureTimestamp": player.sts}},
        "contentCheckOk": true, "racyCheckOk": true
    });
    let requested_at = now_unix();
    let resp = ctx.post("player", client, &body)?;
    let status = resp["playabilityStatus"]["status"].as_str().unwrap_or("?");
    if status != "OK" {
        bail!("playabilityStatus {status}: {}", resp["playabilityStatus"]["reason"]);
    }
    if is_live(&resp) {
        return Err(Unsupported("live stream".into()).into());
    }
    let wait = preroll_wait(&resp);
    if !wait.is_finite() || wait > MAX_PREROLL_WAIT_SECS {
        bail!("pre-roll ad wait of {wait} s (over {MAX_PREROLL_WAIT_SECS} s; no YouTube Premium in the cookies?)");
    }
    let available_at = if wait > 0.0 {
        (requested_at.ceil() + wait) as u64
    } else {
        0
    };
    lap("player API", &mut t);

    let fmt = best_audio(&resp)?;
    let (base_url, sig) = if let Some(u) = fmt["url"].as_str() {
        (u.to_string(), None)
    } else {
        let cipher = fmt["signatureCipher"]
            .as_str()
            .context("no url and no signatureCipher")?;
        let q: std::collections::HashMap<_, _> = url::form_urlencoded::parse(cipher.as_bytes()).into_owned().collect();
        let sp = q.get("sp").cloned().unwrap_or_else(|| "signature".into());
        (
            q.get("url").context("cipher without url")?.clone(),
            Some((q.get("s").context("cipher without s")?.clone(), sp)),
        )
    };
    let mut stream = Url::parse(&base_url)?;
    let n = stream
        .query_pairs()
        .find(|(k, _)| k == "n")
        .map(|(_, v)| v.into_owned());

    // Challenges.
    let mut requests = vec![];
    if let Some(n) = &n {
        requests.push(json!({"type": "n", "challenges": [n]}));
    }
    if let Some((s, _)) = &sig {
        requests.push(json!({"type": "sig", "challenges": [s]}));
    }
    let solved = if requests.is_empty() {
        vec![]
    } else {
        let p = jsc::Player {
            id: &ctx.sess.player_id,
            ejs: &ejs,
            pre_path: &player.pre_path,
            raw: player.raw.as_deref(),
        };
        jsc::solve(&p, &requests)?
    };
    lap("jsc", &mut t);

    let solution = |i: usize, challenge: &str, what: &str| -> Result<String> {
        let s = solved
            .get(i)
            .and_then(|r| r[challenge].as_str())
            .with_context(|| format!("{what} not solved"))?;
        // an empty or unchanged value means the solver did not really run: the stream would fail later
        if s.is_empty() || (what == "n" && s == challenge) {
            bail!("{what} challenge: implausible solution {s:?}");
        }
        Ok(s.to_string())
    };
    let mut pairs: Vec<(String, String)> = stream.query_pairs().into_owned().collect();
    let mut i = 0;
    if let Some(n) = &n {
        let new_n = solution(i, n, "n")?;
        i += 1;
        for p in pairs.iter_mut().filter(|p| p.0 == "n") {
            p.1 = new_n.clone();
        }
    }
    if let Some((s, sp)) = &sig {
        pairs.push((sp.clone(), solution(i, s, "sig")?));
    }
    stream.query_pairs_mut().clear().extend_pairs(pairs);

    let d = &resp["videoDetails"];
    let title = match d["title"].as_str() {
        Some(t) => t.to_string(),
        None => match title_job.join() {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                crate::fsutil::log(&format!("WARN title {video_id}: {e:#}"));
                video_id.to_string()
            }
            Err(_) => bail!("title thread panicked"),
        },
    };
    lap("title", &mut t);
    Ok(Audio {
        id: video_id.to_string(),
        title,
        duration: d["lengthSeconds"].as_str().and_then(|s| s.parse().ok()),
        url: stream.to_string(),
        itag: fmt["itag"].as_u64().unwrap_or(0),
        mime: fmt["mimeType"].as_str().unwrap_or("").to_string(),
        bitrate: fmt["bitrate"].as_u64().unwrap_or(0),
        sample_rate: fmt["audioSampleRate"].as_str().and_then(|s| s.parse().ok()),
        available_at,
        timings,
    })
}

/// Live and post-live streams go to yt-dlp. `videoDetails.isLive` alone is not enough when the client omits
/// fields; an HLS manifest without a duration is live too.
fn is_live(resp: &Value) -> bool {
    let d = &resp["videoDetails"];
    let sd = &resp["streamingData"];
    d["isLive"] == true
        || d["isPostLiveDvr"] == true
        || (sd["hlsManifestUrl"].is_string() && d["lengthSeconds"].as_str().is_none_or(|l| l == "0"))
}

/// Best audio-only format: the original/default track (not an auto-dub), not DRC, then the highest bitrate.
/// yt-dlp prefers the original language the same way (language_preference).
fn best_audio(resp: &Value) -> Result<&Value> {
    let original = |f: &Value| -> bool {
        match f.get("audioTrack") {
            None => true,
            Some(t) => t["audioIsDefault"] == true || f["xtags"].as_str().is_some_and(|x| x.contains("acont=original")),
        }
    };
    resp["streamingData"]["adaptiveFormats"]
        .as_array()
        .context("no adaptiveFormats")?
        .iter()
        .filter(|f| f["mimeType"].as_str().is_some_and(|m| m.starts_with("audio/")))
        .max_by_key(|f| (original(f), f["isDrc"] != true, f["bitrate"].as_u64().unwrap_or(0)))
        .context("no audio format")
}

/// Sum of pre-roll ad durations, in seconds (the skip time when the ad can be skipped).
/// yt-dlp: _video.py _get_available_at_timestamp.
fn preroll_wait(resp: &Value) -> f64 {
    let mut ads = vec![];
    for p in resp["adPlacements"].as_array().into_iter().flatten() {
        let r = &p["adPlacementRenderer"];
        if r["config"]["adPlacementConfig"]["kind"] == "AD_PLACEMENT_KIND_START" {
            collect_ads(&r["renderer"], &mut ads);
        }
    }
    for s in resp["adSlots"].as_array().into_iter().flatten() {
        let r = &s["adSlotRenderer"];
        if r["adSlotMetadata"]["triggerEvent"] == "SLOT_TRIGGER_EVENT_BEFORE_CONTENT" {
            collect_ads(&r["fulfillmentContent"], &mut ads);
        }
    }
    ads.iter()
        .filter_map(|ad| {
            if let Some(ms) = ad["skipOffsetMilliseconds"].as_f64() {
                return Some(ms / 1000.0);
            }
            let vars = ad["playerVars"].as_str()?;
            url::form_urlencoded::parse(vars.as_bytes())
                .find(|(k, _)| k == "length_seconds")
                .and_then(|(_, v)| v.parse().ok())
        })
        .sum()
}

fn collect_ads<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == "instreamVideoAdRenderer" {
                    out.push(x);
                } else {
                    collect_ads(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_ads(x, out)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP context: client with cookies + session; InnerTube API requests with account headers.

pub struct Ctx {
    pub http: Client,
    account: Vec<Cookie>,
    pub sess: Session,
    pub settings: Settings,
    ejs_dir: PathBuf,
    session_path: PathBuf,
    use_cookies: bool,
    /// timings of building the context (cookies, session)
    pub timings: Vec<(String, u128)>,
}

impl Ctx {
    /// `page`: path of the page to read the session from when there is none in the cache.
    pub fn new(use_cookies: bool, page: &str) -> Result<Ctx> {
        let ytdlp = crate::locate::yt_dlp()?;
        let settings = ytdlp.settings()?;
        let t = Instant::now();
        let jar = Arc::new(Jar::default());
        let yt: Url = ORIGIN.parse()?;
        let (account, account_key) = if use_cookies {
            let (c, db) = cookies::firefox_youtube()?;
            if !cookies::logged_in(&c) {
                bail!("no YouTube login in the Firefox cookies");
            }
            let key = cookies::account_key(&c, &db);
            (c, key)
        } else {
            (vec![], "anon".to_string())
        };
        // newest last: for cookies with the same domain/path/name the jar keeps the one added last,
        // which then matches what `cookies::get` (newest first) signs with
        for c in account.iter().rev() {
            let secure = if c.secure { "; Secure" } else { "" };
            jar.add_cookie_str(
                &format!("{}={}; Domain={}; Path={}{secure}", c.name, c.value, c.host, c.path),
                &yt,
            );
        }
        jar.add_cookie_str("PREF=hl=en&tz=UTC; Domain=.youtube.com; Path=/", &yt);
        if !use_cookies {
            jar.add_cookie_str("SOCS=CAI; Domain=.youtube.com; Path=/", &yt);
        }
        // short limits: on a bad connection report an error quickly instead of waiting the default 30 s
        let http = Client::builder()
            .cookie_provider(jar)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()?;
        let cookies_ms = t.elapsed().as_millis();
        let session_path = fsutil::cache_dir()?.join(format!("session-{account_key}.json"));
        let t = Instant::now();
        let sess = session(&http, &session_path, page, use_cookies)?;
        let timings = vec![
            ("cookies".to_string(), cookies_ms),
            (
                if sess.from_cache {
                    "session (cache)"
                } else {
                    "session (page)"
                }
                .to_string(),
                t.elapsed().as_millis(),
            ),
        ];
        Ok(Ctx {
            http,
            account,
            sess,
            settings,
            ejs_dir: ytdlp.ejs_dir,
            session_path,
            use_cookies,
            timings,
        })
    }

    /// After a session-related error the next run starts with a fresh session (no retry now: the error
    /// is meant to be visible).
    pub fn drop_session_if_rejected(&self, e: &anyhow::Error) {
        if is_session_error(e) {
            match std::fs::remove_file(&self.session_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => fsutil::log(&format!("WARN cannot remove {}: {err}", self.session_path.display())),
            }
        }
    }

    /// POST /youtubei/v1/<endpoint>; `body` must already contain `context`.
    pub fn post(&self, endpoint: &str, client: &InnertubeClient, body: &Value) -> Result<Value> {
        let s = &self.sess;
        let mut req = self
            .http
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("User-Agent", client.user_agent.as_deref().unwrap_or(WEB_UA))
            .header("X-Youtube-Client-Name", &client.id)
            .header("X-Youtube-Client-Version", &client.version)
            .header("X-Goog-Visitor-Id", &s.visitor)
            .header("Origin", ORIGIN);
        if self.use_cookies {
            if let Some(d) = &s.delegated {
                req = req.header("X-Goog-PageId", d);
            }
            if s.delegated.is_some() || s.session_index.is_some() {
                req = req.header("X-Goog-AuthUser", s.session_index.as_deref().unwrap_or("0"));
            }
            if let Some(auth) = cookies::sid_authorization(&self.account, ORIGIN, s.user_session.as_deref()) {
                req = req.header("Authorization", auth).header("X-Origin", ORIGIN);
            }
            if s.logged_in {
                req = req.header("X-Youtube-Bootstrap-Logged-In", "true");
            }
        }
        let resp = req.json(body).send()?;
        let status = resp.status();
        if !status.is_success() {
            // InnerTube explains errors in the body; keep that message
            let msg = resp
                .json::<Value>()
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_default();
            let err = anyhow::anyhow!("/youtubei/v1/{endpoint}: HTTP {status} {msg}");
            return Err(if status.is_client_error() {
                err.context(SessionError)
            } else {
                err
            });
        }
        let v: Value = resp.json()?;
        // diagnostics: YTSPRINT_DUMP=<dir> saves the API responses
        if let Ok(dir) = std::env::var("YTSPRINT_DUMP") {
            let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
            std::fs::write(format!("{dir}/{endpoint}-{n:02}.json"), v.to_string())
                .with_context(|| format!("YTSPRINT_DUMP {dir}"))?;
        }
        Ok(v)
    }
}

// ---------------------------------------------------------------------------------------------
// Cache: session (ytcfg from a YouTube page) and player (sts; the prepared player is written by jsc.rs).

const SESSION_TTL_SECS: u64 = 3600;

pub struct Session {
    visitor: String,
    pub player_id: String,
    logged_in: bool,
    session_index: Option<String>,
    delegated: Option<String>,
    user_session: Option<String>,
    /// YouTube Premium account (decides the player client)
    premium: bool,
    pub from_cache: bool,
}

fn session(http: &Client, file: &std::path::Path, page: &str, use_cookies: bool) -> Result<Session> {
    if let Some(v) = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        // a clock set back or a broken value: treat as expired
        let age = (now_unix() as u64).checked_sub(v["saved_at"].as_u64().unwrap_or(0));
        let s = |k: &str| v[k].as_str().map(str::to_string);
        if let (Some(age), Some(visitor), Some(player_id), Some(premium)) =
            (age, s("visitor"), s("player_id"), v["premium"].as_bool())
            && age < SESSION_TTL_SECS
        {
            return Ok(Session {
                visitor,
                player_id,
                logged_in: v["logged_in"] == true,
                session_index: s("session_index"),
                delegated: s("delegated"),
                user_session: s("user_session"),
                premium,
                from_cache: true,
            });
        }
    }
    let resp = http
        .get(format!("{ORIGIN}{page}"))
        .header("User-Agent", WEB_UA)
        .header("Accept-Language", "en-us,en;q=0.5")
        .send()?;
    let status = resp.status();
    if !status.is_success() {
        let err = anyhow::anyhow!("{ORIGIN}{page}: HTTP {status}");
        return Err(if status.is_client_error() {
            err.context(SessionError)
        } else {
            err
        });
    }
    let page_html = resp.text()?;
    let visitor = capture(r#""VISITOR_DATA":"([^"]+)""#, &page_html)
        .context("no VISITOR_DATA on the page")
        .context(SessionError)?
        .to_string();
    let player_id = capture(r#""PLAYER_JS_URL":"/s/player/([0-9a-f]{8,16})/"#, &page_html)
        .or_else(|| capture(r"/s/player/([0-9a-f]{8,16})/", &page_html))
        .context("no player id on the page")
        .context(SessionError)?
        .to_string();
    let logged_in = capture(r#""LOGGED_IN":(true|false)"#, &page_html) == Some("true");
    let session_index = capture(r#""SESSION_INDEX":"?([0-9]+)"#, &page_html).map(str::to_string);
    let datasync = capture(r#""DATASYNC_ID":"([^"]*)""#, &page_html).unwrap_or("");
    // "delegated||user" (secondary channel) or "user||" (primary)
    let (delegated, user_session) = match datasync.split_once("||") {
        Some((a, b)) if !b.is_empty() => (Some(a.to_string()), Some(b.to_string())),
        Some((a, _)) if !a.is_empty() => (None, Some(a.to_string())),
        _ => (None, None),
    };
    if use_cookies && !logged_in {
        return Err(
            anyhow::anyhow!("LOGGED_IN=false despite the cookies (expired login in Firefox?)").context(SessionError),
        );
    }
    let premium = use_cookies
        && is_premium(&page_html)
            .context("no topbarLogoRenderer on the page: cannot tell whether the account has Premium")?;
    let saved = json!({
        "saved_at": now_unix() as u64, "visitor": visitor, "player_id": player_id, "logged_in": logged_in,
        "session_index": session_index, "delegated": delegated, "user_session": user_session,
        "premium": premium,
    });
    fsutil::write_atomic(file, saved.to_string().as_bytes())?;
    Ok(Session {
        visitor,
        player_id,
        logged_in,
        session_index,
        delegated,
        user_session,
        premium,
        from_cache: false,
    })
}

/// The page's logo says whether the account has Premium (yt-dlp: _is_premium_subscriber); None = no logo found.
fn is_premium(page_html: &str) -> Option<bool> {
    let logo = capture(r#""topbarLogoRenderer":\{(.{0,600}?)"endpoint""#, page_html)?;
    Some(logo.contains("YOUTUBE_PREMIUM_LOGO") || logo.to_lowercase().contains("premium"))
}

struct PlayerData {
    sts: u64,
    /// player prepared by this ejs version (it may not exist yet)
    pre_path: PathBuf,
    /// raw base.js when there is no prepared one yet (ytsprint-jsd will prepare it)
    raw: Option<String>,
}

fn player(http: &Client, player_id: &str, player_js: &str, ejs: &jsc::Ejs) -> Result<PlayerData> {
    let dir = fsutil::cache_dir()?;
    let sts_file = dir.join(format!("player-{player_id}.sts"));
    let pre_file = dir.join(format!("player-{player_id}-ejs{}.pre.js", ejs.tag));
    // only check that it exists; jsd keeps the player in memory and asks for the file when it needs it
    if let (Ok(sts), true) = (std::fs::read_to_string(&sts_file), pre_file.exists())
        && let Ok(sts) = sts.trim().parse()
    {
        return Ok(PlayerData {
            sts,
            pre_path: pre_file,
            raw: None,
        });
    }
    let raw = http
        .get(format!("{ORIGIN}/s/player/{player_id}/{player_js}"))
        .header("User-Agent", WEB_UA)
        .send()?
        .error_for_status()?
        .text()?;
    let sts: u64 = capture(r"(?:signatureTimestamp|sts)\s*:\s*([0-9]{5})", &raw)
        .context("no sts in player JS")?
        .parse()?;
    fsutil::write_atomic(&sts_file, sts.to_string().as_bytes())?;
    // A new player or a new ejs: the files of older players are no longer needed (~4 MB each).
    // Temporary files of other processes (".tmp-<pid>") are left alone.
    for e in std::fs::read_dir(&dir)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("player-") && !name.contains(".tmp-") && p != sts_file && p != pre_file {
            std::fs::remove_file(&p).with_context(|| format!("remove old {}", p.display()))?;
        }
    }
    Ok(PlayerData {
        sts,
        pre_path: pre_file,
        raw: Some(raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preroll_wait_sums_ads_before_content() {
        let resp = json!({
            "adPlacements": [
                {"adPlacementRenderer": {"config": {"adPlacementConfig": {"kind": "AD_PLACEMENT_KIND_START"}},
                    "renderer": {"x": {"instreamVideoAdRenderer": {"playerVars": "length_seconds=6&foo=1"}}}}},
                {"adPlacementRenderer": {"config": {"adPlacementConfig": {"kind": "AD_PLACEMENT_KIND_END"}},
                    "renderer": {"instreamVideoAdRenderer": {"playerVars": "length_seconds=30"}}}}
            ],
            "adSlots": [
                {"adSlotRenderer": {"adSlotMetadata": {"triggerEvent": "SLOT_TRIGGER_EVENT_BEFORE_CONTENT"},
                    "fulfillmentContent": {"a": [{"instreamVideoAdRenderer": {"skipOffsetMilliseconds": 5000, "playerVars": "length_seconds=15"}}]}}}
            ]
        });
        assert_eq!(preroll_wait(&resp), 11.0); // 6 s + skippable after 5 s; the end-roll ad does not count
        assert_eq!(preroll_wait(&json!({})), 0.0);
        let inf = json!({"adSlots": [{"adSlotRenderer": {"adSlotMetadata": {"triggerEvent": "SLOT_TRIGGER_EVENT_BEFORE_CONTENT"},
            "fulfillmentContent": {"instreamVideoAdRenderer": {"playerVars": "length_seconds=inf"}}}}]});
        assert!(!preroll_wait(&inf).is_finite()); // rejected by the caller
    }

    #[test]
    fn best_audio_prefers_original_track_over_dub_and_drc() {
        let resp = json!({"streamingData": {"adaptiveFormats": [
            {"itag": 1, "mimeType": "audio/webm", "bitrate": 200000, "audioTrack": {"audioIsDefault": false}, "xtags": "acont=dubbed-auto"},
            {"itag": 2, "mimeType": "audio/webm", "bitrate": 160000, "audioTrack": {"audioIsDefault": true}},
            {"itag": 3, "mimeType": "audio/webm", "bitrate": 170000, "audioTrack": {"audioIsDefault": true}, "isDrc": true},
            {"itag": 4, "mimeType": "video/mp4", "bitrate": 900000}
        ]}});
        assert_eq!(best_audio(&resp).unwrap()["itag"], 2);
        let plain = json!({"streamingData": {"adaptiveFormats": [
            {"itag": 140, "mimeType": "audio/mp4", "bitrate": 130000},
            {"itag": 251, "mimeType": "audio/webm", "bitrate": 140000}
        ]}});
        assert_eq!(best_audio(&plain).unwrap()["itag"], 251);
    }

    #[test]
    fn premium_from_the_logo() {
        let page = |icon: &str, tip: &str| {
            format!(
                r#"{{"topbarLogoRenderer":{{"iconImage":{{"iconType":"{icon}"}},"tooltipText":{{"runs":[{{"text":"{tip}"}}]}},"endpoint":{{}}}}}}"#
            )
        };
        assert_eq!(is_premium(&page("YOUTUBE_PREMIUM_LOGO", "YouTube Home")), Some(true));
        assert_eq!(is_premium(&page("YOUTUBE_LOGO", "YouTube Premium Home")), Some(true));
        assert_eq!(is_premium(&page("YOUTUBE_LOGO", "YouTube Home")), Some(false));
        assert_eq!(is_premium("<html>Premium</html>"), None);
    }

    #[test]
    fn live_detection() {
        assert!(is_live(&json!({"videoDetails": {"isLive": true}})));
        assert!(is_live(&json!({"videoDetails": {"isPostLiveDvr": true}})));
        assert!(is_live(&json!({"streamingData": {"hlsManifestUrl": "https://x"}})));
        assert!(!is_live(
            &json!({"videoDetails": {"lengthSeconds": "3600"}, "streamingData": {"hlsManifestUrl": "https://x"}})
        ));
        assert!(!is_live(&json!({"videoDetails": {"lengthSeconds": "3600"}})));
    }
}
