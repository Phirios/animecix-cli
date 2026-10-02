use anyhow::{Context, Result, bail};
use std::{path::Path, process::Command};

pub fn launch(playlist: &Path, player: Option<&str>, stream_url: &str) -> Result<()> {
    if let Some(player) = player {
        #[cfg(target_os = "macos")]
        if player.ends_with(".app") || player.starts_with('/') && Path::new(player).is_dir() {
            return checked(Command::new("open").args(["-a", player]).arg(
                if player.contains("QuickTime") {
                    std::ffi::OsStr::new(stream_url)
                } else {
                    playlist.as_os_str()
                },
            ));
        }
        let mut child = Command::new(player).arg(playlist).stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().with_context(|| format!("Cannot start player {player}; pass its executable path or app name with --player"))?;
        // Reap executable players without blocking the streaming relay.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("osascript").args(["-l", "JavaScript", "-e",
            "ObjC.import('CoreServices'); ObjC.import('AppKit'); ObjC.bindFunction('LSCopyDefaultRoleHandlerForContentType', ['id', ['id', 'unsigned int']]); var b = $.LSCopyDefaultRoleHandlerForContentType('public.mpeg-4', 2); var a = b ? $.NSWorkspace.sharedWorkspace.URLForApplicationWithBundleIdentifier(b) : null; a ? ObjC.unwrap(a.path) : '';"])
            .output().context("Cannot query the default video player")?;
        let app = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || app.is_empty() {
            bail!(
                "No default MP4 player found. Set one in Finder → Get Info → Open with, or use --player /Applications/VLC.app"
            );
        }
        eprintln!(
            "Opening {}",
            Path::new(&app)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let target = if app.contains("QuickTime Player.app") {
            std::ffi::OsStr::new(stream_url)
        } else {
            playlist.as_os_str()
        };
        checked(Command::new("open").args(["-a", &app]).arg(target))
    }
    #[cfg(target_os = "linux")]
    {
        let _ = stream_url;
        let output = Command::new("xdg-mime")
            .args(["query", "default", "video/mp4"])
            .output()?;
        let desktop = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if desktop.is_empty() {
            bail!("No default MP4 player. Configure one or use --player vlc");
        }
        checked(
            Command::new("gtk-launch")
                .arg(desktop.trim_end_matches(".desktop"))
                .arg(playlist),
        )
        .context("Could not launch the default desktop player; use --player vlc or --player mpv")
    }
    #[cfg(target_os = "windows")]
    {
        let _ = stream_url;
        // Default M3U file association on Windows; VLC registers this when installed.
        checked(Command::new("powershell").args([
            "-NoProfile",
            "-Command",
            "& { param($p) Start-Process -FilePath $p }",
            &playlist.to_string_lossy(),
        ]))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (playlist, stream_url);
        bail!("Use --player with your video player's executable path on this platform")
    }
}

fn checked(command: &mut Command) -> Result<()> {
    let status = command.status().context("Failed to launch video player")?;
    if !status.success() {
        bail!("Player launcher exited with {status}");
    }
    Ok(())
}
