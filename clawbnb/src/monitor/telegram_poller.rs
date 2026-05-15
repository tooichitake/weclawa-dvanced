//! Telegram bot poll loop — v5 M3.
//!
//! Telegram getUpdates 长轮询：bot.poll_updates(token, _, offset) 返回
//! 新 Update 数组 + 把 offset 推进到最后一个 update_id+1。
//!
//! ## 与 iLink poller 的差异
//!
//! - **endpoint**：`https://api.telegram.org/bot<token>/getUpdates`（已经在
//!   [`crate::puppet::telegram::TelegramBot`] 写死），不用 base_url
//! - **offset 状态**：用 `storage::sync_buf` 持久化 last update_id
//! - **dispatch**：每条 Update → [`crate::puppet::telegram_handler::handle_inbound_update`]
//!
//! ## 简化点（v5 范围内）
//!
//! v5 用 `bot.poll_updates` 当前的 stub return（empty GetUpdatesResp）。
//! 真正的 Update 流式 mapping 在 [`crate::puppet::telegram::TelegramBot::poll_updates`]
//! 真填后会自动 work。本 poller 在那之前是"占位骨架"——每 25s 调一次
//! getUpdates，目前返空就 sleep 25s 继续。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::auth::accounts::load_account;
use crate::puppet::telegram::TelegramBot;
use crate::puppet::MessagingPlatform;
use crate::storage::sync_buf::{load_sync_buf, save_sync_buf};

const FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// Spawn 一个 Telegram bot 的 long-poll loop。每次 getUpdates 返回非空
/// Update 时调 telegram_handler::handle_inbound_update 处理。
pub async fn run_telegram_monitor(
    bot: Arc<TelegramBot>,
    account_id: String,
    mut shutdown: watch::Receiver<bool>,
) {
    info!("[{account_id}] starting telegram monitor");

    let account = match load_account(&account_id) {
        Some(a) => a,
        None => {
            error!("[{account_id}] account not found");
            return;
        }
    };

    let token = match &account.token {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            error!("[{account_id}] telegram token missing or empty");
            return;
        }
    };

    let mut offset = load_sync_buf(&account_id).unwrap_or_default();
    if offset.is_empty() {
        offset = "0".to_string();
    }

    loop {
        if *shutdown.borrow() {
            info!("[{account_id}] telegram monitor shutting down");
            return;
        }

        // tokio::select on shutdown so we don't block the 25s poll past drain.
        tokio::select! {
            _ = shutdown.changed() => {
                info!("[{account_id}] telegram monitor shutdown received");
                return;
            }
            r = bot.poll_updates(&token, "", &offset) => {
                match r {
                    Ok(_resp) => {
                        // v5: TelegramBot::poll_updates 当前返 stub default()
                        // ── 真 Update parsing 在 puppet::telegram 真填后 yield 给
                        // handle_inbound_update。本 loop 形态就位。
                        // 下次 poll 之间留一下，否则空 poll 飙 CPU。
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        warn!("[{account_id}] telegram poll: {e} (back off {:?})", FAILURE_BACKOFF);
                        tokio::time::sleep(FAILURE_BACKOFF).await;
                    }
                }
            }
        }

        let _ = save_sync_buf(&account_id, &offset);
    }
}
