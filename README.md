# ytsprint

Fast YouTube audio for [mpv](https://mpv.io): a drop-in replacement for yt-dlp in mpv's `ytdl_hook`
for single videos, playlists and search, using your YouTube Premium session from Firefox.

| | time to stream URL |
|---|---|
| yt-dlp (with cookies) | ~5.5 s |
| ytsprint, first run of the hour | ~1–3 s |
| ytsprint, warm | **0.25–0.6 s** (almost all of it is the YouTube API round trip) |

Measured on a laptop in November 2026; your network decides most of the remaining time.

## What it does — and does not

ytsprint handles itself:
- a single video → best audio-only format (JSON in the yt-dlp schema that mpv's `ytdl_hook` reads),
- playlists, including *Liked videos* (`list=LL`) and private playlists (unavailable entries are skipped),
- search (`ytsprint search`).

Deliberately **routed to yt-dlp** (same arguments): live streams, other sites, any yt-dlp option it does not know.

**No hidden fallbacks.** When ytsprint fails on something it does handle, it does not quietly retry
or hand over to yt-dlp: it exits with an error that mpv shows and writes `ERROR` to its log.
You find out immediately when YouTube changes something.

## Requirements

- **Linux** (Unix socket, systemd user service, `XDG_RUNTIME_DIR` set — as in any normal login session).
  Not tested elsewhere.
- **mpv** (tested with 0.37).
- **yt-dlp with the ejs solver**: the [onedir release](https://github.com/yt-dlp/yt-dlp/releases)
  or `pip install "yt-dlp[default]"`. ytsprint uses yt-dlp's JS challenge solver and routes live streams to yt-dlp.
  Old distribution packages without `yt-dlp-ejs` are skipped; ytsprint uses the first `yt-dlp` in `PATH` that has it.
- **[deno](https://deno.com)** in `PATH` (runs the solver; the runtime yt-dlp recommends).
- **Firefox** logged in to YouTube. **YouTube Premium is strongly recommended**: without it YouTube makes
  the stream wait for a pre-roll ad (HTTP 403 for ~5 s); ytsprint waits that time out, so it works, just slower.
- Rust ≥ 1.88 to build; deno also for `cargo test` (the service is tested with a real deno).

## Install

```sh
cargo install --git https://github.com/kprzybys/ytsprint
ytsprint install-service     # writes and enables ~/.config/systemd/user/ytsprint-jsd.service
```

`install-service` records the absolute paths of `ytsprint` and `deno` (systemd does not see your shell `PATH`).
Run it again after moving either of them.

mpv profile (`~/.config/mpv/mpv.conf`, profiles at the end of the file):

```ini
[yt]
profile-desc=YouTube audio via ytsprint
video=no
ytdl-format=bestaudio/best
script-opts-append=ytdl_hook-try_ytdl_first=yes
script-opts-append=ytdl_hook-ytdl_path=/home/<you>/.cargo/bin/ytsprint
ytdl-raw-options-append=cookies-from-browser=firefox
```

## Usage

```sh
mpv --profile=yt 'https://www.youtube.com/watch?v=...'
mpv --profile=yt --shuffle 'https://www.youtube.com/playlist?list=LL'   # liked videos

ytsprint search 15 'dark ambient'            # id<TAB>seconds<TAB>title, e.g. for fzf
ytsprint --url <id|URL> --cookies            # debugging: stream URL on stdout, timings on stderr
```

Search runs without your account by default: with it, YouTube personalises the results so much that
the same query returns a different list every time. Add `--cookies` if you want that.

## How it works

1. Reads the YouTube cookies from Firefox's `cookies.sqlite` (~10 ms) and signs requests (SAPISIDHASH).
2. Session data (visitor data, current player ID, account IDs) from a YouTube page, cached for 1 h.
3. `/youtubei/v1/player` as the `web_creator` client for Premium accounts, `tv_samsung` otherwise — neither needs a
   PO token in that case (client definitions taken from yt-dlp).
4. The `n`/`sig` challenges are solved by **ytsprint-jsd**: a resident deno process running
   [yt-dlp's ejs solver](https://github.com/yt-dlp/ejs) with the YouTube player kept in memory (1–3 ms per video
   instead of ~0.45 s). A new player (every few days) or a new ejs version is prepared once.
   **Sandboxed:** deno may access its socket and nothing else — the YouTube player code it runs cannot read or
   write any file. ytsprint sends it the solver scripts and the player over the socket when it asks for them.
5. Where the client returns no title (`tv_samsung`), it comes from oEmbed, requested in parallel.

Details: [docs/architecture.md](docs/architecture.md). Background, rejected alternatives and measurements:
[docs/design.md](docs/design.md).

## Updates

- **YouTube player changes**: automatic (prepared once, ~1.2 s, old files deleted).
- **yt-dlp / ejs updates**: automatic — a new ejs is reloaded and the new yt-dlp is asked once (~0.6 s, offline) which
  player JS variant it uses.
- **deno upgrade**: `systemctl --user restart ytsprint-jsd`.
- **ytsprint upgrade**: `cargo install ...` again, then `systemctl --user restart ytsprint-jsd`.
- **YouTube rejects a client** (`web_creator` / `tv_samsung` / `web` version): you get an error; the fix is new values from
  yt-dlp's `yt_dlp/extractor/youtube/_base.py`.

## Environment variables

| Variable | Default |
|---|---|
| `YTSPRINT_YTDLP` | first `yt-dlp` in `PATH` that ships ejs |
| `YTSPRINT_EJS_DIR` | the ejs solver of that yt-dlp |
| `YTSPRINT_DENO` | `deno` in `PATH` |
| `YTSPRINT_FIREFOX_PROFILE` | the profile with the most recently modified `cookies.sqlite` |
| `YTSPRINT_JSD_SOCKET` | `$XDG_RUNTIME_DIR/ytsprint-jsd.sock` |
| `YTSPRINT_FORCE_YTDLP=1` | always use yt-dlp (for comparisons) |
| `YTSPRINT_NO_WAIT=1` | `--url`: do not wait for the pre-roll ad |
| `YTSPRINT_DUMP=<dir>` | save raw API responses (debugging) |

Files: cache in `~/.cache/ytsprint/`, log (timings, `ERROR`, `WARN`, routing to yt-dlp) in
`~/.local/state/ytsprint/log` (rotated at 1 MiB). Both directories are 0700 and the files 0600: the cache holds
account session data.

## Disclaimer

Not affiliated with YouTube or Google. Meant for personal playback of content you have access to.
Your cookies are read locally and sent only to `www.youtube.com`. YouTube's terms may restrict
third-party clients; using this is your decision.

## Credits

The InnerTube client definitions, the SAPISIDHASH scheme and the pre-roll ad handling follow
[yt-dlp](https://github.com/yt-dlp/yt-dlp); the JS challenges are solved by yt-dlp's
[ejs](https://github.com/yt-dlp/ejs). Both are released under the Unlicense, like this project.

## License

[Unlicense](LICENSE) — public domain.
