//! Discord Gateway WebSocket connection — v5.2 O3.
//!
//! Discord 不 long-poll；推 events 走 WebSocket Gateway。本模块管理：
//! - WS 连接 (`wss://gateway.discord.gg/?v=10&encoding=json`)
//! - heartbeat 循环（按 Hello payload 的 `heartbeat_interval`）
//! - Identify payload + intents
//! - dispatch loop：解 MESSAGE_CREATE event → `discord_inbound::message_to_common`
//!   → `discord_handler::handle_inbound` (TODO v5.3)
//! - resume / reconnect 流程 (sequence + session_id 跟踪)
//!
//! ## Feature gating
//!
//! 仅 `--features discord-gateway` 编译进 binary，因为 tokio-tungstenite +
//! futures 加 ~500KB。运营商若不用 Discord 走 default build，体积不变。
//!
//! ## v5.2 实施范围
//!
//! 连接 + heartbeat + 接收 dispatch event 主循环 + emit
//! `DiscordEvent::Message(DiscordMessage)` 给 caller。完整 resume +
//! sharding 留 v5.3。

#![cfg(feature = "discord-gateway")]

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::puppet::discord_inbound::DiscordMessage;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Gateway opcodes (Discord Gateway protocol v10)
const OP_DISPATCH: u8 = 0;
const OP_HEARTBEAT: u8 = 1;
const OP_IDENTIFY: u8 = 2;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

/// 暴露给 caller 的高层 event 抽象。
#[derive(Debug)]
pub enum GatewayEvent {
    /// 新入站消息 (Discord MESSAGE_CREATE event payload)
    Message(DiscordMessage),
    /// connection dropped — caller decide reconnect 策略
    Disconnected(String),
}

/// 启动 Discord Gateway 连接，event 通过返回的 mpsc Receiver 推给 caller。
///
/// Returns a stream of `GatewayEvent`. Caller 通常起一个 task 消费它。
pub fn connect_gateway(
    token: String,
    intents: u32,
) -> mpsc::Receiver<GatewayEvent> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        if let Err(e) = run_gateway_loop(&token, intents, tx.clone()).await {
            error!("discord gateway loop exit: {e}");
            let _ = tx.send(GatewayEvent::Disconnected(e)).await;
        }
    });
    rx
}

async fn run_gateway_loop(
    token: &str,
    intents: u32,
    tx: mpsc::Sender<GatewayEvent>,
) -> Result<(), String> {
    info!("discord gateway connecting to {GATEWAY_URL}");
    let (ws_stream, _) = connect_async(GATEWAY_URL)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    // Step 1: receive HELLO + extract heartbeat_interval
    let hello = ws_stream
        .next()
        .await
        .ok_or("ws closed before HELLO")?
        .map_err(|e| format!("ws recv hello: {e}"))?;
    let hello_text = hello.into_text().map_err(|e| format!("hello to text: {e}"))?;
    #[derive(Deserialize)]
    struct HelloEnvelope {
        op: u8,
        d: HelloD,
    }
    #[derive(Deserialize)]
    struct HelloD {
        heartbeat_interval: u64,
    }
    let hello_env: HelloEnvelope =
        serde_json::from_str(&hello_text).map_err(|e| format!("parse hello: {e}"))?;
    if hello_env.op != OP_HELLO {
        return Err(format!("expected HELLO (op=10), got op={}", hello_env.op));
    }
    let heartbeat_ms = hello_env.d.heartbeat_interval;
    info!("discord gateway HELLO, heartbeat={heartbeat_ms}ms");

    // Step 2: send IDENTIFY
    let identify = json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "weclawbot",
                "device": "weclawbot",
            }
        }
    });
    ws_sink
        .send(Message::Text(identify.to_string()))
        .await
        .map_err(|e| format!("send identify: {e}"))?;

    // Step 3: heartbeat task (separate from receive loop)
    // Use Arc<Mutex<Option<i64>>> for last sequence to share with receive loop.
    let last_seq: std::sync::Arc<std::sync::Mutex<Option<i64>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let last_seq_for_hb = last_seq.clone();
    let (heartbeat_tx, mut heartbeat_rx) = mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let interval = Duration::from_millis(heartbeat_ms);
        loop {
            tokio::time::sleep(interval).await;
            if heartbeat_tx.send(()).await.is_err() {
                break; // receive loop closed
            }
        }
    });

    // Step 4: receive loop
    loop {
        tokio::select! {
            _ = heartbeat_rx.recv() => {
                let seq = *last_seq.lock().unwrap();
                let hb = json!({ "op": OP_HEARTBEAT, "d": seq });
                if let Err(e) = ws_sink.send(Message::Text(hb.to_string())).await {
                    return Err(format!("send heartbeat: {e}"));
                }
            }
            msg = ws_stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(format!("ws recv: {e}")),
                    None => return Err("ws stream ended".into()),
                };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(reason) => {
                        return Err(format!("ws close: {reason:?}"));
                    }
                    _ => continue, // ignore binary / ping / pong
                };
                #[derive(Deserialize)]
                struct Envelope {
                    op: u8,
                    #[serde(default)]
                    s: Option<i64>,
                    #[serde(default)]
                    t: Option<String>,
                    #[serde(default)]
                    d: serde_json::Value,
                }
                let env: Envelope = match serde_json::from_str(&text) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("discord gateway parse: {e}");
                        continue;
                    }
                };
                if let Some(s) = env.s {
                    *last_seq_for_hb.lock().unwrap() = Some(s);
                }
                match env.op {
                    OP_HEARTBEAT_ACK => {
                        // OK — connection alive
                    }
                    OP_DISPATCH => {
                        // event 名在 t 字段
                        if env.t.as_deref() == Some("MESSAGE_CREATE") {
                            match serde_json::from_value::<DiscordMessage>(env.d) {
                                Ok(m) => {
                                    if tx.send(GatewayEvent::Message(m)).await.is_err() {
                                        info!("discord gateway: caller dropped rx");
                                        return Ok(());
                                    }
                                }
                                Err(e) => warn!("MESSAGE_CREATE parse: {e}"),
                            }
                        }
                        // 其他 dispatch event (READY / GUILD_CREATE / 等) 当前忽略
                    }
                    OP_HEARTBEAT => {
                        // server-requested heartbeat
                        let seq = *last_seq_for_hb.lock().unwrap();
                        let hb = json!({ "op": OP_HEARTBEAT, "d": seq });
                        if let Err(e) = ws_sink.send(Message::Text(hb.to_string())).await {
                            return Err(format!("send hb-on-request: {e}"));
                        }
                    }
                    other => {
                        // op 7 (Reconnect) / 9 (Invalid Session) 等 v5.3 处理
                        tracing::debug!("discord gateway unhandled op={other}");
                    }
                }
            }
        }
    }
}
