//! Playlists (/browse VL<id>) and search (/search) as the `web` client.
//! The parser walks the response recursively and collects video entries and continuation tokens,
//! so it does not depend on the exact page layout (it changes more often than the renderers do).

use crate::extract::Ctx;
use crate::fsutil;
use anyhow::Result;
use serde_json::{Value, json};

// yt-dlp _search.py: videos only
const SEARCH_VIDEOS: &str = "EgIQAfABAQ==";
const MAX_PLAYLIST: usize = 5000;
const MAX_SEARCH_PAGES: usize = 10;

pub struct Entry {
    pub id: String,
    pub title: String,
    pub duration: Option<u64>,
}

fn post(ctx: &Ctx, endpoint: &str, mut body: Value) -> Result<Value> {
    let web = &ctx.settings.web;
    body["context"] = json!({"client": web.client});
    ctx.post(endpoint, web, &body)
}

/// No retry (the error is meant to be visible); after a session error the next run starts with a fresh session.
fn with_ctx<T>(use_cookies: bool, page: &str, f: impl Fn(&Ctx) -> Result<T>) -> Result<T> {
    let ctx = Ctx::new(use_cookies, page)?;
    let r = f(&ctx);
    if let Err(e) = &r {
        ctx.drop_session_if_rejected(e);
    }
    r
}

pub fn playlist(list_id: &str, use_cookies: bool) -> Result<(String, Vec<Entry>)> {
    with_ctx(use_cookies, &format!("/playlist?list={list_id}"), |ctx| {
        let first = post(ctx, "browse", json!({"browseId": format!("VL{list_id}")}))?;
        if let Some(alert) = alert(&first) {
            anyhow::bail!("playlist {list_id}: {alert}");
        }
        let title = match first["metadata"]["playlistMetadataRenderer"]["title"]
            .as_str()
            .or_else(|| first["header"]["pageHeaderRenderer"]["pageTitle"].as_str())
        {
            Some(t) => t.to_string(),
            None => {
                fsutil::log(&format!("WARN playlist {list_id}: no title in the response"));
                list_id.to_string()
            }
        };
        let mut entries = vec![];
        let mut page = first;
        loop {
            let mut conts = vec![];
            walk(&page, &mut entries, &mut conts);
            match conts.pop() {
                Some(token) if entries.len() < MAX_PLAYLIST => {
                    page = post(ctx, "browse", json!({"continuation": token}))?
                }
                _ => break,
            }
        }
        dedup(&mut entries);
        if entries.len() >= MAX_PLAYLIST {
            fsutil::log(&format!("WARN playlist {list_id}: truncated to {MAX_PLAYLIST} entries"));
            entries.truncate(MAX_PLAYLIST);
        }
        if entries.is_empty() {
            anyhow::bail!("playlist {list_id}: no entries");
        }
        Ok((title, entries))
    })
}

pub fn search(query: &str, n: usize, use_cookies: bool) -> Result<Vec<Entry>> {
    with_ctx(use_cookies, "/", |ctx| {
        let mut entries = vec![];
        let mut page = post(ctx, "search", json!({"query": query, "params": SEARCH_VIDEOS}))?;
        for _ in 0..MAX_SEARCH_PAGES {
            let before = entries.len();
            let mut conts = vec![];
            walk(&page, &mut entries, &mut conts);
            dedup(&mut entries);
            // stop when there is enough, no next page, or a page that added nothing new
            match conts.pop() {
                Some(token) if entries.len() < n && entries.len() > before => {
                    page = post(ctx, "search", json!({"continuation": token}))?
                }
                _ => break,
            }
        }
        entries.truncate(n);
        Ok(entries)
    })
}

fn alert(v: &Value) -> Option<String> {
    v["alerts"].as_array()?.iter().find_map(|a| {
        let r = a.get("alertRenderer").or_else(|| a.get("alertWithButtonRenderer"))?;
        (r["type"] == "ERROR").then(|| text(&r["text"]))
    })
}

fn dedup(entries: &mut Vec<Entry>) {
    let mut seen = std::collections::HashSet::new();
    entries.retain(|e| seen.insert(e.id.clone()));
}

fn text(v: &Value) -> String {
    if let Some(s) = v["simpleText"]
        .as_str()
        .or_else(|| v["content"].as_str())
        .or_else(|| v.as_str())
    {
        return s.to_string();
    }
    v["runs"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x["text"].as_str()).collect())
        .unwrap_or_default()
}

