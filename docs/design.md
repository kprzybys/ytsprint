# Design notes

Why ytsprint exists, what was tried first, and what the measurements showed (November 2026).

## Goal

Start YouTube audio in mpv as fast as possible. With yt-dlp as mpv's `ytdl_path`, getting to sound took
~5.5 s with cookies (Python start, cookie extraction ~1.6 s, several API clients, the JS challenge in node/deno).
ytsprint does **only** what that use case needs from YouTube and leaves everything else to yt-dlp.

Out of scope: downloading, merging formats, subtitles, comments, chapters, channels, other sites, live streams,
PO tokens, SABR.

## Findings

- **yt-dlp needs no PO token for this.** Its default `tv_samsung` client (`TVHTML5`) returns stream URLs that work
  once the `n`/`sig` JS challenges from the player's `base.js` are solved. Clients such as `web`, `ios` and
  `android_vr` need a PO token.
- **The pre-roll ad.** Without Premium, the stream returns 403 for ~5 s after the player request, until the ad
  "has played" (yt-dlp exposes it as `available_at`). mpv's `ytdl_hook` ignores `available_at`, so with plain
  yt-dlp mpv sometimes fails with `EDL: Could not open source file`. With Premium cookies there is no ad
  in `web_creator`; `tv_samsung` still had one on 6 of 12 tested videos (403 for 3–4 s, always the same videos).
- **Player variant `tv`.** Since player b485e332 (November 2026) ejs 0.8.0 fails on the `main` variant
  (`addOnDisposeCallback is not a function`, yt-dlp/ejs#79); yt-dlp 2026.11.23 switched to `tv`.
  ytsprint asks the installed yt-dlp which variant it uses, so such a switch arrives with the yt-dlp update.
- Stream URLs from `tv_samsung` are fetched with a desktop Chrome User-Agent (what yt-dlp puts in `http_headers`).
- The `tv_samsung` player response has no title → oEmbed.
- SAPISIDHASH with a user session ID ends in the literal `u`: yt-dlp builds it with `''.join(dict)`, which joins
  the dict's keys.
- Search with account cookies returns a different list on every request (5–6 of 15 results in common between two
  identical queries); without the account it is stable (15/15).

## Rejected alternatives

**Existing Rust extractors** (tested November 2026; the URL is requested from the middle of the file, because
YouTube serves the first ~1 MB even to clients it is about to block):

| Project | Result |
|---|---|
| rustypipe 0.11.4 (last release 2025-04) | deobfuscation fails on the current player; `ios` URLs 403 even with botguard PO tokens |
| innertube-rs 0.12 | 403 on 3 of 8 videos; the rest only the first ~1 MB (`ANDROID_VR`, no PO token) |
| tydle 0.1.15 | URL for 8 of 8 videos, but only the first ~1 MB plays |

Every Rust music player found (goosemusic, ytmusic-tui, ytmusic-cli, …) streams through yt-dlp.

**Writing our own challenge solver**: the extracted `n`/`sig` functions are obfuscated JavaScript that changes
with every player, so a JS engine is needed anyway, and the yt-dlp team ships solver fixes ~10 times a year.
Reusing [ejs](https://github.com/yt-dlp/ejs) keeps that maintenance upstream.

**Embedded QuickJS instead of deno** (`rquickjs`), same challenge on the same prepared player:
QuickJS 0.46–0.51 s, deno 0.42–0.46 s. Almost all of it is loading the 4 MB player, not starting the process.
deno is also the runtime yt-dlp recommends and tests most, so ejs problems show up and get fixed there first.

**What actually helped:** keeping the prepared player loaded in a resident deno process (`ytsprint-jsd`):
the challenge drops from ~0.45 s to 1–3 ms.

## Measurements

| | yt-dlp | ytsprint |
|---|---|---|
| single video, to stream URL (Premium) | ~5.5 s | 0.25–0.6 s warm, ~1–3 s first of the hour |
| liked videos (list) | 1.7 s | 0.4 s |
| search, 15 results | 1.8 s | 0.6 s |
| cookie extraction | ~1.6 s | ~10 ms |

ytsprint-jsd uses ~110 MB RSS (~300 MB briefly while preparing a new player).

## Security

The YouTube player is remote JavaScript. ytsprint-jsd runs it in deno with access to its own socket only
(no files, no network, no subprocesses); the solver scripts and the player arrive over the socket. Requests are
validated (`player_key`/`ejs_tag` are hex), the socket lives in `$XDG_RUNTIME_DIR` (0700, required — no `/tmp`
fallback), and the cache and log are 0600 in 0700 directories. Cookies are read from a private copy of the
Firefox database and sent only to `www.youtube.com`.

## Error policy

Errors on the handled path are reported, never masked: no fallback to yt-dlp, no automatic retry, no second JS
runtime. After an error the cached session is dropped so the next run starts fresh. Routing cases ytsprint does
not implement (live streams, other sites) to yt-dlp is deliberate and logged as such, not as an error.

## Upstream changes to watch

Rate of change in yt-dlp over the year before November 2026: 73 commits in `yt_dlp/extractor/youtube`,
21 in `_base.py` (client definitions); ejs 0.1 → 0.8 in 11 releases. The changes relevant here are client
definitions (`web_creator`, `tv_samsung`, `web`), the names the probe reads (`_DEFAULT_PLAYER_JS_VARIANT`,
`_PLAYER_JS_VARIANT_MAP`) and the ejs internals that `jsd.js` relies on.

## Possible next steps

- Keep a warm HTTP/2 connection to YouTube in a resident process (the API round trip is now most of the time).
- A small set of real videos checked periodically (with a range request from the middle of the file).
- Live streams (HLS manifest) — currently routed to yt-dlp.
