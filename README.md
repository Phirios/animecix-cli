# animecix-cli

A standalone Rust CLI for searching Animecix, choosing an anime, season, episode,
video host and quality, and watching in your local video player or downloading
an episode. It contacts Animecix and the video hosts directly. Your internet connection and the upstream
sites must be available.

## Install

Requires Rust 1.88+ and a network-capable video player such as VLC or IINA.

```sh
git clone https://github.com/Phirios/animecix-cli.git
cd animecix-cli
cargo install --path . --locked
animecix "one piece"
```

The installed binary is `~/.cargo/bin/animecix`. Make sure `~/.cargo/bin` is in
your PATH. You can also run directly from this repository:

```sh
cargo run -- "one piece"
```

Use the arrow keys to select; type in the anime and episode menus to filter;
press Enter to confirm. Esc or Left Arrow goes back to the previous selection.
Auto host selection tries the supported providers until a working stream is
found.

## Examples

```sh
# Search without playing; output contains IDs for the commands below
animecix --search "naruto"
animecix --search "naruto" --json

# Skip title search and season/episode prompts
animecix --id 7293 --season 4 --episode 1

# Use VLC explicitly on macOS
animecix "naruto" --player /Applications/VLC.app

# Select a provider and quality without the corresponding menus
animecix --id 7293 -s 4 -e 1 --provider tau-video --quality 720p

# Highest available quality, avoiding the quality menu
animecix --id 7293 -s 4 -e 1 --quality best

# Resolve a remote URL without launching a player
animecix --id 7293 -s 4 -e 1 --url

# Download the selected episode without opening a player
animecix --id 7293 -s 4 -e 1 --quality best --download episode.mp4

# Explicit executable on Linux (also works for mpv on macOS)
animecix "naruto" --player vlc
```

For scripts without a terminal, supply `--id`, `--season`, and `--episode`.
The CLI reports ambiguous choices instead of silently selecting a different anime.
Progress goes to stderr; search results and `--url` output go to stdout.

## Downloads

Use `--download <FILE>` (or `-d <FILE>`) after selecting an episode:

```sh
animecix "one piece" --download episode.mp4
animecix --id 7293 -s 4 -e 1 --quality 720p -d ~/Downloads/episode.mp4
```

The destination directory must exist. Existing files are never overwritten.
Downloads go to a temporary file beside the destination and appear under the
requested name only when complete. Ctrl+C cancels and removes the temporary
file; interrupted downloads are not resumed.

Direct video files download without extra software and preserve the original
bytes (changing the filename extension does not convert their format). HLS
streams require `ffmpeg` in PATH and an `.mp4` or `.mkv` destination. FFmpeg
copies the audio and video streams without re-encoding, using the local relay
for headers, cookies, playlist rewriting, and destination checks. Use `.mkv` if
the source codecs cannot be stored in MP4. Download progress goes to stderr.
`--download` cannot be combined with `--url`, `--search`, or `--player`.

## Shell completion

Generate completions for Zsh, Bash, Fish, PowerShell, or Elvish with
`animecix --completions <shell>`. Completion generation works offline.
Tab completes flags, provider names, shell names, and paths for `--player`.
In Zsh, press Tab after `animecix ` to list options; anime queries do not fall
back to local filenames.

For the current Zsh session:

```sh
source <(animecix --completions zsh)
```

For future Oh My Zsh sessions:

```sh
mkdir -p ~/.oh-my-zsh/custom/completions
animecix --completions zsh > ~/.oh-my-zsh/custom/completions/_animecix
exec zsh
```

Fractional episode numbers are supported in both the episode menu and flags:

```sh
animecix --id 25 -s 1 -e 13.5
```

Episode titles and metadata are loaded from TMDB when the Animecix title has a
TMDB ID and one of these optional credentials is set. Season and episode menus
load metadata on demand for the selected season, caching it while you navigate.
Availability dots appear once that season has been loaded. Press `?` in menus
to view available series, season, or episode metadata.

```sh
export TMDB_API_KEY="your-v3-api-key"
# or: export TMDB_ACCESS_TOKEN="your-api-read-access-token"
```

If neither credential is set, the episode menu still works and shows episode
numbers without titles.

## Default video player

On **macOS**, the CLI queries the default MP4 **viewer** app, then opens a
temporary M3U playlist in that app. QuickTime receives the local stream URL
directly, since it does not support M3U files. To set VLC or IINA as the default: right-click
an MP4 file in Finder → Get Info → Open with → choose the app → Change All.
QuickTime supports common MP4 codecs; for broader codec and HLS support, use
VLC or IINA via `--player`.

On **Linux**, the default `video/mp4` desktop app is launched with `xdg-mime` and
`gtk-launch`. If those utilities are unavailable, use `--player vlc` or
`--player mpv`. On **Windows**, the default M3U app is opened; associate playlists
with VLC, or pass the player executable using `--player`. Actual player launch
behavior should be verified on each platform; automated tests cover the portable
networking and selection logic.

Keep the CLI running while watching. **Ctrl+C stops streaming** and removes the
temporary playlist. The CLI serves video on a randomly selected loopback port,
forwards HTTP Range requests for seeking, supplies the provider's Referer and
User-Agent headers, and rewrites HLS playlists, segment URLs, key URLs and init
maps through the local relay. Normal playback streams video as it arrives;
`--download` saves an episode to disk. Session cookies are kept only in memory,
including cookies set by video hosts.
Upstream requests accept only HTTP(S) URLs without embedded credentials, block
local and private network destinations (including DNS answers and redirects),
and connect directly rather than using environment-configured HTTP proxies.

`--url` prints the upstream URL and exits. Some hosts require headers, so use
normal playback through the relay if a bare URL doesn't work. Stream URLs may
expire; rerun the command to get a fresh one.

## Providers and troubleshooting

Supported: Tau Video, OK, Sibnet, Uqload (including packed player configs), and
public Google Drive files. Vidmoly, Doodstream, and Dailymotion are currently
skipped. Host quotas, verification pages, expired videos, or changes to upstream
APIs may prevent an individual provider from working. Try Auto or another host.

The HTTP API and request-signature protocol originated in
[Phirios/animecixing](https://github.com/Phirios/animecixing); the CLI has no
runtime dependency on it. The signing constant implements the website protocol
and is not a personal account credential.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests cover request signing, season selection, stream extraction, packed player
configs, HLS URI rewriting, and a local upstream server verifying Range and
Referer forwarding. Security tests check private destinations, DNS answers,
redirects, terminal escape sequences, and JSON output. Provider fixtures check
cookie retention and redirected HLS variants. Tests do not require Animecix to
be online. Download tests cover exact bytes, overwrite protection, cleanup,
and HLS remuxing with FFmpeg (when installed).

GitHub Actions runs tests, formatting, and Clippy on Linux, macOS, and Windows
with Rust 1.88, plus a dependency audit. Scripted commands with explicit season
and episode selections, and `--url`, skip optional TMDB metadata requests.

## License

MIT. See [LICENSE](LICENSE). This project is not affiliated with Animecix, TMDB,
or the video providers. The license covers this CLI code, not third-party media.
