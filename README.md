# animecix-cli

A standalone Rust CLI for searching Animecix, choosing an anime, season, episode,
video host and quality, and watching in your local video player. It contacts
Animecix and the video hosts directly: your friend's server, Kubernetes, and
PostgreSQL are not involved. Your own internet connection and the upstream sites
must still be available.

## Install

Requires Rust 1.85+ and a network-capable video player such as VLC or IINA.

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

# Explicit executable on Linux (also works for mpv on macOS)
animecix "naruto" --player vlc
```

For scripts without a terminal, supply `--id`, `--season`, and `--episode`.
The CLI reports ambiguous choices instead of silently selecting a different anime.
Progress goes to stderr; search results and `--url` output go to stdout.

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
show green, yellow, or red availability dots based on Animecix coverage. Press
`i` in those menus to view the series, season, or episode metadata.

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
with VLC, or pass the player executable using `--player`. Linux and Windows
launch paths have not been tested on this Mac.

Keep the CLI running while watching. **Ctrl+C stops streaming** and removes the
temporary playlist. The CLI serves video on a randomly selected loopback port,
forwards HTTP Range requests for seeking, supplies the provider's Referer and
User-Agent headers, and rewrites HLS playlists, segment URLs, key URLs and init
maps through the local relay. Video is streamed as it arrives; episodes are not
downloaded to disk. Session cookies are kept only in memory.

`--url` prints the upstream URL and exits. Some hosts require headers, so use
normal playback through the relay if a bare URL doesn't work. Stream URLs may
expire; rerun the command to get a fresh one.

## Providers and troubleshooting

Supported: Tau Video, OK, Sibnet, Uqload (including packed player configs), and
public Google Drive files. Vidmoly, Doodstream, and Dailymotion are currently
skipped. Host quotas, verification pages, expired videos, or changes to upstream
APIs may prevent an individual provider from working. Try Auto or another host.

The HTTP API and request-signature protocol were ported from the sibling
`animecixing` repository; the CLI has no runtime dependency on it.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests cover request signing, season selection, stream extraction, packed player
configs, HLS URI rewriting, and a local upstream server verifying Range and
Referer forwarding. Tests do not require Animecix to be online.
