use crate::api::client::ILinkClient;
use crate::api::types::*;
use crate::auth::accounts::{list_indexed_account_ids, load_account};

pub async fn run(to: &str, text: &str) -> Result<(), String> {
    let ids = list_indexed_account_ids();
    if ids.is_empty() {
        return Err("no accounts configured — run `weclawbot login` first".into());
    }

    let (account_id, account) = ids
        .iter()
        .find_map(|id| load_account(id).map(|a| (id.clone(), a)))
        .ok_or("no valid account found")?;

    let token = account
        .token
        .filter(|t| !t.is_empty())
        .ok_or(format!("account {account_id} has no token"))?;

    let base_url = account
        .base_url
        .as_deref()
        .unwrap_or(crate::api::client::default_base_url());

    let msg = WeixinMessage {
        to_user_id: Some(to.to_string()),
        message_type: Some(MESSAGE_TYPE_BOT),
        message_state: Some(MESSAGE_STATE_FINISH),
        item_list: Some(vec![MessageItem {
            item_type: Some(MESSAGE_ITEM_TYPE_TEXT),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let client = ILinkClient::new();
    client.send_message(base_url, &token, msg).await?;
    println!("Message sent to {to}");
    Ok(())
}
