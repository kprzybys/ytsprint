//! Video ID → bestaudio. Path: watch page (ytcfg: visitor data, player, session) → player JS (sts)
//! → /youtubei/v1/player as `tv_samsung` → n/sig challenges via ejs (jsc.rs).

use crate::cookies::{self, Cookie};
use crate::jsc;
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::cookie::Jar;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use url::Url;

pub const WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
// Client definition copied from yt-dlp 2026.10.26 (_base.py, 'tv_samsung').
const TV_UA: &str =
    "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";
const TV_CLIENT_NAME: &str = "TVHTML5";
const TV_CLIENT_ID: &str = "7";
const TV_CLIENT_VERSION: &str = "5.20260707";
const ORIGIN: &str = "https://www.youtube.com";

/// A case ytsprint deliberately does not handle (it goes to yt-dlp), as opposed to an error (which is reported).
#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "not handled by ytsprint: {}", self.0)
    }
}
impl std::error::Error for Unsupported {}

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
        .unwrap()
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
    let r = bestaudio_once(video_id, use_cookies, false);
    if let Err(e) = &r
        && !e.is::<Unsupported>()
    {
        // No retry (the error is meant to be visible); the next run starts with a fresh session.
        let _ = std::fs::remove_file(session_file(use_cookies));
    }
    r
}

