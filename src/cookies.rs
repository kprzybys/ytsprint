//! YouTube cookies from Firefox (cookies.sqlite, unencrypted) + SAPISIDHASH headers
//! (yt-dlp: _base.py _get_sid_authorization_header / _generate_cookie_auth_headers).

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Cookie {
    pub name: String,
    pub value: String,
    pub host: String,
    pub path: String,
    pub secure: bool,
}

/// The cookies.sqlite file: $YTSPRINT_FIREFOX_PROFILE, otherwise the most recently modified one
/// in the Firefox directories (the same ones as yt-dlp).
fn cookies_db() -> Result<PathBuf> {
    if let Some(p) = crate::fsutil::env_path("YTSPRINT_FIREFOX_PROFILE") {
        return Ok(p.join("cookies.sqlite"));
    }
    let home = PathBuf::from(std::env::var("HOME")?);
    let config = crate::fsutil::env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    newest_cookies_db(&home, &config)
}

fn newest_cookies_db(home: &Path, config: &Path) -> Result<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for base in [
        config.join("mozilla/firefox"), // new installations since Firefox 147
        home.join(".mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/config/mozilla/firefox"), // Flatpak
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
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

/// YouTube cookies of the default Firefox context.
///
/// The database is copied together with its `-wal` file into a private temporary directory and opened there:
/// opening the live file with `immutable=1` would ignore the WAL, i.e. cookies Firefox wrote recently
/// (a fresh login, rotated session cookies). Copying ~3 MB costs a few ms.
pub fn firefox_youtube() -> Result<(Vec<Cookie>, PathBuf)> {
    let db = cookies_db()?;
    let tmp = crate::fsutil::runtime_dir()?.join(format!("ytsprint-cookies-{}", std::process::id()));
    // left by a killed process with the same PID: its WAL must not be applied to the new copy
    match std::fs::remove_dir_all(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("remove stale {}", tmp.display()));
        }
        _ => {}
    }
    crate::fsutil::private_dir(&tmp)?;
    let result = read_copy(&db, &tmp);
    let _ = std::fs::remove_dir_all(&tmp); // best effort: it is our own private temp dir
    Ok((result?, db))
}

fn read_copy(db: &Path, tmp: &Path) -> Result<Vec<Cookie>> {
    let copy = tmp.join("cookies.sqlite");
    std::fs::copy(db, &copy).with_context(|| format!("copy {}", db.display()))?;
    let wal = db.with_file_name("cookies.sqlite-wal");
    if wal.exists() {
        std::fs::copy(&wal, tmp.join("cookies.sqlite-wal")).with_context(|| format!("copy {}", wal.display()))?;
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    // originAttributes = '': the default context only, not Multi-Account Containers or cookies partitioned
    // under other sites (Total Cookie Protection), which could belong to another account.
    let mut st = conn.prepare(
        "SELECT name, value, host, path, isSecure, expiry FROM moz_cookies
         WHERE (host = 'youtube.com' OR host LIKE '%.youtube.com') AND originAttributes = ''
         ORDER BY lastAccessed DESC",
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
    let mut out = vec![];
    for row in rows {
        let (c, exp) = row.context("cookies.sqlite: unexpected row")?;
        // expiry: seconds in older Firefox, milliseconds in newer versions
        let exp = if exp > 100_000_000_000 { exp / 1000 } else { exp };
        if exp == 0 || exp > now {
            out.push(c);
        }
    }
    Ok(out)
}

/// Short key of the account and profile: part of the session cache name, so switching accounts or
/// profiles does not reuse another account's session data.
pub fn account_key(cookies: &[Cookie], db: &Path) -> String {
    let mut h = Sha1::new();
    h.update(db.to_string_lossy().as_bytes());
    h.update(
        get(cookies, "SAPISID")
            .or_else(|| get(cookies, "__Secure-3PAPISID"))
            .unwrap_or(""),
    );
    h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect()
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
    fn finds_the_newest_profile_in_any_firefox_directory() {
        let home = std::env::temp_dir().join(format!("ytsprint-test-ff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config");
        assert!(newest_cookies_db(&home, &config).is_err());
        let db = |dir: &str, age: u64| {
            let d = home.join(dir).join("x.default");
            std::fs::create_dir_all(&d).unwrap();
            let f = std::fs::File::create(d.join("cookies.sqlite")).unwrap();
            f.set_modified(SystemTime::now() - std::time::Duration::from_secs(age))
                .unwrap();
            d.join("cookies.sqlite")
        };
        let old = db(".mozilla/firefox", 100);
        assert_eq!(newest_cookies_db(&home, &config).unwrap(), old);
        let xdg = db(".config/mozilla/firefox", 10);
        assert_eq!(newest_cookies_db(&home, &config).unwrap(), xdg);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn logged_in_needs_login_info() {
        assert!(!logged_in(&[c("SAPISID", "x")]));
        assert!(logged_in(&[c("SAPISID", "x"), c("LOGIN_INFO", "y")]));
        assert!(sid_authorization_at(&[], "o", None, "1").is_none());
    }
}
