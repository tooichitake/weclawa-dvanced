//! "对方正在输入" — the WeChat typing indicator for AI reply paths.
//!
//! iLink's `sendTyping` expires after a few seconds, so we have to pulse
//! periodically while the AI provider is generating. The pulse loop lives
//! in a background tokio task that the handler owns via [`TypingHandle`];
//! dropping the handle (or calling `.stop()`) ends the loop.
//!
//! ## Why this lives in `monitor` rather than `ai`
//!
//! It's a function of the iLink protocol (typing pulses + per-user
//! `typing_ticket` fetched from `get_config`), not of the AI provider. The
//! handler starts typing BEFORE `resolve_message` so the WeChat client sees
//! the indicator within ~100 ms of inbound — earlier than the AI subprocess
//! could ever start.
//!
//! ## Typing ticket cache
//!
//! iLink rejects (silently drops) typing pulses without a valid
//! `typing_ticket`. The ticket is per-user and fetched via `get_config`
//! once on the first message; subsequent messages for the same user reuse
//! the cached value.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{oneshot, Mutex};
use tracing::debug;

use crate::api::client::ILinkClient;

/// Per-message context passed to the pulse loop. Holds everything the
/// `sendTyping` HTTP call needs.
#[derive(Clone)]
pub struct TypingContext {
    pub client: Arc<ILinkClient>,
    pub base_url: String,
    pub token: String,
    pub user_id: String,
    /// Lifted from `WeixinMessage.context_token` of the inbound. Required
    /// by iLink's `get_config` to mint a `typing_ticket`.
    pub context_token: Option<String>,
}

/// Handle returned by [`start_typing_pulse`]. Dropping it (or calling
/// `.stop()`) ends the pulse loop.
pub struct TypingHandle {
    stop: Option<oneshot::Sender<()>>,
}

impl TypingHandle {
    pub fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for TypingHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn the pulse loop in a tokio task. First pulse fires within ~100 ms
/// of return (after `get_config` for ticket on the first call).
pub fn start(ctx: TypingContext) -> TypingHandle {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(pulse_loop(ctx, rx));
    TypingHandle { stop: Some(tx) }
}

// --- internals ---

static TYPING_TICKET_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn ticket_cache() -> &'static Mutex<HashMap<String, String>> {
    TYPING_TICKET_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn ensure_ticket(ctx: &TypingContext) -> Option<String> {
    // Smoke harness uses fake accounts; skip the network call entirely.
    if crate::service::test_inject::test_mode_enabled() {
        return None;
    }

    {
        let cache = ticket_cache().lock().await;
        if let Some(t) = cache.get(&ctx.user_id) {
            return Some(t.clone());
        }
    }

    match ctx
        .client
        .get_config(
            &ctx.base_url,
            &ctx.token,
            &ctx.user_id,
            ctx.context_token.as_deref(),
        )
        .await
    {
        Ok(resp) => {
            let ticket = resp.typing_ticket.filter(|s| !s.is_empty());
            if let Some(t) = ticket.as_ref() {
                let mut cache = ticket_cache().lock().await;
                cache.insert(ctx.user_id.clone(), t.clone());
            }
            ticket
        }
        Err(e) => {
            debug!("get_config for typing ticket failed (non-fatal): {e}");
            None
        }
    }
}

async fn pulse_loop(ctx: TypingContext, mut stop: oneshot::Receiver<()>) {
    let ticket = ensure_ticket(&ctx).await;
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = interval.tick() => {
                if let Err(e) = ctx
                    .client
                    .send_typing(&ctx.base_url, &ctx.token, &ctx.user_id, ticket.as_deref())
                    .await
                {
                    debug!("typing pulse failed (non-fatal): {e}");
                }
            }
        }
    }
}
