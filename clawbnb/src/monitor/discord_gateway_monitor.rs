//! Discord Gateway monitor — v5.3 (feature-gated `discord-gateway`).
//!
//! 把 [`crate::puppet::discord_gateway::connect_gateway`] yield 的
//! `GatewayEvent::Message` 转发到 [`crate::puppet::discord_handler::handle_inbound_message`]。
//!
//! ## Lifecycle
//!
//! - daemon 启动时 per-Discord-account 起一个 task 跑 [`run_discord_monitor`]
//! - WebSocket 断线 → gateway 内部 yield `Disconnected`，本 loop 把它升
//!   级成 reconnect（v5.4 加 resume + session_id 续传；当前重连 = 重 IDENTIFY）
//! - daemon shutdown 触发 `shutdown_rx` change → 退 loop

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::puppet::discord::DiscordBot;
use crate::puppet::discord_gateway::{connect_gateway, GatewayEvent};

const RECONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
/// Discord Gateway intents bitmask — minimum needed to receive guild +
/// DM message_create events. Reference:
/// <https://discord.com/developers/docs/topics/gateway#gateway-intents>
///
/// - GUILD_MESSAGES (1 << 9) = 512
/// - DIRECT_MESSAGES (1 << 12) = 4096
/// - MESSAGE_CONTENT (1 << 15) = 32768 (privileged — must be enabled in dev portal)
const DEFAULT_INTENTS: u32 = 512 | 4096 | 32768;

pub async fn run_discord_monitor(
    bot: Arc<DiscordBot>,
    account_id: String,
    token: String,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!("[{account_id}] discord gateway monitor starting");

    loop {
        if *shutdown_rx.borrow() {
            info!("[{account_id}] discord monitor shutdown signal — exit");
            return;
        }

        let mut rx = connect_gateway(token.clone(), DEFAULT_INTENTS);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("[{account_id}] discord monitor shutdown — closing gateway");
                        return;
                    }
                }
                event = rx.recv() => {
                    match event {
                        Some(GatewayEvent::Message(msg)) => {
                            let bot = Arc::clone(&bot);
                            let acct = account_id.clone();
                            let tok = token.clone();
                            tokio::spawn(async move {
                                crate::puppet::discord_handler::handle_inbound_message(
                                    bot, &msg, &acct, &tok,
                                )
                                .await;
                            });
                        }
                        Some(GatewayEvent::Disconnected(reason)) => {
                            warn!(
                                "[{account_id}] discord gateway disconnected: {reason} — reconnecting in {}s",
                                RECONNECT_BACKOFF.as_secs()
                            );
                            break;
                        }
                        None => {
                            warn!(
                                "[{account_id}] discord gateway channel closed — reconnecting in {}s",
                                RECONNECT_BACKOFF.as_secs()
                            );
                            break;
                        }
                    }
                }
            }
        }

        // backoff before reconnect
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}
