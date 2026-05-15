use crate::daemon::pid::{is_process_alive, read_pid, remove_pid};

pub async fn run() -> Result<(), String> {
    let pid = read_pid().ok_or("weclawbot is not running (no PID file)")?;

    if !is_process_alive(pid) {
        remove_pid();
        return Err(format!("stale PID file (pid {pid} is not running), cleaned up"));
    }

    kill_process(pid)?;
    remove_pid();
    println!("weclawbot stopped (pid {pid})");
    Ok(())
}

fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        unsafe {
            if libc::kill(pid as i32, libc::SIGTERM) != 0 {
                return Err(format!("kill({pid}): {}", std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .map_err(|e| format!("taskkill: {e}"))?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err("unsupported platform".into())
    }
}
