use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn, error};

use crate::api::client::ILinkClient;
use crate::api::types::SESSION_EXPIRED_ERRCODE;
use crate::auth::accounts::load_account;
use crate::storage::sync_buf::{load_sync_buf, save_sync_buf};

use super::handler::handle_inbound_message;

const MAX_CONSECUTIVE_FAILURES: usize = 3;
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const SESSION_PAUSE: Duration = Duration::from_secs(3600);

pub async fn run_account_monitor(
    client: Arc<ILinkClient>,
    account_id: String,
    mut shutdown: watch::Receiver<bool>,
) {
    info!("[{account_id}] Starting monitor");

    let account = match load_account(&account_id) {
        Some(a) => a,
        None => {
            error!("[{account_id}] Account not found");
            return;
        }
    };

    let token = match &account.token {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            error!("[{account_id}] No valid token");
            return;
        }
    };

    let base_url = account
        .base_url
        .as_deref()
        .unwrap_or(crate::api::client::default_base_url())
        .to_string();

    let mut buf = load_sync_buf(&account_id).unwrap_or_default();
    let mut failures: usize = 0;

    info!("[{account_id}] Monitor ready, starting long-poll loop");

    loop {
        if *shutdown.borrow() {
            info!("[{account_id}] Shutdown signal, stopping monitor");
            break;
        }

        let result = client
            .get_updates(&base_url, &token, &buf, None)
            .await;

        match result {
            Ok(resp) => {
                failures = 0;

                let errcode = resp.errcode.unwrap_or(0);
                let ret = resp.ret.unwrap_or(0);

                if errcode == SESSION_EXPIRED_ERRCODE {
                    warn!("[{account_id}] Session expired (errcode={errcode}), pausing for 1 hour");
                    tokio::select! {
                        _ = tokio::time::sleep(SESSION_PAUSE) => {}
                        _ = shutdown.changed() => { break; }
                    }
                    continue;
                }

                if ret != 0 || (errcode != 0 && errcode != SESSION_EXPIRED_ERRCODE) {
                    warn!(
                        "[{account_id}] getUpdates error ret={ret} errcode={errcode} msg={:?}",
                        resp.errmsg
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = shutdown.changed() => { break; }
                    }
                    continue;
                }

                if let Some(new_buf) = &resp.get_updates_buf {
                    if !new_buf.is_empty() && *new_buf != buf {
                        buf = new_buf.clone();
                        save_sync_buf(&account_id, &buf);
                    }
                }

                if let Some(msgs) = resp.msgs {
                    // v7.6 — resolve tenant once per batch (not per
                    // message) since all messages in a `getUpdates`
                    // response belong to the same account/tenant.
                    let tenant =
                        crate::tenancy::resolver::resolve_tenant_for_account(&account_id);
                    for msg in msgs {
                        // Phase 4 + v7.6: inbound metric. account_id +
                        // tenant_id labels are bounded by # of bot
                        // accounts × # of paying tenants (single
                        // digits × low hundreds), safe cardinality.
                        // tenant_id is what the SLA aggregator needs
                        // to compute per-tenant uptime via PromQL.
                        metrics::counter!(
                            "weclawbot_inbound_messages_total",
                            "account_id" => account_id.clone(),
                            "tenant_id" => tenant.as_str().to_string(),
                            "platform" => "ilink-wechat",
                            "status" => "received"
                        )
                        .increment(1);
                        handle_inbound_message(&client, &account_id, &token, &base_url, &msg)
                            .await;
                    }
                }
            }
            Err(e) => {
                failures += 1;
                warn!("[{account_id}] getUpdates failed ({failures}/{MAX_CONSECUTIVE_FAILURES}): {e}");
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!("[{account_id}] Too many failures, backing off {FAILURE_BACKOFF:?}");
                    tokio::select! {
                        _ = tokio::time::sleep(FAILURE_BACKOFF) => {}
                        _ = shutdown.changed() => { break; }
                    }
                    failures = 0;
                }
            }
        }
    }

    info!("[{account_id}] Monitor stopped");
}
