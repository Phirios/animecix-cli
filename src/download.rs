use crate::{network::Client, relay, streams::Source, terminal};
use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

pub fn check_destination(path: &Path) -> Result<()> {
    if path.file_name().is_none() {
        bail!("--download requires a file path");
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => bail!("Download destination already exists; choose a different file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if !parent(path).is_dir() {
        bail!("Download destination directory does not exist");
    }
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub async fn save(client: Client, source: &Source, path: &Path) -> Result<()> {
    check_destination(path)?;
    // Keep the temporary file on the same filesystem for atomic publication.
    let temporary = tempfile::Builder::new()
        .prefix(".animecix-")
        .suffix(".part")
        .tempfile_in(parent(path))
        .context("Cannot create download file")?;
    let mut response = tokio::select! {
        response = client.get(&source.url)?.header("Referer", &source.referer).send() => {
            response.map_err(reqwest::Error::without_url)?.error_for_status()
                .map_err(reqwest::Error::without_url)?
        }
        signal = tokio::signal::ctrl_c() => { signal?; bail!("Download cancelled"); }
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if content_type.contains("text/html") || content_type.contains("application/json") {
        bail!("Host returned a web page instead of video");
    }
    let prefix = tokio::select! {
        result = crate::network::peek(&mut response) => result?,
        signal = tokio::signal::ctrl_c() => { signal?; bail!("Download cancelled"); }
    };
    let hls = crate::network::hls_prefix(&prefix)
        || response.url().path().ends_with(".m3u8")
        || content_type.to_ascii_lowercase().contains("mpegurl")
        || source.label == "HLS";
    if hls {
        drop(response);
        let format = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("mp4") => "mp4",
            Some("mkv") => "matroska",
            _ => bail!("HLS downloads require an .mp4 or .mkv destination"),
        };
        download_hls(client, source, temporary.path(), format).await?;
    } else {
        tokio::select! {
            result = download_file(response, prefix, temporary.reopen()?) => result?,
            signal = tokio::signal::ctrl_c() => { signal?; bail!("Download cancelled"); }
        }
    }
    if temporary.as_file().metadata()?.len() == 0 {
        bail!("Host returned an empty download");
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .context("Cannot save download; destination may have appeared while downloading")?;
    Ok(())
}

async fn download_file(
    mut response: reqwest::Response,
    prefix: Vec<u8>,
    file: std::fs::File,
) -> Result<()> {
    let expected = response
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<u64>().ok());
    let mut file = tokio::fs::File::from_std(file);
    file.write_all(&prefix).await?;
    let mut bytes = prefix.len() as u64;
    let mut last_update = Instant::now();
    eprintln!("Downloading video…");
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        file.write_all(&chunk).await?;
        bytes += chunk.len() as u64;
        if last_update.elapsed() >= Duration::from_secs(1) {
            eprintln!("Downloaded {:.1} MiB", bytes as f64 / 1_048_576.0);
            last_update = Instant::now();
        }
    }
    if expected.is_some_and(|size| size != bytes) {
        bail!("Video transfer ended before the complete file arrived");
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn download_hls(client: Client, source: &Source, path: &Path, format: &str) -> Result<()> {
    let relay = relay::start(client, source).await?;
    eprintln!("Downloading HLS with ffmpeg…");
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-progress",
        "pipe:1",
        "-stats_period",
        "1",
        "-protocol_whitelist",
        "http,tcp,crypto",
        "-format_whitelist",
        "hls,mpegts,mov,aac,mp3",
        "-allowed_extensions",
        "ALL",
        "-i",
        &relay.url,
        "-map",
        "0:v:0?",
        "-map",
        "0:a?",
        "-c",
        "copy",
    ]);
    if format == "mp4" {
        command.args(["-movflags", "+faststart"]);
    }
    command
        .args(["-f", format])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("HLS downloads require ffmpeg in PATH")?;
    let stdout = child.stdout.take().context("Cannot read ffmpeg progress")?;
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(bytes) = line
                .strip_prefix("total_size=")
                .and_then(|v| v.parse::<u64>().ok())
            {
                eprintln!("Downloaded {:.1} MiB", bytes as f64 / 1_048_576.0);
            }
        }
        Ok::<_, std::io::Error>(())
    });
    let mut stderr = child.stderr.take().context("Cannot read ffmpeg errors")?;
    // Drain stderr continuously so ffmpeg cannot block on a full pipe. Keep a
    // bounded diagnostic, sanitizing remote text before printing it.
    let diagnostics = tokio::spawn(async move {
        let mut output = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let count = stderr.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            let keep = count.min(8192usize.saturating_sub(output.len()));
            output.extend_from_slice(&buffer[..keep]);
        }
        Ok::<_, std::io::Error>(output)
    });
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        signal = tokio::signal::ctrl_c() => { signal?; child.kill().await?; None }
    };
    let output = diagnostics.await??;
    progress.await??;
    let Some(status) = status else {
        bail!("Download cancelled");
    };
    if !status.success() || !output.is_empty() {
        let prefix = relay
            .url
            .rsplit_once("/0/")
            .map(|(prefix, _)| prefix)
            .unwrap_or(&relay.url);
        let diagnostic =
            terminal::text(&String::from_utf8_lossy(&output)).replace(prefix, "local relay");
        bail!(
            "ffmpeg failed ({status}): {diagnostic}. Try an .mkv destination if codecs cannot fit in MP4"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    fn source(url: String) -> Source {
        Source {
            url,
            label: "video".into(),
            referer: "https://host.test/embed".into(),
            provider: "test".into(),
        }
    }

    #[tokio::test]
    async fn direct_download_preserves_bytes_and_refuses_overwrite() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let upstream = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/video",
                    get(|headers: axum::http::HeaderMap| async move {
                        assert_eq!(headers["referer"], "https://host.test/embed");
                        ([("content-type", "video/mp4")], "complete video fixture")
                    }),
                ),
            )
            .into_future(),
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.mp4");
        save(Client::for_test(), &source(url.clone()), &path)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete video fixture");
        assert!(save(Client::for_test(), &source(url), &path).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"complete video fixture");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        upstream.abort();
    }

    #[tokio::test]
    async fn failed_download_removes_partial_file() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = socket.read(&mut request).await.unwrap();
            assert!(count > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial").await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.mp4");
        assert!(save(Client::for_test(), &source(url), &path).await.is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_download_removes_temporary_file() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let requested = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify = requested.clone();
        let upstream = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/video",
                    get(move || {
                        let notify = notify.clone();
                        async move {
                            notify.notify_one();
                            axum::body::Body::from_stream(futures_util::stream::pending::<
                                Result<Vec<u8>, std::io::Error>,
                            >())
                        }
                    }),
                ),
            )
            .into_future(),
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.mp4");
        let destination = path.clone();
        let task =
            tokio::spawn(async move { save(Client::for_test(), &source(url), &destination).await });
        requested.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        upstream.abort();
    }

    #[tokio::test]
    async fn hls_download_remuxes_through_relay() {
        // The Linux CI job installs ffmpeg. Other platforms still exercise
        // direct downloads; skip this fixture if ffmpeg is not installed.
        let fixture = match std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=32x32:r=5",
                "-t",
                "2",
                "-c:v",
                "mpeg2video",
                "-f",
                "mpegts",
                "pipe:1",
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Skipping HLS fixture: ffmpeg not installed");
                return;
            }
            Err(error) => panic!("Cannot run ffmpeg: {error}"),
        };
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/playlist", listener.local_addr().unwrap());
        let upstream = tokio::spawn(axum::serve(listener, Router::new()
            // Deliberately omit a playlist extension and use the wrong MIME
            // type to check that sniffed playlists still go through the relay.
            .route("/playlist", get(|| async { ([("content-type", "application/octet-stream")], "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2,\nsegment.ts\n#EXT-X-ENDLIST\n") }))
            .route("/segment.ts", get(move |headers: axum::http::HeaderMap| {
                let data = fixture.stdout.clone();
                async move {
                    assert_eq!(headers["referer"], "https://host.test/embed");
                    ([("content-type", "video/mp2t")], data)
                }
            }))
        ).into_future());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.mp4");
        save(Client::for_test(), &source(url), &path).await.unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 100);
        assert_eq!(&bytes[4..8], b"ftyp");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        upstream.abort();
    }
}