/// "1:02:03" / "4:05" → seconds
fn hms(s: &str) -> Option<u64> {
    let parts: Vec<u64> = s.trim().split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (2..=3)
        .contains(&parts.len())
        .then(|| parts.iter().fold(0, |acc, p| acc * 60 + p))
}

/// First text that looks like a duration in thumbnail badges (lockupViewModel).
fn badge_duration(v: &Value) -> Option<u64> {
    match v {
        Value::Object(m) => {
            if let Some(b) = m.get("thumbnailBadgeViewModel")
                && let Some(d) = b["text"].as_str().and_then(hms)
            {
                return Some(d);
            }
            m.values().find_map(badge_duration)
        }
        Value::Array(a) => a.iter().find_map(badge_duration),
        _ => None,
    }
}

fn find_token(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(t) = m.get("continuationCommand").and_then(|c| c["token"].as_str()) {
                return Some(t.to_string());
            }
            m.values().find_map(find_token)
        }
        Value::Array(a) => a.iter().find_map(find_token),
        _ => None,
    }
}

/// Ad containers among the results (their contents look like normal videos).
fn is_ad_key(k: &str) -> bool {
    matches!(k, "adSlotRenderer" | "adPlacementRenderer" | "searchPyvRenderer")
        || k.ends_with("AdLayoutRenderer")
        || k.starts_with("promoted")
}

fn walk(v: &Value, out: &mut Vec<Entry>, conts: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match k.as_str() {
                    "playlistVideoRenderer" => {
                        if let (Some(id), true) = (x["videoId"].as_str(), x["isPlayable"] != false) {
                            out.push(Entry {
                                id: id.into(),
                                title: text(&x["title"]),
                                duration: x["lengthSeconds"].as_str().and_then(|s| s.parse().ok()),
                            });
                        }
                    }
                    "videoRenderer" => {
                        if let Some(id) = x["videoId"].as_str() {
                            out.push(Entry {
                                id: id.into(),
                                title: text(&x["title"]),
                                duration: x["lengthText"]["simpleText"].as_str().and_then(hms),
                            });
                        }
                    }
                    "lockupViewModel" => {
                        if x["contentType"] == "LOCKUP_CONTENT_TYPE_VIDEO"
                            && let Some(id) = x["contentId"].as_str()
                        {
                            out.push(Entry {
                                id: id.into(),
                                title: text(&x["metadata"]["lockupMetadataViewModel"]["title"]),
                                duration: badge_duration(&x["contentImage"]),
                            });
                        }
                    }
                    "continuationItemRenderer" => conts.extend(find_token(x)),
                    // ads among the results (e.g. inFeedAdLayoutRenderer with a lockupViewModel inside)
                    k if is_ad_key(k) => {}
                    _ => walk(x, out, conts),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, out, conts)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(hms("4:05"), Some(245));
        assert_eq!(hms("1:02:03"), Some(3723));
        assert_eq!(hms("LIVE"), None);
        assert_eq!(hms("5"), None);
    }

    #[test]
    fn walk_collects_videos_and_continuations_and_skips_ads() {
        let page: Value = serde_json::from_str(include_str!("../tests/fixtures/search-page.json")).unwrap();
        let (mut out, mut conts) = (vec![], vec![]);
        walk(&page, &mut out, &mut conts);
        dedup(&mut out);
        let got: Vec<_> = out
            .iter()
            .map(|e| (e.id.as_str(), e.title.as_str(), e.duration))
            .collect();
        assert_eq!(
            got,
            [
                ("vidA_______", "First video", Some(3723)),
                ("vidB_______", "Lockup video", Some(245)),
                ("vidC_______", "Playlist item", Some(60)),
            ]
        );
        assert_eq!(conts, ["TOKEN-1"]);
    }

    #[test]
    fn ad_keys() {
        for k in [
            "adSlotRenderer",
            "inFeedAdLayoutRenderer",
            "promotedSparklesWebRenderer",
            "searchPyvRenderer",
        ] {
            assert!(is_ad_key(k), "{k}");
        }
        for k in [
            "adaptiveFormats",
            "addToPlaylist",
            "videoRenderer",
            "itemSectionRenderer",
        ] {
            assert!(!is_ad_key(k), "{k}");
        }
    }
}
