mod api;
mod player;
mod relay;
mod streams;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, ValueHint};
use clap_complete::Shell;
use dialoguer::{FuzzySelect, Input, Select, theme::ColorfulTheme};
use serde_json::Value;
use std::{collections::BTreeSet, io::IsTerminal};

#[derive(Parser, Debug)]
#[command(
    name = "animecix",
    version,
    about = "Search Animecix and stream directly in your local video player",
    after_help = "Examples:\n  animecix \"one piece\"\n  animecix --search naruto\n  animecix --id 7293 --season 4 --episode 1\n  animecix naruto --player /Applications/VLC.app\n\nKeep the CLI running while watching. Ctrl+C stops the local streaming relay."
)]
struct Args {
    /// Anime name (omit to enter it interactively)
    query: Vec<String>,
    /// List search results without playing
    #[arg(long)]
    search: bool,
    /// JSON search results
    #[arg(long, requires = "search")]
    json: bool,
    /// Skip search and select an Animecix title ID
    #[arg(long)]
    id: Option<u64>,
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    season: Option<u32>,
    /// Episode number, including specials such as 13.5
    #[arg(short, long, value_parser = parse_episode)]
    episode: Option<f64>,
    /// Override default MP4 app (e.g. /Applications/VLC.app, vlc, or mpv)
    #[arg(long, value_hint = ValueHint::FilePath)]
    player: Option<String>,
    /// Choose a host; default tries supported hosts in order
    #[arg(long, value_parser = ["auto", "tau-video", "ok", "sibnet", "uqload", "google-drive"], default_value = "auto")]
    provider: String,
    /// Resolution label such as 720p, 1080p, full, or best
    #[arg(long)]
    quality: Option<String>,
    /// Print the remote stream URL instead of opening a player (no relay)
    #[arg(long)]
    url: bool,
    /// Generate shell completion without connecting to Animecix
    #[arg(long, value_enum)]
    completions: Option<Shell>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    if let Some(shell) = args.completions {
        clap_complete::generate(
            shell,
            &mut Args::command(),
            "animecix",
            &mut std::io::stdout(),
        );
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let theme = ColorfulTheme::default();
    let mut query = args.query.join(" ");
    if query.trim().is_empty() && args.id.is_none() {
        if !interactive {
            bail!("Supply an anime name or --id (see --help)");
        }
        query = Input::<String>::with_theme(&theme)
            .with_prompt("Search anime")
            .interact_text()?;
    }
    if args.search && query.trim().is_empty() {
        bail!("--search requires an anime name");
    }
    eprintln!("Connecting directly to Animecix…");
    let api = api::Api::new()
        .await
        .context("Cannot connect to Animecix; check your internet connection")?;
    let (id, name) = if let Some(id) = args.id.filter(|_| !args.search) {
        (id.to_string(), query.clone())
    } else {
        let results = api.search(query.trim()).await?;
        if args.search {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                if results.is_empty() {
                    println!("No anime found for {query:?}");
                }
                for title in results {
                    println!("{}\t{}", api::title_id(&title)?, api::title_name(&title));
                }
            }
            return Ok(());
        }
        if results.is_empty() {
            bail!("No anime found for {query:?}");
        }
        let labels: Vec<_> = results
            .iter()
            .map(|v| {
                format!(
                    "{}  [ID {}]",
                    api::title_name(v),
                    api::title_id(v).unwrap_or_default()
                )
            })
            .collect();
        let i = choose(&labels, "Select anime", interactive, true)?;
        (api::title_id(&results[i])?, api::title_name(&results[i]))
    };
    let title = api.title(&id, &name).await?;
    let name = if name.is_empty() {
        api::title_name(&title)
    } else {
        name
    };
    eprintln!("{name}  [ID {id}]");
    let seasons = season_numbers(&title);
    let season = match args.season {
        Some(n) => n,
        None => {
            seasons[choose(
                &seasons
                    .iter()
                    .map(|n| format!("Season {n}"))
                    .collect::<Vec<_>>(),
                "Select season",
                interactive,
                false,
            )?]
        }
    };
    let videos = api.videos(&id, season).await?;
    let videos: Vec<_> = videos
        .into_iter()
        .filter(|v| v.season_num.is_none_or(|s| s == season))
        .collect();
    let episodes = episode_numbers(&videos);
    if episodes.is_empty() {
        bail!("No videos available for season {season}");
    }
    let episode = match args.episode {
        Some(n) => n,
        None => {
            episodes[choose(
                &episodes
                    .iter()
                    .map(|n| format!("Episode {n}"))
                    .collect::<Vec<_>>(),
                "Select episode (type to filter)",
                interactive,
                true,
            )?]
        }
    };
    let mut candidates: Vec<_> = videos
        .into_iter()
        .filter(|v| v.episode_num.unwrap_or(1.0) == episode)
        .filter(|v| streams::provider(v) != "unsupported")
        .filter(|v| args.provider == "auto" || args.provider == streams::provider(v))
        .collect();
    candidates.sort_by_key(streams::priority);
    // A season may contain duplicate entries for the same provider URL.
    let mut seen = BTreeSet::new();
    candidates.retain(|v| seen.insert((v.id, v.url.clone())));
    if candidates.is_empty() {
        bail!(
            "No supported {} streams for S{season} E{episode}. Try another episode or --provider auto",
            args.provider
        );
    }
    if interactive && args.provider == "auto" && !args.url {
        let mut labels = vec!["Auto (try hosts until one works)".to_owned()];
        labels.extend(candidates.iter().map(|v| {
            format!(
                "{}  [video {}]",
                v.name.as_deref().unwrap_or(streams::provider(v)),
                v.id.map(|n| n.to_string()).unwrap_or_default()
            )
        }));
        let selected = choose(&labels, "Select video host", true, false)?;
        if selected > 0 {
            candidates = vec![candidates.remove(selected - 1)];
        }
    }
    let client = streams::client()?;
    let mut selected = None;
    for video in candidates {
        eprintln!("Resolving {}…", streams::provider(&video));
        // Bound a whole resolver, including retries and host page reads.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            streams::resolve(&client, &video),
        )
        .await;
        let mut sources = match result {
            Ok(Ok(sources)) => sources,
            Ok(Err(error)) => {
                eprintln!("  Host unavailable: {}", safe_error(&error));
                continue;
            }
            Err(_) => {
                eprintln!("  Host timed out");
                continue;
            }
        };
        if let Some(quality) = args.quality.as_deref().filter(|q| *q != "best") {
            sources.retain(|s| {
                s.label.eq_ignore_ascii_case(quality)
                    || s.label.trim_end_matches('p') == quality.trim_end_matches('p')
            });
            if sources.is_empty() {
                eprintln!("  Requested quality {quality} is unavailable");
                continue;
            }
        } else if interactive && args.quality.is_none() && !args.url && sources.len() > 1 {
            let i = choose(
                &sources.iter().map(|s| s.label.clone()).collect::<Vec<_>>(),
                "Select quality",
                true,
                false,
            )?;
            sources = vec![sources.remove(i)];
        }
        for source in sources {
            match streams::verify(&client, &source).await {
                Ok(()) => {
                    selected = Some(source);
                    break;
                }
                Err(error) => eprintln!("  {} unavailable: {}", source.label, safe_error(&error)),
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let source =
        selected.context("No playable stream found. Try a different host, quality, or episode")?;
    if args.url {
        println!("{}", source.url);
        return Ok(());
    }
    let relay = relay::start(client, &source).await?;
    let directory = tempfile::tempdir()?;
    let playlist = directory.path().join("animecix.m3u");
    // The title is untrusted remote data; strip newlines before writing M3U metadata.
    let label = format!("{name} — S{season} E{episode}").replace(['\r', '\n'], " ");
    std::fs::write(
        &playlist,
        format!("#EXTM3U\n#EXTINF:-1,{label}\n{}\n", relay.url),
    )?;
    player::launch(&playlist, args.player.as_deref(), &relay.url)?;
    eprintln!("Playing {label} · {} · {}", source.provider, source.label);
    eprintln!("Keep this terminal open while watching. Ctrl+C stops streaming.");
    tokio::signal::ctrl_c().await?;
    drop(relay);
    Ok(())
}

fn safe_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .split(" for url")
        .next()
        .unwrap_or("Host error")
        .to_owned()
}

fn choose(labels: &[String], prompt: &str, interactive: bool, fuzzy: bool) -> Result<usize> {
    if labels.is_empty() {
        bail!("No choices available for {prompt}");
    }
    if labels.len() == 1 {
        return Ok(0);
    }
    if !interactive {
        bail!(
            "{prompt} requires an interactive terminal. Use --search to find the ID, then supply --id, --season, and --episode"
        );
    }
    let theme = ColorfulTheme::default();
    let result = if fuzzy {
        FuzzySelect::with_theme(&theme)
            .with_prompt(prompt)
            .items(labels)
            .default(0)
            .interact_opt()?
    } else {
        Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(labels)
            .default(0)
            .interact_opt()?
    };
    result.context("Selection cancelled")
}

fn parse_episode(input: &str) -> std::result::Result<f64, String> {
    let number: f64 = input
        .parse()
        .map_err(|_| "Expected an episode number such as 1 or 13.5".to_owned())?;
    if !number.is_finite() || number < 0.0 {
        return Err("Episode number must be finite and nonnegative".into());
    }
    Ok(number)
}

fn episode_numbers(videos: &[api::Video]) -> Vec<f64> {
    let mut numbers: Vec<_> = videos
        .iter()
        .map(|v| v.episode_num.unwrap_or(1.0))
        .collect();
    numbers.sort_by(f64::total_cmp);
    numbers.dedup();
    numbers
}

fn season_numbers(title: &Value) -> Vec<u32> {
    let seasons: BTreeSet<_> = title["seasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            s.as_u64()
                .or_else(|| s["season_number"].as_u64())
                .or_else(|| s["number"].as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
        })
        .collect();
    if seasons.is_empty() {
        vec![1]
    } else {
        seasons.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_episodes_parse_sort_and_select_without_truncation() {
        let videos: Vec<api::Video> = serde_json::from_value(serde_json::json!([
            {"url": "https://example.test/14", "episode_num": 14},
            {"url": "https://example.test/special", "episode_num": 13.5},
            {"url": "https://example.test/13", "episode_num": 13},
            {"url": "https://example.test/special-duplicate", "episode_num": 13.5}
        ]))
        .unwrap();
        assert_eq!(episode_numbers(&videos), [13.0, 13.5, 14.0]);
        let args =
            Args::try_parse_from(["animecix", "--id", "25", "-s", "1", "-e", "13.5"]).unwrap();
        assert_eq!(args.episode, Some(13.5));
        assert_eq!(
            videos
                .iter()
                .filter(|v| v.episode_num == args.episode)
                .count(),
            2
        );
        for value in ["NaN", "inf", "-1"] {
            assert!(parse_episode(value).is_err());
        }
    }

    #[test]
    fn completion_contains_options_and_provider_values_for_supported_shells() {
        for shell in [
            Shell::Zsh,
            Shell::Bash,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let mut output = Vec::new();
            clap_complete::generate(shell, &mut Args::command(), "animecix", &mut output);
            let output = String::from_utf8(output).unwrap();
            assert!(
                output.contains("episode"),
                "missing episode completion for {shell}"
            );
            if matches!(shell, Shell::Zsh | Shell::Bash | Shell::Fish) {
                assert!(
                    output.contains("tau-video"),
                    "missing provider values for {shell}"
                );
            }
        }
        let args = Args::try_parse_from(["animecix", "--completions", "zsh"]).unwrap();
        assert_eq!(args.completions, Some(Shell::Zsh));
    }

    #[test]
    fn supports_season_shapes_and_removes_duplicates() {
        let title =
            serde_json::json!({"seasons": [2, {"season_number": 4}, {"number": 1}, 2, {}, 0]});
        assert_eq!(season_numbers(&title), [1, 2, 4]);
    }
    #[test]
    fn rejects_ambiguous_noninteractive_choices() {
        assert!(
            choose(
                &["Naruto".into(), "Shippuden".into()],
                "Select anime",
                false,
                true
            )
            .is_err()
        );
    }
}
