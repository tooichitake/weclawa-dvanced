use std::env;
use std::fs;
use std::path::PathBuf;

const REPO: &str = "anthropics/weclawa-advanced";

pub async fn run() -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    println!("weclawbot {current} — checking for updates...");

    let (tag, download_url) = fetch_latest_release().await?;
    let remote_version = tag.strip_prefix("weclawbot-v").unwrap_or(&tag);

    if remote_version == current {
        println!("Already up to date.");
        return Ok(());
    }

    println!("New version available: {remote_version}");
    println!("Downloading...");

    let exe_path = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    download_and_replace(&download_url, &exe_path).await?;

    println!("Updated to {remote_version}. Restart weclawbot to use the new version.");
    Ok(())
}

async fn fetch_latest_release() -> Result<(String, String), String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .get(&url)
        .header("User-Agent", "weclawbot-updater")
        .send()
        .await
        .map_err(|e| format!("fetch release: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse release: {e}"))?;

    let tag = resp["tag_name"]
        .as_str()
        .ok_or("no tag_name in release")?
        .to_string();

    let asset_name = platform_asset_name();
    let assets = resp["assets"]
        .as_array()
        .ok_or("no assets in release")?;

    let download_url = assets
        .iter()
        .find_map(|a| {
            let name = a["name"].as_str()?;
            if name == asset_name {
                a["browser_download_url"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .ok_or(format!("no matching asset '{asset_name}' in release {tag}"))?;

    Ok((tag, download_url))
}

fn platform_asset_name() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    { "weclawbot-windows-x86_64.exe" }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    { "weclawbot-windows-aarch64.exe" }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    { "weclawbot-linux-x86_64" }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    { "weclawbot-linux-aarch64" }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    { "weclawbot-darwin-x86_64" }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    { "weclawbot-darwin-aarch64" }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    { "weclawbot-unknown" }
}

async fn download_and_replace(url: &str, exe_path: &PathBuf) -> Result<(), String> {
    let client = reqwest::Client::new();
    let bytes = client
        .get(url)
        .header("User-Agent", "weclawbot-updater")
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    let tmp = exe_path.with_extension("tmp");
    fs::write(&tmp, &bytes).map_err(|e| format!("write tmp: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod: {e}"))?;
    }

    // On Windows, rename the running exe first, then move new one in
    #[cfg(windows)]
    {
        let old = exe_path.with_extension("old.exe");
        let _ = fs::remove_file(&old);
        fs::rename(exe_path, &old).map_err(|e| format!("rename old: {e}"))?;
        if let Err(e) = fs::rename(&tmp, exe_path) {
            let _ = fs::rename(&old, exe_path);
            return Err(format!("replace: {e}"));
        }
        let _ = fs::remove_file(&old);
    }

    #[cfg(not(windows))]
    {
        fs::rename(&tmp, exe_path).map_err(|e| format!("replace: {e}"))?;
    }

    Ok(())
}
