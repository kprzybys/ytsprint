# ytsprint

Fast YouTube audio for [mpv](https://mpv.io): a drop-in replacement for yt-dlp in mpv's `ytdl_hook`
for single videos, playlists and search, using your YouTube Premium session from Firefox.

| | time to stream URL |
|---|---|
| yt-dlp (with cookies) | ~5.5 s |
| ytsprint, empty cache, new YouTube player or new yt-dlp | ~3–5 s |
| ytsprint, first run of the hour | ~1–2 s |
| ytsprint, warm | **0.2–0.6 s** (almost all of it is the YouTube API round trip) |

Measured on a laptop with a Premium account in November 2026; your network decides most of the remaining time.
Without Premium, add the pre-roll ad (see [Requirements](#requirements)).

## What it does — and does not

ytsprint handles itself:
- a single video → best audio-only format (JSON in the yt-dlp schema that mpv's `ytdl_hook` reads),
- playlists, including *Liked videos* (`list=LL`) and private playlists (unavailable entries are skipped;
  up to 5000 entries, a longer list is cut with a `WARN` in the log),
- search (`ytsprint search`).

Deliberately **routed to yt-dlp** (same arguments): live streams, other sites, any yt-dlp option it does not know
(also `--help` and `--version`). It returns no subtitles, video, chapters or comments.

**No hidden fallbacks.** When ytsprint fails on something it does handle, it does not quietly retry
or hand over to yt-dlp: it exits with an error that mpv shows and writes `ERROR` to its log.
You find out immediately when YouTube changes something.

## Requirements

- **Linux** (Unix socket, systemd user service, `XDG_RUNTIME_DIR` set — as in any normal login session).
  Not tested elsewhere.
- **mpv** (tested with 0.37 and 0.40).
- **yt-dlp with the ejs solver**: the [onedir release](https://github.com/yt-dlp/yt-dlp/releases)
  or `pip install "yt-dlp[default]"`. It is always needed: its JS challenge solver, player variant and client definitions are used for every video,
  playlist and search, and live streams are routed to it. Old distribution packages without `yt-dlp-ejs` are skipped; ytsprint uses the first `yt-dlp` in `PATH` that has it.
- **[deno](https://deno.com)** in `PATH` (runs the solver; the runtime yt-dlp recommends).
- **Firefox** (native, Flatpak or Snap) logged in to YouTube, default container only. Other browsers are not read.
  Without an account, leave `cookies-from-browser` out of the mpv profile.
- **YouTube Premium is strongly recommended**: without it YouTube blocks the stream (HTTP 403) during a pre-roll ad.
  ytsprint waits for the ad's full length, typically 6–7 s, so it works, just slower.
- Rust ≥ 1.89 to build; deno also for `cargo test` (the service is tested with a real deno).

## Install

```sh
cargo install --locked --git https://github.com/kprzybys/ytsprint
ytsprint install-service     # writes and enables ~/.config/systemd/user/ytsprint-jsd.service
```

`install-service` records the absolute paths of `ytsprint` and `deno` (systemd does not see your shell `PATH`),
and restarts the service. Run it again after moving either of them and after upgrading ytsprint.

mpv must find `yt-dlp` in its own `PATH`; when you start mpv from a desktop launcher, that may lack
`~/.local/bin` — set `YTSPRINT_YTDLP` then.

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
ytsprint install-service                     # write, enable and restart the systemd user service
ytsprint jsd                                 # what the service runs
```

Search runs without your account by default: with it, YouTube personalises the results so much that
the same query returns a different list every time. Add `--cookies` if you want that.

## How it works

1. Reads the YouTube cookies from Firefox's `cookies.sqlite` (~10 ms) and signs requests (SAPISIDHASH).
2. Session data (visitor data, current player ID, account IDs, Premium or not) from a YouTube page, cached for 1 h.
3. `/youtubei/v1/player` as the `web_creator` client for Premium accounts, `tv_samsung` otherwise — neither needs a
   PO token in that case (client definitions are read from your yt-dlp).
4. The `n`/`sig` challenges are solved by **ytsprint-jsd**: a resident deno process running
   [yt-dlp's ejs solver](https://github.com/yt-dlp/ejs) with the YouTube player kept in memory (1–3 ms per video
   instead of ~0.45 s). A new player (every few days) or a new ejs version is prepared once.
   **Sandboxed:** deno may access its socket and nothing else — the YouTube player code it runs cannot read or
   write any file or use the network. ytsprint sends it the solver scripts and the player over the socket when it asks for them.
5. Where the client returns no title (`tv_samsung`), it comes from oEmbed, requested in parallel.

Details: [docs/architecture.md](docs/architecture.md). Background, rejected alternatives and measurements:
[docs/design.md](docs/design.md).

## Updates

- **YouTube player changes**: automatic (prepared once, ~1–3 s; of the older ones only the previous player is kept).
- **yt-dlp / ejs updates**: automatic — a new ejs is reloaded and the new yt-dlp is asked once (~0.6 s, offline) for its
  player JS variant and client definitions.
- **deno upgrade**: `systemctl --user restart ytsprint-jsd`.
- **ytsprint upgrade**: `cargo install ...` again, then `ytsprint install-service` (the unit may have changed too).
- **YouTube rejects a client** (`web_creator` / `tv_samsung` / `web` version): you get an error; the fix
  arrives with a yt-dlp update, because the definitions are read from your yt-dlp.
- **yt-dlp from a git checkout** (editable install): ytsprint does not notice `git pull`; delete
  `~/.cache/ytsprint/ytdlp-settings.json` afterwards.

So keep yt-dlp up to date: when YouTube breaks something, ytsprint fails until a yt-dlp release fixes it.

## Troubleshooting

Look at `~/.local/state/ytsprint/log` and `systemctl --user status ytsprint-jsd`.

| Message or symptom | What to do |
|---|---|
| `ytsprint-jsd is not running` | `ytsprint install-service`, or `systemctl --user start ytsprint-jsd` |
| `ytsprint-jsd was stuck and has been restarted` | play again; that one play failed, the service is fresh |
| `ytsprint-jsd:` + a JavaScript error, `n challenge: …` | YouTube's new player is not supported by the solver yet: update yt-dlp |
| `yt-dlp not found in PATH`, `no yt-dlp with the ejs solver` | install a current yt-dlp (see Requirements) or set `YTSPRINT_YTDLP` |
| `cannot read its settings (incompatible yt-dlp?)` | yt-dlp is too old, or changed internally: update yt-dlp, then ytsprint |
| `cookies.sqlite not found`, `no YouTube login in the Firefox cookies`, `LOGGED_IN=false` | log in to YouTube in Firefox; several profiles: set `YTSPRINT_FIREFOX_PROFILE` |
| `cannot tell whether the account has Premium` | YouTube changed its page: an update of ytsprint is needed |
| `waiting N s for the pre-roll ad` | no Premium in the cookies, or no `cookies-from-browser` in the mpv profile |
| slow start, `yt-dlp (arguments not handled)` in the log | mpv passed an option ytsprint does not know, e.g. another `ytdl-format` or a subtitle language |
| `playabilityStatus …` | YouTube's own reason (private, removed, region) |

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
| `YTSPRINT_DUMP=<dir>` | save raw API responses in an existing directory (debugging; private account data) |

An empty value counts as unset. `XDG_CACHE_HOME`, `XDG_STATE_HOME` and `XDG_CONFIG_HOME` are respected;
`XDG_RUNTIME_DIR` is required.

Files: cache in `~/.cache/ytsprint/`, log (timings, `ERROR`, `WARN`, routing to yt-dlp) in
`~/.local/state/ytsprint/log` (rotated to `log.1` at 1 MiB). Both directories are 0700 and the files 0600: the cache
holds account session data, and the log lists what you played. All files: [docs/architecture.md](docs/architecture.md#files).

## Uninstall

```sh
systemctl --user disable --now ytsprint-jsd
rm ~/.config/systemd/user/ytsprint-jsd.service && systemctl --user daemon-reload
cargo uninstall ytsprint
rm -r ~/.cache/ytsprint ~/.local/state/ytsprint
```

Then remove the `[yt]` profile from `mpv.conf`.

## Development

`cargo test` needs deno (it runs the service with a stub solver) but no YouTube account and no network.
CI runs fmt, clippy and the tests on stable Rust and `cargo check` on the oldest supported one (1.89).
To try a build in mpv without installing it:
`mpv --profile=yt --script-opts-append=ytdl_hook-ytdl_path=$PWD/target/release/ytsprint URL`.

## Disclaimer

Not affiliated with YouTube or Google. Meant for personal playback of content you have access to.
Your cookies are read locally and ytsprint sends them only to `www.youtube.com`; for routed requests yt-dlp reads
them itself. YouTube's terms may restrict
third-party clients; using this is your decision.

## Credits

The InnerTube client definitions, the SAPISIDHASH scheme and the pre-roll ad handling follow
[yt-dlp](https://github.com/yt-dlp/yt-dlp); the JS challenges are solved by yt-dlp's
[ejs](https://github.com/yt-dlp/ejs). Both are released under the Unlicense, like this project.

## License

[Unlicense](LICENSE) — public domain.
