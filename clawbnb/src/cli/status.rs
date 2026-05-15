use crate::auth::accounts::list_indexed_account_ids;
use crate::daemon::pid::{is_process_alive, read_pid};

pub async fn run() -> Result<(), String> {
    let accounts = list_indexed_account_ids();

    match read_pid() {
        Some(pid) if is_process_alive(pid) => {
            println!("weclawbot is running (pid {pid})");
            println!("  accounts: {}", accounts.len());
            println!("  version:  {}", env!("CARGO_PKG_VERSION"));
        }
        Some(pid) => {
            println!("weclawbot is not running (stale pid {pid})");
            println!("  accounts: {}", accounts.len());
        }
        None => {
            println!("weclawbot is not running");
            println!("  accounts: {}", accounts.len());
        }
    }
    Ok(())
}
