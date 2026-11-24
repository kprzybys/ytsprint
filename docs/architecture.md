# Architecture

```
 mpv --profile=yt URL
   │  ytdl_hook (built into mpv) runs ytdl_path with yt-dlp arguments
   │  (optionally through your own wrapper, e.g. one that prefetches while you browse a list)
   ▼
 ytsprint (Rust) ─────────────────────► YouTube InnerTube API
   │  ├─ main.rs     yt-dlp-compatible CLI, JSON for mpv, `search`, `jsd`, `install-service`
   │  ├─ cookies.rs  Firefox cookies, SAPISIDHASH
   │  ├─ extract.rs  session, player API (web_creator/tv_samsung), format selection, pre-roll wait, caches
   │  ├─ browse.rs   playlists and search (web client), result parser
   │  ├─ locate.rs   finds yt-dlp (with ejs) and deno without hard-coded paths
   │  └─ jsc.rs      Unix socket client ──────┐
   │                                          ▼
   │                     ytsprint-jsd (deno, systemd user service, src/jsd.js)
   │                       yt-dlp's ejs solver + YouTube player kept in memory;
   │                       sandboxed: access to its own socket only
   │
   ├─ JSON with the stream URL ──► mpv plays from googlevideo.com
   └─ live streams / other sites / unknown options ──► exec yt-dlp (same arguments)
```

## One play, step by step (~0.25–0.6 s)

| Step | Time | Where |
|---|---|---|
| cookies: `cookies.sqlite` + its `-wal` copied to a private temp dir and read (default container only) | ~10–30 ms | `cookies.rs` |
| session (visitor data, player ID, account IDs) | 0 ms cached, ~1 s once an hour | `extract.rs` |
| `/youtubei/v1/player` as `web_creator` (Premium) or `tv_samsung` | 0.1–0.5 s | `extract.rs` |
| title from oEmbed, in parallel | 0 ms (background) | `extract.rs` |
| `n` / `sig` challenge | 1–3 ms | `ytsprint-jsd` |
| JSON for mpv | – | `main.rs` |

Playlists (`/browse VL<id>`, further pages via continuation tokens) and search (`/search`) use the `web` client.
mpv receives the list of `entries` and opens each video separately through the path above.

## Key decisions

1. **`web_creator` client for Premium accounts, `tv_samsung` for the rest.** A pre-roll ad blocks the stream with
   HTTP 403 until it "has played" (`available_at`). mpv ignores `available_at`, which shows up as
   `EDL: Could not open source file` with plain yt-dlp. With `web_creator` and Premium there is no ad, and Premium
   lifts its PO token requirement. `tv_samsung` needs no PO token at all, but has ads without Premium, and on some
   videos even with it. Premium is read from the logo on the session page; a page without it is an error.
2. **yt-dlp's ejs solver.** The hardest and most fragile part is maintained by the yt-dlp team and arrives with
   their updates.
3. **Player kept in memory (`ytsprint-jsd`), sandboxed.** Saves ~0.45 s per play (deno start + ~300 ms loading
   the 4 MB player). deno gets `--allow-net/read/write` for its socket path only, so the remote player code has
   no file access. Protocol: the client sends a small request (`player_key`, `ejs_tag`, challenges); jsd answers
   `need: ejs/player` when it lacks them (after a restart, a new player or new ejs) and the client resends with
   the scripts / the prepared player / the raw player. A raw player is prepared once and returned, and the client
   writes it to the cache atomically. A damaged cached player is reported, deleted, and prepared on the next play.
   jsd relies on an ejs internal (`Function("_result", prepared)` sets `_result.n/sig`); if that changes, you get
   an error, not a silent failure. Socket timeouts on both sides (15 s), so a stuck jsd is an error, not a hang.
4. **Client definitions copied from yt-dlp** (`web_creator` and `tv_samsung` in `extract.rs`, `web` in `browse.rs`): when YouTube
   changes something, it is clear where the new values come from.
5. **No hidden fallbacks or retries.** Errors are shown in mpv and logged as `ERROR`; after a session-related
   error (HTTP 4xx, logged out) the cached session is dropped so the next run starts fresh. Only what ytsprint
   does not do at all goes to yt-dlp. Non-fatal oddities (no title from oEmbed, a truncated playlist) are logged as `WARN`.
6. **Layout-independent result parser.** It looks for renderer types (`playlistVideoRenderer`, `videoRenderer`,
   `lockupViewModel`, `continuationItemRenderer`) anywhere in the response and skips ads.
7. **Search without the account by default**: with it, results changed on every query (personalisation).
8. **Title via oEmbed**: the `tv_samsung` client returns none (`web_creator` does); the request runs in parallel (2 s limit).
9. **No hard-coded paths**: yt-dlp, ejs and deno are found via `PATH` (`locate.rs`); the systemd unit is
   generated with absolute paths by `install-service`.
10. **Audio track**: the original/default track is preferred over auto-dubs and DRC variants, then the bitrate.
11. **Pre-roll wait** (no Premium): announced on stderr and capped at 60 s; longer is an error.

## Caches and updates

| What changes | What happens |
|---|---|
| YouTube player (every few days) | new ID in the session → base.js downloaded → jsd prepares it once (~1.2 s) → older `player-*` files deleted |
| yt-dlp / ejs | the ejs tag (size + mtime of its files) is part of `player-<id>-ejs<tag>.pre.js` and of jsd's state → jsd asks for the new scripts and the player is prepared again; the yt-dlp location cache is invalidated when the binary points elsewhere |
| session | valid for 1 h, per account and Firefox profile; dropped after a session-related error |
| deno | `systemctl --user restart ytsprint-jsd` |
| ytsprint | `cargo install …` + `systemctl --user restart ytsprint-jsd` |
| YouTube rejects a client | error in mpv and `ERROR` in the log → new values from yt-dlp's `_base.py` |

## Files

| Path | Contents |
|---|---|
| `~/.cache/ytsprint/` (0700, files 0600) | `session-<account>.json` (1 h), `player-<id>.sts`, `player-<id>-ejs<tag>.pre.js`, `ytdlp-location.json` |
| `~/.local/state/ytsprint/log` (0600, rotated at 1 MiB) | timings per step, `ERROR`, `WARN`, routing to yt-dlp |
| `$XDG_RUNTIME_DIR/ytsprint-jsd.sock` | service socket |
| `$XDG_RUNTIME_DIR/ytsprint-jsd.js` | the service script (embedded in the binary, written on start) |
| `~/.config/systemd/user/ytsprint-jsd.service` | generated by `ytsprint install-service` |
