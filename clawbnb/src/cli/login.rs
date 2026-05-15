use crate::api::client::ILinkClient;
use crate::auth::accounts::{normalize_account_id, register_account_id, save_account};
use crate::auth::qr_login::interactive_qr_login;
use crate::storage::state_dir::ensure_dirs;

pub async fn run() -> Result<(), String> {
    ensure_dirs().map_err(|e| format!("init dirs: {e}"))?;

    let client = ILinkClient::new();
    let result = interactive_qr_login(&client).await?;

    if result.already_connected {
        println!("{}", result.message);
        return Ok(());
    }

    if !result.connected {
        return Err(result.message);
    }

    let account_id = result
        .account_id
        .as_deref()
        .map(normalize_account_id)
        .ok_or("server did not return account_id")?;

    save_account(
        &account_id,
        result.bot_token.as_deref(),
        result.base_url.as_deref(),
        result.user_id.as_deref(),
    );
    register_account_id(&account_id);

    println!("Account saved: {account_id}");
    Ok(())
}