fn bestaudio_once(video_id: &str, use_cookies: bool, fresh: bool) -> Result<Audio> {
    let mut timings = vec![];
    let mut t = Instant::now();
    let mut lap = |what: &str, t: &mut Instant| {
        timings.push((what.to_string(), t.elapsed().as_millis()));
        *t = Instant::now();
    };

    let ctx = Ctx::new(
        use_cookies,
        fresh,
        &format!("/watch?v={video_id}&bpctr=9999999999&has_verified=1"),
    )?;
    lap(
        if ctx.sess.from_cache {
            "session (cache)"
        } else {
            "cookies + watch page"
        },
        &mut t,
    );
    let mut player = player(&ctx.http, &ctx.sess.player_id)?;
    let player_id = ctx.sess.player_id.clone();
    let sts = player.sts;
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
        std::thread::spawn(move || -> Option<String> {
            let url = format!("{ORIGIN}/oembed?format=json&url=https://www.youtube.com/watch?v={id}");
            let v: Value = http
                .get(url)
                .header("User-Agent", WEB_UA)
                .send()
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .ok()?;
            v["title"].as_str().map(str::to_string)
        })
    };

    // 3. Player API as tv_samsung (+ account headers).
    let body = json!({
        "context": {"client": {
            "clientName": TV_CLIENT_NAME, "clientVersion": TV_CLIENT_VERSION,
            "deviceMake": "Samsung", "deviceModel": "UKS9800", "userAgent": TV_UA,
            "osName": "Tizen", "osVersion": "2.4.0", "hl": "en", "timeZone": "UTC", "utcOffsetMinutes": 0
        }},
        "videoId": video_id,
        "playbackContext": {"contentPlaybackContext": {"html5Preference": "HTML5_PREF_WANTS", "signatureTimestamp": sts}},
        "contentCheckOk": true, "racyCheckOk": true
    });
    let requested_at = now_unix();
    let resp = ctx.post("player", TV_UA, TV_CLIENT_ID, TV_CLIENT_VERSION, &body)?;
    let status = resp["playabilityStatus"]["status"].as_str().unwrap_or("?");
    if status != "OK" {
        bail!("playabilityStatus {status}: {}", resp["playabilityStatus"]["reason"]);
    }
    if resp["videoDetails"]["isLive"] == true {
        return Err(Unsupported("live stream".into()).into());
    }
    let wait = preroll_wait(&resp);
    let available_at = if wait > 0.0 {
        (requested_at.ceil() + wait) as u64
    } else {
        0
    };
    lap("player API", &mut t);

    // 4. Best audio-only format.
    let fmt = resp["streamingData"]["adaptiveFormats"]
        .as_array()
        .context("no adaptiveFormats")?
        .iter()
        .filter(|f| f["mimeType"].as_str().is_some_and(|m| m.starts_with("audio/")))
        .max_by_key(|f| f["bitrate"].as_u64().unwrap_or(0))
        .context("no audio format")?;
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

    // 5. Challenges.
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
        jsc::solve(
            &player.ejs_dir,
            &player_id,
            &player.pre_path,
            player.raw.take().as_deref(),
            &requests,
        )?
    };
    lap("jsc", &mut t);

    let mut pairs: Vec<(String, String)> = stream.query_pairs().into_owned().collect();
    let mut i = 0;
    if let Some(n) = &n {
        let new_n = solved[i][n].as_str().context("n not solved")?.to_string();
        i += 1;
        for p in pairs.iter_mut().filter(|p| p.0 == "n") {
            p.1 = new_n.clone();
        }
    }
    if let Some((s, sp)) = &sig {
        pairs.push((sp.clone(), solved[i][s].as_str().context("sig not solved")?.to_string()));
    }
    stream.query_pairs_mut().clear().extend_pairs(pairs);

    let d = &resp["videoDetails"];
    Ok(Audio {
        id: video_id.to_string(),
        title: d["title"]
            .as_str()
            .map(str::to_string)
            .or_else(|| title_job.join().ok().flatten())
            .unwrap_or_else(|| video_id.to_string()),
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
    use_cookies: bool,
}

impl Ctx {
    /// `page`: path of the page to read the session from when there is none in the cache.
    pub fn new(use_cookies: bool, fresh: bool, page: &str) -> Result<Ctx> {
        let jar = Arc::new(Jar::default());
        let yt: Url = ORIGIN.parse()?;
        let account: Vec<Cookie> = if use_cookies {
            cookies::firefox_youtube()?
        } else {
            vec![]
        };
        if use_cookies && !cookies::logged_in(&account) {
            bail!("no YouTube login in the Firefox cookies");
        }
        for c in &account {
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
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let sess = session(&http, page, use_cookies, fresh)?;
        Ok(Ctx {
            http,
            account,
            sess,
            use_cookies,
        })
    }

    /// POST /youtubei/v1/<endpoint>; `body` must already contain `context`.
    pub fn post(&self, endpoint: &str, ua: &str, client_id: &str, client_version: &str, body: &Value) -> Result<Value> {
        let s = &self.sess;
        let mut req = self
            .http
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("User-Agent", ua)
            .header("X-Youtube-Client-Name", client_id)
            .header("X-Youtube-Client-Version", client_version)
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
        let v: Value = req.json(body).send()?.error_for_status()?.json()?;
        // diagnostics: YTSPRINT_DUMP=<dir> saves the API responses
        if let Ok(dir) = std::env::var("YTSPRINT_DUMP") {
            let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
            let _ = std::fs::write(format!("{dir}/{endpoint}-{n:02}.json"), v.to_string());
        }
        Ok(v)
    }
}

// ---------------------------------------------------------------------------------------------
// Cache: session (ytcfg from the watch page) and player (sts + ejs-preprocessed JS).

const SESSION_TTL_SECS: u64 = 3600;

pub fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let dir = std::path::PathBuf::from(base).join("ytsprint");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn session_file(use_cookies: bool) -> std::path::PathBuf {
    cache_dir().join(if use_cookies {
        "session-account.json"
    } else {
        "session-anon.json"
    })
}

pub struct Session {
    visitor: String,
    player_id: String,
    logged_in: bool,
    session_index: Option<String>,
    delegated: Option<String>,
    user_session: Option<String>,
    from_cache: bool,
}

fn session(http: &Client, page: &str, use_cookies: bool, fresh: bool) -> Result<Session> {
    let file = session_file(use_cookies);
    if !fresh
        && let Ok(v) = std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|s| Ok(serde_json::from_str::<Value>(&s)?))
    {
        let age = now_unix() as u64 - v["saved_at"].as_u64().unwrap_or(0);
        if age < SESSION_TTL_SECS {
            let s = |k: &str| v[k].as_str().map(str::to_string);
            return Ok(Session {
                visitor: s("visitor").context("cache: visitor")?,
                player_id: s("player_id").context("cache: player_id")?,
                logged_in: v["logged_in"] == true,
                session_index: s("session_index"),
                delegated: s("delegated"),
                user_session: s("user_session"),
                from_cache: true,
            });
        }
    }
    let page = http
        .get(format!("{ORIGIN}{page}"))
        .header("User-Agent", WEB_UA)
        .header("Accept-Language", "en-us,en;q=0.5")
        .send()?
        .error_for_status()?
        .text()?;
    let visitor = capture(r#""VISITOR_DATA":"([^"]+)""#, &page)
        .context("no VISITOR_DATA")?
        .to_string();
    let player_id = capture(r#""PLAYER_JS_URL":"/s/player/([0-9a-fA-F]{8,})/"#, &page)
        .or_else(|| capture(r"/s/player/([0-9a-fA-F]{8,})/", &page))
        .context("no player id")?
        .to_string();
    let logged_in = capture(r#""LOGGED_IN":(true|false)"#, &page) == Some("true");
    let session_index = capture(r#""SESSION_INDEX":"?([0-9]+)"#, &page).map(str::to_string);
    let datasync = capture(r#""DATASYNC_ID":"([^"]*)""#, &page).unwrap_or("");
    // "delegated||user" (secondary channel) or "user||" (primary)
    let (delegated, user_session) = match datasync.split_once("||") {
        Some((a, b)) if !b.is_empty() => (Some(a.to_string()), Some(b.to_string())),
        Some((a, _)) if !a.is_empty() => (None, Some(a.to_string())),
        _ => (None, None),
    };
    if use_cookies && !logged_in {
        bail!("watch page: LOGGED_IN=false despite the cookies (expired session?)");
    }
    let saved = json!({
        "saved_at": now_unix() as u64, "visitor": visitor, "player_id": player_id, "logged_in": logged_in,
        "session_index": session_index, "delegated": delegated, "user_session": user_session,
    });
    let _ = std::fs::write(&file, saved.to_string());
    Ok(Session {
        visitor,
        player_id,
        logged_in,
        session_index,
        delegated,
        user_session,
        from_cache: false,
    })
}

struct PlayerData {
    sts: u64,
    /// player prepared by this ejs version (it may not exist yet)
    pre_path: std::path::PathBuf,
    ejs_dir: std::path::PathBuf,
    /// Raw base.js when there is no preprocessed one yet (ytsprint-jsd will prepare it).
    raw: Option<String>,
}

fn player(http: &Client, player_id: &str) -> Result<PlayerData> {
    let dir = cache_dir();
    let sts_file = dir.join(format!("player-{player_id}.sts"));
    let ejs_dir = crate::locate::yt_dlp()?.ejs_dir;
    let pre_file = dir.join(format!("player-{player_id}-ejs{}.pre.js", jsc::ejs_tag(&ejs_dir)?));
    // only check that it exists; jsd keeps the player in memory and reads the file once
    if let (Ok(sts), true) = (std::fs::read_to_string(&sts_file), pre_file.exists())
        && let Ok(sts) = sts.trim().parse()
    {
        return Ok(PlayerData {
            sts,
            pre_path: pre_file,
            ejs_dir,
            raw: None,
        });
    }
    let raw = http
        .get(format!("{ORIGIN}/s/player/{player_id}/player_ias.vflset/en_US/base.js"))
        .header("User-Agent", WEB_UA)
        .send()?
        .error_for_status()?
        .text()?;
    let sts: u64 = capture(r"(?:signatureTimestamp|sts)\s*:\s*([0-9]{5})", &raw)
        .context("no sts in player JS")?
        .parse()?;
    let _ = std::fs::write(&sts_file, sts.to_string());
    // A new player or a new ejs: the files of older players are no longer needed (~4 MB each).
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("player-") && p != sts_file && p != pre_file {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(PlayerData {
        sts,
        pre_path: pre_file,
        ejs_dir,
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
    }
}
