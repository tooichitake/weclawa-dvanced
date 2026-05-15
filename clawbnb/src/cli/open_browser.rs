use crate::daemon::pid::{is_process_alive, read_pid};

pub async fn run(bind: &str, port: u16) -> Result<(), String> {
    let url = format!("http://{bind}:{port}");

    // Auto-start if not running
    if !read_pid().is_some_and(|p| is_process_alive(p)) {
        println!("weclawbot is not running, starting...");
        super::start::run(false, bind, port).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!("Opening console: {url}");
    open_browser(&url)?;
    Ok(())
}

fn open_browser(url: &str) -> Result<(), String> {
    // Each entry: (program, args-before-url). We try them in order and stop on
    // the first one that spawns successfully. The URL is appended last.
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &[&str])] = &[("cmd", &["/C", "start", ""])];

    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("open", &[])];

    // Linux + WSL launcher order, picked so each entry behaves correctly:
    //   1. wslview    — wslu's URL handler, designed for this exact case.
    //   2. xdg-open   — standard freedesktop on real Linux DEs.
    //   3. powershell.exe Start-Process — WSL path. Unlike `cmd /c start`,
    //      PowerShell tolerates a UNC working directory silently; the user
    //      sees no "UNC paths are not supported" warning.
    //   4. explorer.exe URL — last resort: Windows shell handles URLs and
    //      also tolerates UNC CWD silently.
    // `cmd.exe /C start` is intentionally NOT in this list — it always warns
    // about UNC working directories when invoked from a WSL filesystem path.
    #[cfg(target_os = "linux")]
    let candidates: &[(&str, &[&str])] = &[
        ("wslview", &[]),
        ("xdg-open", &[]),
        (
            "powershell.exe",
            &["-NoProfile", "-WindowStyle", "Hidden", "-Command", "Start-Process"],
        ),
        ("explorer.exe", &[]),
    ];

    let mut last_err: Option<String> = None;
    for (prog, pre_args) in candidates {
        let mut cmd = std::process::Command::new(prog);
        for a in *pre_args {
            cmd.arg(a);
        }
        cmd.arg(url);
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(format!("{prog}: {e}"));
            }
        }
    }

    // Couldn't launch anything — surface the URL and keep going.
    eprintln!(
        "Could not auto-open a browser (last error: {}). \
         Please open this URL manually:\n  {url}",
        last_err.as_deref().unwrap_or("no launcher available")
    );
    Ok(())
}
