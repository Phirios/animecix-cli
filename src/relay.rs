use crate::network::Client;
use crate::streams::Source;
use anyhow::Result;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Url;
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone)]
struct RelayState {
    client: Client,
    referer: String,
    urls: Arc<Mutex<Vec<String>>>,
    prefix: String,
    token: String,
}

pub struct Relay {
    pub url: String,
    task: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start(client: Client, source: &Source) -> Result<Relay> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = format!("{:032x}", rand::random::<u128>());
    let prefix = format!("http://127.0.0.1:{}/{token}", listener.local_addr()?.port());
    let hls = source.url.contains(".m3u8") || source.label == "HLS";
    let url = format!("{prefix}/0/stream.{}", if hls { "m3u8" } else { "mp4" });
    let state = RelayState {
        client,
        referer: source.referer.clone(),
        urls: Arc::new(Mutex::new(vec![source.url.clone()])),
        prefix,
        token,
    };
    let app = Router::new()
        .route("/{token}/{id}/{name}", get(handle))
        .with_state(state);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("Streaming relay stopped: {error}");
        }
    });
    Ok(Relay { url, task })
}

async fn handle(
    State(state): State<RelayState>,
    Path((token, id, _)): Path<(String, usize, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    let url = state
        .urls
        .lock()
        .expect("URL registry poisoned")
        .get(id)
        .cloned();
    let Some(url) = url else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match forward(&state, &url, method, headers).await {
        Ok(response) => response,
        Err(error) => {
            // Do not print signed stream URLs from reqwest errors.
            eprintln!(
                "Stream request failed: {}",
                crate::terminal::text(
                    error
                        .root_cause()
                        .to_string()
                        .split(" for url")
                        .next()
                        .unwrap_or("upstream error")
                )
            );
            (StatusCode::BAD_GATEWAY, "Video host request failed").into_response()
        }
    }
}

async fn forward(
    state: &RelayState,
    url: &str,
    method: Method,
    headers: HeaderMap,
) -> Result<Response> {
    let mut request = state
        .client
        .request(method.clone(), url)?
        .header("Referer", &state.referer);
    for name in ["range", "if-range", "if-none-match", "if-modified-since"] {
        if let Some(value) = headers.get(name) {
            request = request.header(name, value);
        }
    }
    let mut response = request.send().await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let hls = response.url().path().ends_with(".m3u8")
        || content_type.to_ascii_lowercase().contains("mpegurl");
    let mut builder = Response::builder().status(status);
    for name in [
        "content-type",
        "content-range",
        "accept-ranges",
        "etag",
        "last-modified",
        "cache-control",
    ] {
        if let Some(value) = response.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    if method == Method::HEAD {
        if let Some(length) = response.headers().get("content-length") {
            builder = builder.header("content-length", length);
        }
        return Ok(builder.body(Body::empty())?);
    }
    let mut prefix = crate::network::peek(&mut response).await?;
    if (hls || crate::network::hls_prefix(&prefix)) && status.is_success() {
        let base = response.url().clone();
        prefix.extend_from_slice(&response.bytes().await?);
        let playlist = rewrite_playlist(&String::from_utf8_lossy(&prefix), &base, |url| {
            state.client.validate(&url)?;
            let mut urls = state.urls.lock().expect("URL registry poisoned");
            let index = if let Some(index) = urls.iter().position(|s| s == url.as_str()) {
                index
            } else {
                urls.push(url.to_string());
                urls.len() - 1
            };
            let filename = if url.path().ends_with(".m3u8") {
                "playlist.m3u8"
            } else {
                // Keep conventional segment extensions for FFmpeg's HLS
                // validation, without putting upstream paths in local URLs.
                match url.path().rsplit('.').next() {
                    Some("mp4") => "segment.mp4",
                    Some("m4s") => "segment.m4s",
                    Some("aac") => "segment.aac",
                    Some("mp3") => "segment.mp3",
                    _ => "segment.ts",
                }
            };
            Ok(format!("{}/{index}/{filename}", state.prefix))
        })?;
        Ok(builder
            .header("content-type", "application/vnd.apple.mpegurl")
            .body(Body::from(playlist))?)
    } else {
        if let Some(value) = response.headers().get("content-length") {
            builder = builder.header("content-length", value);
        }
        let body =
            futures_util::stream::once(async move { Ok::<_, reqwest::Error>(prefix.into()) })
                .chain(response.bytes_stream());
        Ok(builder.body(Body::from_stream(body))?)
    }
}

fn rewrite_playlist(
    text: &str,
    base: &Url,
    mut register: impl FnMut(Url) -> Result<String>,
) -> Result<String> {
    let uri = Regex::new(r#"URI="([^"]+)""#)?;
    let mut output = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            let mut rewritten = line.to_owned();
            for capture in uri.captures_iter(line) {
                let url = base.join(&capture[1])?;
                if ["http", "https"].contains(&url.scheme()) {
                    rewritten =
                        rewritten.replace(&capture[0], &format!("URI=\"{}\"", register(url)?));
                } else {
                    anyhow::bail!("Unsupported HLS URI scheme");
                }
            }
            output.push_str(&rewritten);
        } else if !line.is_empty() {
            let url = base.join(line)?;
            if ["http", "https"].contains(&url.scheme()) {
                output.push_str(&register(url)?);
            } else {
                anyhow::bail!("Unsupported HLS URI scheme");
            }
        }
        output.push('\n');
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    #[test]
    fn rewrites_segments_keys_and_init_maps() {
        let input = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../key\"\n#EXT-X-MAP:URI=\"init.mp4\"\npart.ts\nhttps://cdn.test/next.m3u8\n";
        let output = rewrite_playlist(
            input,
            &Url::parse("https://host.test/hls/list.m3u8").unwrap(),
            |url| Ok(format!("local:{url}")),
        )
        .unwrap();
        assert!(output.contains("URI=\"local:https://host.test/key\""));
        assert!(output.contains("URI=\"local:https://host.test/hls/init.mp4\""));
        assert!(output.contains("local:https://host.test/hls/part.ts"));
        assert!(output.contains("local:https://cdn.test/next.m3u8"));
    }

    #[test]
    fn rejects_local_and_non_http_playlist_targets() {
        for target in [
            "http://127.0.0.1/private",
            "http://192.168.1.1/admin",
            "file:///etc/passwd",
        ] {
            let c = Client::new().unwrap();
            let result = rewrite_playlist(
                &format!("#EXTM3U\n{target}\n"),
                &Url::parse("https://cdn.test/master.m3u8").unwrap(),
                |url| {
                    c.validate(&url)?;
                    Ok("local".into())
                },
            );
            assert!(result.is_err(), "accepted {target}");
        }
    }

    #[tokio::test]
    async fn forwards_ranges_and_referer_and_rejects_unknown_routes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}/video", listener.local_addr().unwrap());
        let upstream = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/video",
                    get(|headers: HeaderMap| async move {
                        assert_eq!(headers[header::RANGE], "bytes=2-4");
                        assert_eq!(headers[header::REFERER], "https://host.test/embed/1");
                        (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                (header::CONTENT_RANGE, "bytes 2-4/10"),
                                (header::CONTENT_TYPE, "video/mp4"),
                            ],
                            "234",
                        )
                    }),
                ),
            )
            .into_future(),
        );
        let client = Client::for_test();
        let relay = start(
            client.clone(),
            &Source {
                url: upstream_url,
                referer: "https://host.test/embed/1".into(),
                label: "720p".into(),
                provider: "test".into(),
            },
        )
        .await
        .unwrap();
        let result = client
            .get(&relay.url)
            .unwrap()
            .header("Range", "bytes=2-4")
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(result.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(result.text().await.unwrap(), "234");
        assert_eq!(
            client
                .get(relay.url.replace("/0/", "/99/"))
                .unwrap()
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let unknown = format!(
            "{}/bad/0/file",
            relay.url.split('/').take(3).collect::<Vec<_>>().join("/")
        );
        assert_eq!(
            client.get(unknown).unwrap().send().await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        upstream.abort();
    }
}
