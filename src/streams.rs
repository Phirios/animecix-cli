use crate::api::Video;
use crate::network::Client;
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub label: String,
    pub url: String,
    pub referer: String,
    pub provider: String,
}

pub fn client() -> Result<Client> {
    Client::new()
}

pub fn provider(video: &Video) -> &str {
    let host = Url::parse(&video.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    if host == "tau-video.xyz" {
        "tau-video"
    } else if host == "ok.ru" {
        "ok"
    } else if host == "video.sibnet.ru" {
        "sibnet"
    } else if host.starts_with("uqload.") {
        "uqload"
    } else if host == "drive.google.com" {
        "google-drive"
    } else {
        "unsupported"
    }
}

pub fn priority(video: &Video) -> usize {
    ["tau-video", "ok", "sibnet", "uqload", "google-drive"]
        .iter()
        .position(|p| *p == provider(video))
        .unwrap_or(99)
}

pub fn score(source: &Source) -> u32 {
    if source.label == "full" {
        return 10_000;
    }
    Regex::new(r"(\d{3,4})p?")
        .unwrap()
        .captures(&source.label)
        .and_then(|m| m[1].parse().ok())
        .unwrap_or(0)
}

async fn text(client: &Client, url: &str, referer: &str) -> Result<String> {
    Ok(client
        .get(url)?
        .header("Referer", referer)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

pub async fn resolve(client: &Client, video: &Video) -> Result<Vec<Source>> {
    let host = provider(video);
    let mut pairs: Vec<(String, String)> = Vec::new();
    match host {
        "tau-video" => {
            let url = Url::parse(&video.url)?;
            let parts: Vec<_> = url
                .path_segments()
                .context("Missing Tau video ID")?
                .collect();
            if parts.len() < 2 || !["embed", "embeded", "embed-2"].contains(&parts[0]) {
                bail!("Unrecognized Tau URL");
            }
            let mut api = Url::parse("https://tau-video.xyz/api/video/")?;
            api.path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .push(parts[1]);
            if let Some(id) = video.id {
                api.query_pairs_mut().append_pair("vid", &id.to_string());
            }
            let mut data = None;
            for attempt in 1..=4 {
                let response = client.get(api.as_str())?.send().await?;
                if response.status().as_u16() == 429 && attempt < 4 {
                    let seconds = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(attempt * 2)
                        .min(30);
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    continue;
                }
                data = Some(response.error_for_status()?.json::<Value>().await?);
                break;
            }
            let data = data.context("Tau returned no response")?;
            if let Some(urls) = data["urls"].as_array() {
                for source in urls {
                    if let Some(url) = source["url"].as_str() {
                        pairs.push((
                            source["label"].as_str().unwrap_or("video").into(),
                            url.into(),
                        ));
                    }
                }
            }
            if pairs.is_empty()
                && let Some(hls) = data["hls"].as_str()
            {
                pairs = hls_options(client, hls, &video.url).await?;
            }
        }
        "sibnet" | "uqload" => {
            let html = text(client, &video.url, &video.url).await?;
            let html = unpack(&html).unwrap_or(html);
            pairs = extract_sources(&html);
        }
        "ok" => {
            let html = text(client, &video.url, &video.url)
                .await?
                .replace("&quot;", "\"")
                .replace("&#34;", "\"")
                .replace("&amp;", "&");
            let re = Regex::new(r#""metadata":"((?:\\.|[^"\\])*)""#)?;
            let metadata = re.captures(&html).context("No OK video metadata")?;
            let raw: String = serde_json::from_str(&format!("\"{}\"", &metadata[1]))?;
            let data: Value = serde_json::from_str(&raw)?;
            if let Some(videos) = data["videos"].as_array() {
                for v in videos {
                    if let Some(url) = v["url"].as_str() {
                        let label = match v["name"].as_str().unwrap_or("video") {
                            "mobile" => "144p",
                            "lowest" => "240p",
                            "low" => "360p",
                            "sd" => "480p",
                            "hd" => "720p",
                            "full" => "1080p",
                            "quad" => "1440p",
                            "ultra" => "2160p",
                            other => other,
                        };
                        pairs.push((label.into(), url.into()));
                    }
                }
            }
            if pairs.is_empty()
                && let Some(hls) = data["ondemandHls"].as_str()
            {
                pairs = hls_options(client, hls, &video.url).await?;
            }
        }
        "google-drive" => {
            let re = Regex::new(r"/file/d/([^/]+)")?;
            let id = re.captures(&video.url).context("No Google Drive file ID")?;
            pairs.push((
                "original".into(),
                format!(
                    "https://drive.usercontent.google.com/uc?id={}&export=download",
                    &id[1]
                ),
            ));
        }
        _ => bail!("Unsupported video host"),
    }
    let base = Url::parse(&video.url)?;
    let mut sources = Vec::new();
    for (label, url) in pairs {
        let url = base.join(&url.replace("\\/", "/"))?;
        if !["http", "https"].contains(&url.scheme()) {
            continue;
        }
        if !sources.iter().any(|s: &Source| s.url == url.as_str()) {
            sources.push(Source {
                label: crate::terminal::text(&label),
                url: url.to_string(),
                referer: video.url.clone(),
                provider: host.into(),
            });
        }
    }
    sources.sort_by_key(|s| std::cmp::Reverse(score(s)));
    if sources.is_empty() {
        bail!("No playable sources from {host}");
    }
    Ok(sources)
}

pub fn extract_sources(html: &str) -> Vec<(String, String)> {
    let re =
        Regex::new(r#"(?:file|src|url|sources)["']?\s*:\s*(?:\[\s*)?["']([^"']+)["']"#).unwrap();
    re.captures_iter(html)
        .filter_map(|m| {
            let url = m[1].replace("\\/", "/");
            if !url.contains(".mp4") && !url.contains(".m3u8") {
                return None;
            }
            let label = Regex::new(r"(\d{3,4}p)")
                .unwrap()
                .captures(&url)
                .map(|m| m[1].to_owned())
                .unwrap_or_else(|| {
                    if url.contains(".m3u8") {
                        "HLS"
                    } else {
                        "video"
                    }
                    .into()
                });
            Some((label, url))
        })
        .collect()
}

async fn hls_options(client: &Client, url: &str, referer: &str) -> Result<Vec<(String, String)>> {
    let response = client
        .get(url)?
        .header("Referer", referer)
        .send()
        .await?
        .error_for_status()?;
    let base = response.url().clone();
    let playlist = response.text().await?;
    let lines: Vec<_> = playlist.lines().map(str::trim).collect();
    let re = Regex::new(r"RESOLUTION=\d+x(\d+)")?;
    let mut options = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("#EXT-X-STREAM-INF:")
            && let Some(uri) = lines[i + 1..]
                .iter()
                .find(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let label = re
                .captures(line)
                .map(|m| format!("{}p", &m[1]))
                .unwrap_or("HLS".into());
            options.push((label, base.join(uri)?.to_string()));
        }
    }
    if options.is_empty() {
        options.push(("HLS".into(), base.to_string()));
    }
    Ok(options)
}

fn unpack(html: &str) -> Option<String> {
    let re = Regex::new(r"(?s)eval\(function\(p,a,c,k,e,d\).*?\}\('((?:\\.|[^'\\])*)',(\d+),(\d+),'([^']*)'\.split\('\|'\)").ok()?;
    let m = re.captures(html)?;
    let radix: u32 = m[2].parse().ok()?;
    if !(2..=62).contains(&radix) {
        return None;
    }
    let count: usize = m[3].parse().ok()?;
    let dictionary: Vec<_> = m[4].split('|').collect();
    let mut payload = m[1].to_owned();
    for i in (0..count.min(dictionary.len())).rev() {
        if dictionary[i].is_empty() {
            continue;
        }
        let word = base_n(i, radix);
        payload = Regex::new(&format!(r"\b{word}\b"))
            .ok()?
            .replace_all(&payload, regex::NoExpand(dictionary[i]))
            .into_owned();
    }
    Some(
        payload
            .replace("\\'", "'")
            .replace("\\\"", "\"")
            .replace("\\/", "/"),
    )
}

fn base_n(mut n: usize, radix: u32) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut result = Vec::new();
    loop {
        result.push(digits[n % radix as usize] as char);
        n /= radix as usize;
        if n == 0 {
            break;
        }
    }
    result.iter().rev().collect()
}

pub async fn verify(client: &Client, source: &Source) -> Result<()> {
    let response = client
        .get(&source.url)?
        .header("Referer", &source.referer)
        .header("Range", "bytes=0-1023")
        .send()
        .await?
        .error_for_status()?;
    let content = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content.contains("text/html") || content.contains("application/json") {
        bail!("Host returned a web page instead of video (verification or quota limit)");
    }
    // Drop the response immediately; do not download the episode.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn hls_uses_redirect_destination_and_provider_cookies() {
        use axum::{
            Router,
            http::{HeaderMap, StatusCode},
            response::Redirect,
            routing::get,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(axum::serve(listener, Router::new()
            .route("/entry/master.m3u8", get(|| async {
                ([("set-cookie", "session=needed; Path=/")], Redirect::temporary("/actual/master.m3u8"))
            }))
            .route("/actual/master.m3u8", get(|headers: HeaderMap| async move {
                if headers.get("cookie").is_some_and(|c| c == "session=needed") {
                    (StatusCode::OK, "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nvariant.m3u8\n")
                } else { (StatusCode::FORBIDDEN, "missing cookie") }
            }))
            .route("/actual/variant.m3u8", get(|headers: HeaderMap| async move {
                assert_eq!(headers["cookie"], "session=needed");
                ([("content-type", "application/vnd.apple.mpegurl")], "#EXTM3U\nsegment.ts\n")
            }))
        ).into_future());
        let c = Client::for_test();
        let options = hls_options(&c, &format!("{base}/entry/master.m3u8"), &base)
            .await
            .unwrap();
        assert_eq!(
            options,
            vec![("720p".into(), format!("{base}/actual/variant.m3u8"))]
        );
        verify(
            &c,
            &Source {
                label: "720p".into(),
                url: options[0].1.clone(),
                referer: base,
                provider: "test".into(),
            },
        )
        .await
        .unwrap();
        upstream.abort();
    }

    #[test]
    fn extracts_relative_and_escaped_urls() {
        let sources = extract_sources(
            r#"player.src({src: "/v/720p.mp4"}); sources: ["https:\/\/cdn.test\/v.mp4"]; file: "advert.html""#,
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], ("720p".into(), "/v/720p.mp4".into()));
        assert_eq!(sources[1].1, "https://cdn.test/v.mp4");
    }
    #[test]
    fn unpacks_without_evaluating_javascript() {
        let html = "eval(function(p,a,c,k,e,d){return p;}('0: \"1\"',2,2,'file|https://cdn.test/a.mp4'.split('|'),0,{}))";
        assert_eq!(unpack(html).unwrap(), "file: \"https://cdn.test/a.mp4\"");
        assert_eq!(base_n(61, 62), "Z");
    }
}
