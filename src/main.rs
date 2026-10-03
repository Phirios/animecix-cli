mod api;
mod player;
mod relay;
mod streams;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, ValueHint};
use clap_complete::Shell;
use console::{Key, Term};
use dialoguer::{Input, theme::ColorfulTheme};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::{IsTerminal, Write},
    time::Duration,
};

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
        std::io::stdout().write_all(&completion_script(shell)?)?;
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
    let api = with_loading("Connecting to Animecix", api::Api::new())
        .await
        .context("Cannot connect to Animecix; check your internet connection")?;
    'anime_selection: loop {
        let (id, name) = if let Some(id) = args.id.filter(|_| !args.search) {
            (id.to_string(), query.clone())
        } else {
            loop {
                let results = with_loading("Searching Animecix", api.search(query.trim())).await?;
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
                let infos: Vec<_> = results.iter().map(search_info).collect();
                match choose_with_info(&labels, "Select anime", interactive, true, Some(&infos))? {
                    Choice::Selected(i) => {
                        break (api::title_id(&results[i])?, api::title_name(&results[i]));
                    }
                    Choice::Back => {
                        query = Input::<String>::with_theme(&theme)
                            .with_prompt("Search anime")
                            .interact_text()?;
                    }
                    Choice::Info(_) => unreachable!(),
                }
            }
        };
        let title = with_loading("Loading title metadata", api.title(&id, &name)).await?;
        let name = if name.is_empty() {
            api::title_name(&title)
        } else {
            name
        };
        eprintln!("{name}  [ID {id}]");
        let seasons = season_numbers(&title);
        let tmdb_series =
            match with_loading("Loading TMDB series metadata", api.tmdb_series(&title)).await {
                Ok(series) => series,
                Err(error) => {
                    eprintln!("TMDB series metadata unavailable: {}", safe_error(&error));
                    None
                }
            };
        let mut season_videos = BTreeMap::new();
        let mut tmdb_seasons = BTreeMap::new();
        if tmdb_series.is_some() {
            for season in &seasons {
                if let Ok(Some(metadata)) = with_loading(
                    &format!("Loading TMDB season {season}"),
                    api.tmdb_season(&title, *season),
                )
                .await
                {
                    tmdb_seasons.insert(*season, metadata);
                }
                if let Ok(videos) = with_loading(
                    &format!("Loading Animecix season {season}"),
                    api.videos(&id, *season),
                )
                .await
                {
                    season_videos.insert(
                        *season,
                        videos
                            .into_iter()
                            .filter(|video| video.season_num.is_none_or(|s| s == *season))
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }
        let (season, episode, videos) = 'episode_selection: loop {
            let season = match args.season {
                Some(n) => n,
                None => {
                    let labels: Vec<_> = seasons
                        .iter()
                        .map(|number| season_label(*number, &tmdb_seasons, &season_videos))
                        .collect();
                    let infos: Vec<_> = seasons
                        .iter()
                        .map(|number| {
                            season_info(*number, tmdb_series.as_ref(), tmdb_seasons.get(number))
                        })
                        .collect();
                    match choose_with_info(
                        &labels,
                        "Select season",
                        interactive,
                        false,
                        Some(&infos),
                    )? {
                        Choice::Selected(i) => seasons[i],
                        Choice::Back if args.id.is_some() => bail!("Selection cancelled"),
                        Choice::Back => continue 'anime_selection,
                        Choice::Info(_) => unreachable!(),
                    }
                }
            };
            let videos: Vec<_> = match season_videos.get(&season) {
                Some(videos) => videos.clone(),
                None => with_loading(
                    &format!("Loading Animecix season {season}"),
                    api.videos(&id, season),
                )
                .await?
                .into_iter()
                .filter(|v| v.season_num.is_none_or(|s| s == season))
                .collect(),
            };
            let episodes = episode_numbers(&videos);
            if episodes.is_empty() {
                bail!("No videos available for season {season}");
            }
            let tmdb_season = tmdb_seasons.get(&season);
            let episode = match args.episode {
                Some(n) => n,
                None => match choose_with_info(
                    &episode_labels(&episodes, tmdb_season, &videos),
                    "Select episode (type to filter)",
                    interactive,
                    true,
                    Some(&episode_infos(&episodes, tmdb_series.as_ref(), tmdb_season)),
                )? {
                    Choice::Selected(i) => episodes[i],
                    Choice::Back => continue 'episode_selection,
                    Choice::Info(_) => unreachable!(),
                },
            };
            break (season, episode, videos);
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
            let selected = choose_index(&labels, "Select video host", true, false)?;
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
                let i = choose_index(
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
                    Err(error) => {
                        eprintln!("  {} unavailable: {}", source.label, safe_error(&error))
                    }
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let source = selected
            .context("No playable stream found. Try a different host, quality, or episode")?;
        if args.url {
            println!("{}", source.url);
            break 'anime_selection Ok(());
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
        break 'anime_selection Ok(());
    }
}

fn completion_script(shell: Shell) -> Result<Vec<u8>> {
    let mut command = Args::command();
    let mut output = Vec::new();
    clap_complete::generate(shell, &mut command, "animecix", &mut output);
    if shell != Shell::Zsh {
        return Ok(output);
    }
    let mut script = String::from_utf8(output)?;
    // Anime names are not file paths. At an empty argument offer options;
    // when a query is entered, leave the user's text intact.
    script = script.replace(
        "*::query -- Anime name (omit to enter it interactively):_default",
        "*::query -- Anime name (omit to enter it interactively):_animecix_query",
    );
    let mut helper = String::from(
        "_animecix_query() {\n    if [[ -n $PREFIX ]]; then\n        _message 'anime name (run animecix to search interactively)'\n        return\n    fi\n    local -a options\n    options=(\n",
    );
    for arg in command.get_arguments().filter(|a| !a.is_hide_set()) {
        if let Some(long) = arg.get_long() {
            let description = arg
                .get_help()
                .map(|h| h.to_string())
                .unwrap_or_else(|| long.to_owned());
            let entry = format!("--{long}:{}", description.replace(':', " "));
            helper.push_str(&format!("        '{}'\n", entry.replace('\'', "'\\''")));
        }
    }
    helper.push_str("    )\n    _describe -O -t options 'animecix options' options\n    local result=$?\n    compstate[insert]=''\n    compstate[list]=list\n    return $result\n}\n\n");
    script = script.replacen("_animecix() {", &format!("{helper}_animecix() {{"), 1);
    Ok(script.into_bytes())
}

fn safe_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .split(" for url")
        .next()
        .unwrap_or("Host error")
        .to_owned()
}

async fn with_loading<T, F>(label: &str, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let term = Term::stderr();
    if !term.is_term() {
        return future.await;
    }
    let label = label.to_owned();
    let spinner = tokio::spawn(async move {
        let frames = ["|", "/", "-", "\\"];
        let mut frame = 0;
        loop {
            let _ = term.write_str(&format!("\r\x1b[2K{} {}", frames[frame], label));
            let _ = term.flush();
            frame = (frame + 1) % frames.len();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let result = future.await;
    spinner.abort();
    let term = Term::stderr();
    term.write_str("\r\x1b[2K")?;
    term.flush()?;
    result
}

enum Choice {
    Selected(usize),
    Back,
    Info(usize),
}

fn choose(labels: &[String], prompt: &str, interactive: bool, fuzzy: bool) -> Result<Choice> {
    if labels.is_empty() {
        bail!("No choices available for {prompt}");
    }
    if labels.len() == 1 {
        return Ok(Choice::Selected(0));
    }
    if !interactive {
        bail!(
            "{prompt} requires an interactive terminal. Use --search to find the ID, then supply --id, --season, and --episode"
        );
    }
    let term = Term::stderr();
    let mut filter = String::new();
    let mut selected = 0;
    let mut rendered_lines = 0;
    term.hide_cursor()?;
    let result = loop {
        let matches: Vec<_> = labels
            .iter()
            .enumerate()
            .filter(|(_, label)| !fuzzy || label.to_lowercase().contains(&filter.to_lowercase()))
            .collect();
        if matches.is_empty() {
            selected = 0;
        } else {
            selected = selected.min(matches.len() - 1);
        }
        if rendered_lines > 0 {
            term.clear_last_lines(rendered_lines)?;
        }
        term.write_line(&format!("? {prompt}  (press i for more info, ←/Esc back)"))?;
        if fuzzy && !filter.is_empty() {
            term.write_line(&format!("  Filter: {filter}"))?;
        }
        if matches.is_empty() {
            term.write_line("  No matching choices")?;
        } else {
            for (position, (_, label)) in matches.iter().enumerate() {
                if position == selected {
                    let highlighted =
                        label.replace("\x1b[0m", "\x1b[0m\x1b[48;5;24m\x1b[38;5;255m");
                    term.write_line(&format!(
                        "\x1b[48;5;24m\x1b[38;5;255m❯ {highlighted}\x1b[0m"
                    ))?;
                } else {
                    term.write_line(&format!("  {label}"))?;
                }
            }
        }
        rendered_lines = 1 + usize::from(fuzzy && !filter.is_empty()) + matches.len().max(1);
        term.flush()?;
        match term.read_key()? {
            Key::ArrowDown | Key::Tab if !matches.is_empty() => {
                selected = (selected + 1) % matches.len();
            }
            Key::ArrowUp | Key::BackTab if !matches.is_empty() => {
                selected = selected.checked_sub(1).unwrap_or(matches.len() - 1);
            }
            Key::ArrowLeft | Key::Escape => break Choice::Back,
            Key::Char('i') if !matches.is_empty() => break Choice::Info(matches[selected].0),
            Key::Enter if !matches.is_empty() => break Choice::Selected(matches[selected].0),
            Key::Backspace if fuzzy => {
                filter.pop();
                selected = 0;
            }
            Key::Char('q') if filter.is_empty() => break Choice::Back,
            Key::Char(character) if fuzzy && !character.is_ascii_control() => {
                filter.push(character);
                selected = 0;
            }
            _ => {}
        }
    };
    term.clear_last_lines(rendered_lines)?;
    term.show_cursor()?;
    term.flush()?;
    Ok(result)
}

fn choose_with_info(
    labels: &[String],
    prompt: &str,
    interactive: bool,
    fuzzy: bool,
    infos: Option<&[String]>,
) -> Result<Choice> {
    loop {
        match choose(labels, prompt, interactive, fuzzy)? {
            Choice::Info(index) => {
                if let Some(info) = infos.and_then(|infos| infos.get(index)) {
                    show_info(info)?;
                }
            }
            choice => return Ok(choice),
        }
    }
}

fn show_info(info: &str) -> Result<()> {
    let term = Term::stderr();
    for line in info.lines() {
        term.write_line(line)?;
    }
    term.write_line("\x1b[2mPress any key to return to the selection\x1b[0m")?;
    term.read_key()?;
    Ok(())
}

fn choose_index(labels: &[String], prompt: &str, interactive: bool, fuzzy: bool) -> Result<usize> {
    loop {
        match choose(labels, prompt, interactive, fuzzy)? {
            Choice::Selected(index) => return Ok(index),
            Choice::Back => bail!("Selection cancelled"),
            Choice::Info(_) => show_info("\x1b[1;33mNo additional information available\x1b[0m")?,
        }
    }
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

fn season_label(
    number: u32,
    tmdb_seasons: &BTreeMap<u32, api::TmdbSeason>,
    season_videos: &BTreeMap<u32, Vec<api::Video>>,
) -> String {
    let label = format!("Season {number}");
    let Some(tmdb_season) = tmdb_seasons.get(&number) else {
        return label;
    };
    let videos = season_videos.get(&number).map(Vec::as_slice).unwrap_or(&[]);
    format!(
        "{} {}",
        availability_dot(tmdb_season.episodes.len(), available_episode_count(videos)),
        label
    )
}

fn search_info(title: &Value) -> String {
    let mut info = format!("\x1b[1;36m{}\x1b[0m", api::title_name(title));
    if let Ok(id) = api::title_id(title) {
        info.push_str(&format!("\n\x1b[1;33mAnimecix ID:\x1b[0m {id}"));
    }
    for (label, key) in [
        ("Release date", "release_date"),
        ("Year", "year"),
        ("Description", "overview"),
        ("Description", "description"),
    ] {
        if let Some(value) = title[key].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            info.push_str(&format!("\n\x1b[1;33m{label}:\x1b[0m {value}"));
        }
    }
    info
}

fn episode_labels(
    numbers: &[f64],
    tmdb_season: Option<&api::TmdbSeason>,
    videos: &[api::Video],
) -> Vec<String> {
    numbers
        .iter()
        .map(|number| {
            let Some(episode) = tmdb_season.and_then(|season| {
                season
                    .episodes
                    .iter()
                    .find(|episode| episode.episode_number == *number)
            }) else {
                return format!("Episode {number}");
            };
            let available = videos
                .iter()
                .filter(|video| video.episode_num.unwrap_or(1.0) == *number)
                .count();
            let supported = videos.iter().any(|video| {
                video.episode_num.unwrap_or(1.0) == *number
                    && streams::provider(video) != "unsupported"
            });
            format!(
                "{} Episode {number} — {}",
                episode_dot(available, supported),
                episode.name
            )
        })
        .collect()
}

fn availability_dot(expected: usize, available: usize) -> String {
    if expected == 0 {
        String::new()
    } else if available == 0 {
        "\x1b[31m●\x1b[0m".to_owned()
    } else if available < expected {
        "\x1b[33m●\x1b[0m".to_owned()
    } else {
        "\x1b[32m●\x1b[0m".to_owned()
    }
}

fn episode_dot(entries: usize, supported: bool) -> String {
    if supported {
        "\x1b[32m●\x1b[0m".to_owned()
    } else if entries > 0 {
        "\x1b[33m●\x1b[0m".to_owned()
    } else {
        "\x1b[31m●\x1b[0m".to_owned()
    }
}

fn available_episode_count(videos: &[api::Video]) -> usize {
    let mut numbers: Vec<_> = videos
        .iter()
        .filter(|video| streams::provider(video) != "unsupported")
        .map(|video| video.episode_num.unwrap_or(1.0))
        .collect();
    numbers.sort_by(f64::total_cmp);
    numbers.dedup();
    numbers.len()
}

fn season_info(
    number: u32,
    series: Option<&api::TmdbSeries>,
    season: Option<&api::TmdbSeason>,
) -> String {
    let mut info = series_info(series);
    if let Some(season) = season {
        info.push_str(&format!(
            "\n\x1b[1;35mSeason {number}: {}\x1b[0m",
            season.name
        ));
        append_metadata(
            &mut info,
            season.air_date.as_deref(),
            None,
            None,
            season.overview.as_deref(),
        );
    } else {
        info.push_str(&format!("\nSeason {number}\nNo TMDB season metadata"));
    }
    info
}

fn episode_infos(
    numbers: &[f64],
    series: Option<&api::TmdbSeries>,
    season: Option<&api::TmdbSeason>,
) -> Vec<String> {
    numbers
        .iter()
        .map(|number| {
            let mut info = series_info(series);
            if let Some(episode) = season.and_then(|season| {
                season
                    .episodes
                    .iter()
                    .find(|episode| episode.episode_number == *number)
            }) {
                info.push_str(&format!(
                    "\n\x1b[1;35mEpisode {number}: {}\x1b[0m",
                    episode.name
                ));
                append_metadata(
                    &mut info,
                    episode.air_date.as_deref(),
                    episode.runtime,
                    episode.vote_average,
                    episode.overview.as_deref(),
                );
            } else {
                info.push_str(&format!("\nEpisode {number}\nNo TMDB episode metadata"));
            }
            info
        })
        .collect()
}

fn series_info(series: Option<&api::TmdbSeries>) -> String {
    let Some(series) = series else {
        return "\x1b[1;33mNo TMDB metadata available\x1b[0m".to_owned();
    };
    let mut info = format!("\x1b[1;36m{}\x1b[0m", series.name);
    append_metadata(
        &mut info,
        series.first_air_date.as_deref(),
        series.runtime,
        series.vote_average,
        series.overview.as_deref(),
    );
    info
}

fn append_metadata(
    info: &mut String,
    date: Option<&str>,
    runtime: Option<u64>,
    vote_average: Option<f64>,
    overview: Option<&str>,
) {
    if let Some(date) = date {
        info.push_str(&format!("\n\x1b[1;33mRelease date:\x1b[0m {date}"));
    }
    if let Some(runtime) = runtime {
        info.push_str(&format!("\n\x1b[1;33mDuration:\x1b[0m {runtime} minutes"));
    }
    if let Some(vote_average) = vote_average {
        info.push_str(&format!("\n\x1b[1;33mRating:\x1b[0m {vote_average:.1}/10"));
    }
    if let Some(overview) = overview {
        info.push_str(&format!("\n\x1b[1;33mDescription:\x1b[0m {overview}"));
    }
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
            let output = String::from_utf8(completion_script(shell).unwrap()).unwrap();
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
    fn episode_labels_include_titles_and_fallback_without_one() {
        let season = api::TmdbSeason {
            name: "Season 1".into(),
            air_date: None,
            overview: None,
            episodes: vec![api::TmdbEpisode {
                episode_number: 1.0,
                name: "Enter the Battle".into(),
                air_date: None,
                runtime: None,
                vote_average: None,
                overview: None,
            }],
        };
        assert_eq!(
            episode_labels(&[1.0, 2.0], Some(&season), &[]),
            ["\x1b[31m●\x1b[0m Episode 1 — Enter the Battle", "Episode 2"]
        );
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
