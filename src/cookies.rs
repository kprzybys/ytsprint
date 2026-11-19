//! YouTube cookies from Firefox (cookies.sqlite, unencrypted) + SAPISIDHASH headers
//! (yt-dlp: _base.py _get_sid_authorization_header / _generate_cookie_auth_headers).

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha1::{Digest, Sha1};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Cookie {
    pub name: String,
    pub value: String,
    pub host: String,
    pub path: String,
    pub secure: bool,
}

/// The cookies.sqlite file: $YTSPRINT_FIREFOX_PROFILE, otherwise the most recently modified one
/// under ~/.mozilla/firefox (yt-dlp does the same).
fn cookies_db() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("YTSPRINT_FIREFOX_PROFILE") {
        return Ok(PathBuf::from(p).join("cookies.sqlite"));
    }
    let home = std::env::var("HOME")?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for base in [
        format!("{home}/.mozilla/firefox"),
        format!("{home}/snap/firefox/common/.mozilla/firefox"),
    ] {
        let Ok(dir) = std::fs::read_dir(base) else { continue };
        for e in dir.flatten() {
            let db = e.path().join("cookies.sqlite");
            if let Ok(m) = db.metadata().and_then(|m| m.modified())
                && best.as_ref().is_none_or(|(t, _)| m > *t)
            {
                best = Some((m, db));
            }
        }
    }
    best.map(|(_, p)| p)
        .context("cookies.sqlite not found in the Firefox profile")
}

pub fn firefox_youtube() -> Result<Vec<Cookie>> {
    let db = cookies_db()?;
    // immutable=1: read without locks while Firefox is running and holding the database.
    let uri = format!("file:{}?immutable=1", db.display());
    let conn = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut st = conn.prepare(
        "SELECT name, value, host, path, isSecure, expiry FROM moz_cookies
         WHERE host = 'youtube.com' OR host LIKE '%.youtube.com'",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            Cookie {
                name: r.get(0)?,
                value: r.get(1)?,
                host: r.get(2)?,
                path: r.get(3)?,
                secure: r.get::<_, i64>(4)? != 0,
            },
            r.get::<_, i64>(5)?,
        ))
    })?;
    // expiry: seconds in older Firefox, milliseconds in newer versions
    Ok(rows
        .flatten()
        .filter(|(_, exp)| {
            let exp = if *exp > 100_000_000_000 { exp / 1000 } else { *exp };
            exp == 0 || exp > now
        })
        .map(|(c, _)| c)
        .collect())
}

pub fn get<'a>(cookies: &'a [Cookie], name: &str) -> Option<&'a str> {
    cookies.iter().find(|c| c.name == name).map(|c| c.value.as_str())
}

pub fn logged_in(cookies: &[Cookie]) -> bool {
    get(cookies, "LOGIN_INFO").is_some()
        && ["SAPISID", "__Secure-3PAPISID", "__Secure-1PAPISID"]
            .iter()
            .any(|n| get(cookies, n).is_some())
}

/// `Authorization` header: SAPISIDHASH / SAPISID1PHASH / SAPISID3PHASH.
pub fn sid_authorization(cookies: &[Cookie], origin: &str, user_session_id: Option<&str>) -> Option<String> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs().to_string();
    sid_authorization_at(cookies, origin, user_session_id, &ts)
}

fn sid_authorization_at(cookies: &[Cookie], origin: &str, user_session_id: Option<&str>, ts: &str) -> Option<String> {
    let sapisid = get(cookies, "SAPISID").or_else(|| get(cookies, "__Secure-3PAPISID"));
    let p1 = get(cookies, "__Secure-1PAPISID");
    let p3 = get(cookies, "__Secure-3PAPISID");
    let ts = ts.to_string();
    let parts: Vec<String> = [("SAPISIDHASH", sapisid), ("SAPISID1PHASH", p1), ("SAPISID3PHASH", p3)]
        .into_iter()
        .filter_map(|(scheme, sid)| {
            let sid = sid?;
            let mut hashed = vec![];
            if let Some(u) = user_session_id {
                hashed.push(u.to_string());
            }
            hashed.extend([ts.clone(), sid.to_string(), origin.to_string()]);
            let digest = Sha1::digest(hashed.join(" ").as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            let mut v = vec![ts.clone(), hex];
            if user_session_id.is_some() {
                // yt-dlp: ''.join(additional_parts) joins the dict's KEYS, i.e. the literal "u"
                v.push("u".to_string());
            }
            Some(format!("{scheme} {}", v.join("_")))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, value: &str) -> Cookie {
        Cookie {
            name: name.into(),
            value: value.into(),
            host: ".youtube.com".into(),
            path: "/".into(),
            secure: true,
        }
    }

    // Expected values computed with yt-dlp's algorithm (_base.py _make_sid_authorization) in Python.
    #[test]
    fn sapisidhash_matches_yt_dlp() {
        let jar = [
            c("SAPISID", "abc"),
            c("__Secure-1PAPISID", "one"),
            c("__Secure-3PAPISID", "three"),
        ];
        let got = sid_authorization_at(&jar, "https://www.youtube.com", None, "1700000000").unwrap();
        assert_eq!(got, include_str!("../tests/fixtures/sapisidhash-plain.txt").trim());
        let got = sid_authorization_at(&jar, "https://www.youtube.com", Some("12345"), "1700000000").unwrap();
        assert_eq!(got, include_str!("../tests/fixtures/sapisidhash-user.txt").trim());
    }

    #[test]
    fn falls_back_to_3papisid_when_sapisid_missing() {
        let jar = [c("__Secure-3PAPISID", "three")];
        let got = sid_authorization_at(&jar, "https://www.youtube.com", None, "1").unwrap();
        assert!(got.starts_with("SAPISIDHASH 1_") && got.contains(" SAPISID3PHASH 1_"));
    }

    #[test]
    fn logged_in_needs_login_info() {
        assert!(!logged_in(&[c("SAPISID", "x")]));
        assert!(logged_in(&[c("SAPISID", "x"), c("LOGIN_INFO", "y")]));
        assert!(sid_authorization_at(&[], "o", None, "1").is_none());
    }
}
