//! Telegram bot poll loop — v7.0 H1 (now real).
//!
//! Telegram getUpdates 长轮询：每轮调 `TelegramBot::poll_telegram_updates`
//! 拿 `(Vec<Update>, new_offset)`，逐条 dispatch 给
//! `puppet::telegram_handler::handle_inbound_update`，最后把 offset 持久化
//! 进 `storage::sync_buf` 以便重启后从断点继续。
//!
//! ## 与 iLink poller 的差异
//!
//! - **endpoint**：`https://api.telegram.org/bot<token>/getUpdates`，由
//!   [`crate::puppet::telegram::TelegramBot`] 内部固定，参数 `base_url` 忽略
//! - **offset 语义**：last `update_id` + 1。第一次调用传 0 表示 "从最旧
//!   待传送的 update 开始"。Telegram 在我们 ack（下一次带 offset > N）
//!   之后丢掉它的 N 及更小的缓冲——丢消息可能源于此，所以 offset 必须
//!   在 dispatch 之前持久化才安全。
//! - **空响应**：long-poll 25s 内没事件 → 返 `(Vec::new(), 不变 offset)`。
//!   loop 不 sleep，立刻进下一轮。
//! - **错误**：失败 → backoff 30s 再试，连续失败不会"自杀"——telegram
//!   bot 没有 redeliver 概念，poll 必须保持活着。
//!
//! ## 不在本期做的
//!
//! - **Webhook 模式**作为 long-poll 的替代（Telegram 推 events 进
//!   `POST /api/v1/puppet/telegram/webhook/<id>`）。生产部署有 reverse-proxy
//!   时更省资源，但 long-poll 不依赖 inbound 网络可达性，调试更友好。
//! - **dedup gate 前置**到 offset 持久化之前——目前 handler 内部走
//!   `dedup_async::mark_seen_scoped`，daemon 重启 + Telegram redeliver
//!   能识别重复。但 offset 已经 ack 不重新发了，所以这不是问题；只有
//!   "handler 处理一半 daemon 崩"才会丢，跟 iLink 一致行为。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::auth::accounts::load_account;
use crate::puppet::telegram::TelegramBot;
use crate::storage::sync_buf::{load_sync_buf, save_sync_buf};

const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// Telegram long-poll 上限。Bot API 文档：50s 以下都行，建议 25-30s
/// 给 reqwest 30s connection timeout 留 buffer。
const LONG_POLL_TIMEOUT_SECS: u32 = 25;

/// Spawn 一个 Telegram bot 的 long-poll loop。每次 getUpdates 返回非空
/// Update 时为每条调 [`crate::puppet::telegram_handler::handle_inbound_update`]
/// 处理。
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

    // offset 从 sync_buf 取（重启续传）。空 / 非数字 → 0 表示从头拉
    // 所有待传送 update。
    let mut offset: i64 = load_sync_buf(&account_id)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    info!("[{account_id}] telegram offset start = {offset}");

    loop {
        if *shutdown.borrow() {
            info!("[{account_id}] telegram monitor shutting down");
            return;
        }

        tokio::select! {
            _ = shutdown.changed() => {
                info!("[{account_id}] telegram monitor shutdown received");
                return;
            }
            r = bot.poll_telegram_updates(&token, offset, LONG_POLL_TIMEOUT_SECS) => {
                match r {
                    Ok((updates, new_offset)) => {
                        if updates.is_empty() {
                            // long-poll timed out with no events — loop
                            // immediately, telegram is happy to be called
                            // again right away (it just sat idle 25s).
                            continue;
                        }

                        // Persist offset BEFORE dispatch so that if a
                        // handler crashes mid-processing, restart resumes
                        // *past* the crashed event (we'd rather drop a
                        // message than infinite-loop on one that panics).
                        // dedup_async at the handler layer catches genuine
                        // duplicates if telegram redelivers due to
                        // network blips before our ack lands.
                        offset = new_offset;
                        // `save_sync_buf` swallows fs errors internally
                        // (best-effort) — already noisy via the file's
                        // own tracing.
                        save_sync_buf(&account_id, &offset.to_string());

                        let count = updates.len();
                        for update in updates {
                            let bot_clone = Arc::clone(&bot);
                            let token_clone = token.clone();
                            let acct_clone = account_id.clone();
                            // Spawn each dispatch so a slow handler can't
                            // back-pressure the poll loop — telegram has
                            // strict redelivery semantics tied to offset
                            // ack, we already ack'd.
                            tokio::spawn(async move {
                                crate::puppet::telegram_handler::handle_inbound_update(
                                    bot_clone, &update, &acct_clone, &token_clone,
                                )
                                .await;
                            });
                        }
                        info!("[{account_id}] telegram dispatched {count} update(s), offset → {offset}");
                    }
                    Err(e) => {
                        warn!(
                            "[{account_id}] telegram poll: {e} (back off {:?})",
                            FAILURE_BACKOFF
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(FAILURE_BACKOFF) => {}
                            _ = shutdown.changed() => {
                                info!("[{account_id}] telegram monitor shutdown during backoff");
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}
