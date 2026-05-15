use std::sync::Arc;

use tokio::sync::watch;
use tracing::info;

use crate::api::client::ILinkClient;
use crate::auth::accounts::list_indexed_account_ids;
use crate::daemon::log::setup_file_logging;
use crate::daemon::pid::{is_process_alive, read_pid, write_pid};
use crate::monitor::poller::run_account_monitor;
use crate::service::server::run_server;
use crate::storage::state_dir::ensure_dirs;

pub async fn run(foreground: bool, bind: &str, port: u16) -> Result<(), String> {
    ensure_dirs().map_err(|e| format!("init dirs: {e}"))?;
    let _ = crate::config::Config::ensure_exists();

    if let Some(pid) = read_pid() {
        if is_process_alive(pid) {
            return Err(format!("weclawbot is already running (pid {pid})"));
        }
    }

    if !foreground {
        return start_background(bind, port);
    }

    setup_file_logging();
    crate::daemon::panic_handler::install();
    write_pid(std::process::id());
    info!("weclawbot {} starting (foreground)", env!("CARGO_PKG_VERSION"));

    // Phase 1: open SQLite. Auto-run the legacy JSON → DB importer on
    // first boot (state.db doesn't yet exist + legacy files present).
    // The pool is created either way so later phases can swap callsites
    // over from `~/.weclawbot/users/<hash>/*.json` to repo APIs.
    // v4.2: sqlx async pool 是 daemon 唯一 DB 接口。rusqlite/refinery 全删，
    // sqlx 自己跑 migrations（含 refinery 兼容导入）。
    let _state_base = crate::storage::state_dir::state_dir().clone();
    let pool = match crate::storage::db_async::open_default().await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("open state.db (sqlx): {e}");
            crate::daemon::pid::remove_pid();
            return Err(format!("open state.db: {e}"));
        }
    };
    crate::storage::db_async::set_global_async_pool(pool.clone());
    info!("sqlx async pool initialized + migrations applied");

    // Phase 4: install the Prometheus exporter so subsequent
    // `metrics::counter!(...)` calls land in the registry.
    crate::observability::metrics::install();

    // v2.1.B1: 一次性 load config + 安装 ArcSwap 进程缓存 + 启 notify
    // watcher。从此 handler.rs / sysconfig.rs 都走 Config::cached()，
    // 不再 hot-path 同步 fs::read_to_string。文件 mtime 变了 → reload。
    crate::config::Config::reload_cached();
    spawn_config_watcher();

    // Phase 6.1: ensure the DB master key is loaded (from env or
    // auto-generated file) BEFORE we try to read/write account tokens.
    // Loading lazily later would also work but doing it here surfaces
    // key-load problems at boot rather than mid-message.
    if let Err(e) = crate::storage::crypto::ensure_master_key() {
        tracing::error!("db master key: {e}");
        crate::daemon::pid::remove_pid();
        return Err(format!("db master key: {e}"));
    }
    // Phase 6.1 token backfill — sqlx async version (rusqlite path removed).
    // Skipped on first boot of v4.2 binary: v2-v4.1 already ran refinery
    // which encrypted tokens during V0002. Legacy plaintext-only rows
    // remain decryptable via materialize_account fallback.

    // Phase 3: ensure there's at least one active super_admin API key.
    if let Err(e) = crate::auth::admin_key::ensure_bootstrap_super_admin(pool.clone()).await {
        tracing::error!("bootstrap admin key: {e}");
        crate::daemon::pid::remove_pid();
        return Err(format!("bootstrap admin key: {e}"));
    }

    // Ensure the defaults template exists, then backfill `model: sonnet`
    // into every existing user whose settings.json doesn't carry one yet.
    // Idempotent: subsequent restarts will see all users `kept_existing`.
    if let Err(e) = crate::defaults::ensure_defaults_exist() {
        tracing::warn!("defaults bootstrap: {e}");
    }
    let migration = crate::defaults::migrate_existing_users();
    info!(
        "settings migration: scanned={} model_backfilled={} kept_existing={} errors={}",
        migration.users_scanned,
        migration.users_updated,
        migration.users_kept_existing,
        migration.errors.len()
    );
    for e in &migration.errors {
        tracing::warn!("settings migration: {e}");
    }

    // Apply idempotent config migrations (currently: ai.timeoutMs 60s→300s).
    let fired = crate::config::Config::apply_migrations();
    for name in &fired {
        info!("config migration fired: {name}");
    }

    // Sandbox preflight is fatal: weclawbot needs bwrap to enforce per-user
    // isolation. Without it we refuse to start — failing loud is better than
    // silently leaking state across users.
    let report = crate::sandbox::preflight();
    if let Some(p) = &report.podman {
        info!("podman detected: {} ({})", p.path.display(), p.version);
    }
    if let Some(r) = &report.runsc {
        info!(
            "runsc detected: {} ({}, registered={})",
            r.path.display(),
            r.version,
            r.registered
        );
    }
    if !report.errors.is_empty() {
        for e in &report.errors {
            eprintln!("preflight: {e}");
            tracing::error!("preflight: {e}");
        }
        crate::daemon::pid::remove_pid();
        return Err("preflight failed; see messages above".into());
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client = Arc::new(ILinkClient::new());

    // Phase 5.3: 启动时立刻检查一次 Claude OAuth token 剩余寿命，
    // 然后开后台 watcher 每 30 分钟扫一次。避免运营商在 token 默默
    // 过期后才发现 sandbox 401。
    crate::auth::claude_oauth::report_once();
    crate::auth::claude_oauth::spawn_watcher(shutdown_rx.clone());

    let account_ids = list_indexed_account_ids();
    info!("Starting monitors for {} accounts", account_ids.len());

    // v5 M3: dispatch per-account 按 platform_id ── iLink 走 run_account_monitor，
    // Telegram 走 run_telegram_monitor。Discord/Feishu 不 poll (gateway/webhook)。
    let mut handles = Vec::new();
    let telegram_bot = Arc::new(crate::puppet::telegram::TelegramBot::new());
    for id in account_ids {
        let acct = match crate::auth::accounts::load_account(&id) {
            Some(a) => a,
            None => continue,
        };
        let rx = shutdown_rx.clone();
        match acct.platform_id.as_str() {
            "telegram" => {
                let bot = Arc::clone(&telegram_bot);
                handles.push(tokio::spawn(
                    crate::monitor::telegram_poller::run_telegram_monitor(bot, id, rx),
                ));
            }
            "discord" => {
                // v5.3: Discord Gateway WebSocket monitor (feature-gated)
                #[cfg(feature = "discord-gateway")]
                {
                    if let Some(token) = acct.token.as_ref() {
                        let bot = Arc::new(crate::puppet::discord::DiscordBot::new());
                        let tok = token.clone();
                        handles.push(tokio::spawn(
                            crate::monitor::discord_gateway_monitor::run_discord_monitor(
                                bot, id.clone(), tok, rx,
                            ),
                        ));
                        info!("[{id}] platform=discord — gateway monitor started");
                    } else {
                        info!("[{id}] platform=discord — no token, skipping gateway");
                    }
                }
                #[cfg(not(feature = "discord-gateway"))]
                {
                    info!(
                        "[{id}] platform=discord — discord-gateway feature not enabled; \
                         rebuild with --features discord-gateway to enable gateway monitor"
                    );
                }
            }
            "feishu" => {
                // v5.3: Feishu 走 HTTP webhook route (service::feishu_webhook).
                // 这里只 log，inbound 通过 /api/v1/puppet/feishu/webhook/<id> 入。
                info!(
                    "[{id}] platform=feishu — inbound via /api/v1/puppet/feishu/webhook/{id}"
                );
            }
            _ => {
                // ilink-wechat default
                let c = Arc::clone(&client);
                handles.push(tokio::spawn(run_account_monitor(c, id, rx)));
            }
        }
    }

    // v2.2 L4.1: 后台 reconciler 定期扫 orphan podman 容器 + sandbox
    // dirs（DB 里没行但 fs/podman 还活着的残留）。delete_user 的 fs
    // cleanup 是 best-effort，碰到磁盘满 / 权限 / 容器持文件 等情况会
    // 失败，没有 reconciler 残留会无限堆积。
    {
        let repo: Arc<crate::repo::users_async::SqlxUserRepo> =
            Arc::new(crate::repo::users_async::SqlxUserRepo::new(pool.clone()));
        let _reconciler_handle = crate::sandbox::reconciler::spawn_reconciler(
            repo,
            crate::sandbox::reconciler::DEFAULT_INTERVAL,
        );
        info!("reconciler started (interval = 10 min)");
    }

    let server_handle = tokio::spawn(run_server(bind.to_string(), port));

    // Phase 5.4 graceful shutdown drain:
    // - SIGINT (Ctrl-C on terminal) or SIGTERM (systemd / docker stop / `weclawbot stop`)
    //   triggers a drain instead of an abort.
    // - Pollers stop fetching new inbound messages (shutdown channel).
    // - Currently-running handler tasks (the ones holding a sandbox +
    //   claude process) keep running until they complete naturally OR
    //   `DRAIN_DEADLINE_SECS` elapses, whichever comes first.
    // - HTTP server stays up so the operator can hit /healthz during
    //   drain; only torn down after the drain window closes.
    wait_for_shutdown_signal().await;
    println!("\nShutting down gracefully (drain mode, max {DRAIN_DEADLINE_SECS}s)...");
    info!("graceful shutdown initiated — pollers will stop, in-flight handlers will drain");
    let _ = shutdown_tx.send(true);

    // 把每个 monitor task 等到自然退出，但整体不超过 DRAIN_DEADLINE_SECS。
    // 超时后 abort 残留 task，避免 Ctrl-C 多按几下都退不出。
    let deadline = tokio::time::Duration::from_secs(DRAIN_DEADLINE_SECS);
    let drain = async {
        for h in handles {
            let _ = h.await;
        }
    };
    if tokio::time::timeout(deadline, drain).await.is_err() {
        tracing::warn!(
            "drain timed out after {DRAIN_DEADLINE_SECS}s — in-flight tasks will be aborted"
        );
    }

    // v5.4 L4.1: ACP mode 下还有 per-user long-running claude 进程，
    // drain 完 poller 后显式 kill 它们让 daemon exit 干净（不留 zombie）。
    #[cfg(feature = "acp")]
    crate::ai::claude::session::shutdown_all().await;

    server_handle.abort();
    crate::daemon::pid::remove_pid();
    info!("weclawbot stopped");
    Ok(())
}

/// 最长等多久让 in-flight 跑完。60s 覆盖大部分单条 claude 调用
/// （含 sandbox 启动 + 一次回复）。超时后强制结束，避免 SIGTERM
/// 卡死无法 stop。
const DRAIN_DEADLINE_SECS: u64 = 60;

/// 同时等 SIGINT (Ctrl-C) 和 SIGTERM。在 Unix 下两者都接；Windows
/// 上只接 Ctrl-C（systemd 信号在 Windows 没有概念）。
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("install SIGTERM handler: {e} — falling back to Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
            _ = term.recv() => info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl-C");
    }
}

/// v2.1.B1: spawn 一个后台 task 监听 `~/.weclawbot/config.json` 的
/// mtime 变化，每次变化时调 `Config::reload_cached()` 重读到 ArcSwap。
///
/// 用 notify crate 的 RecommendedWatcher（platform: inotify / kqueue /
/// ReadDirectoryChangesW）。事件触发的 callback 必须 Send + 'static，
/// 我们在里面 spawn 一个 blocking task 跑 reload —— reload 本身是同步
/// 文件 IO，不要污染 watcher 自己的线程。
fn spawn_config_watcher() {
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    let path = crate::storage::state_dir::config_path();
    if !path.exists() {
        // 文件还没建出来；启动早期 Config::ensure_exists 会建。下次 daemon
        // restart 后 watcher 才有用。这里不致命，不卡 boot。
        tracing::debug!("config watcher: {} doesn't exist yet, skip watch", path.display());
        return;
    }
    let watch_path = path.clone();
    std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("config watcher init: {e}");
                    return;
                }
            };
            if let Err(e) = watcher.watch(&watch_path, RecursiveMode::NonRecursive) {
                tracing::warn!("config watcher watch: {e}");
                return;
            }
            tracing::info!("config watcher: monitoring {}", watch_path.display());
            while let Ok(res) = rx.recv() {
                match res {
                    Ok(ev) if matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) => {
                        tracing::info!("config.json changed — reloading");
                        crate::config::Config::reload_cached();
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("config watcher event: {e}"),
                }
            }
        })
        .ok();
}

fn start_background(bind: &str, port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    #[cfg(windows)]
    {
        // Rust's std::process::Command always sets bInheritHandles=TRUE on Windows,
        // so a child spawned with Stdio::null() can still inherit the launching
        // shell's stdout pipe handle. PowerShell then waits on that pipe until the
        // daemon exits. We route through cmd.exe's `start` builtin instead, which
        // calls CreateProcess with bInheritHandles=FALSE.
        use std::os::windows::process::CommandExt;
        let cmdline = format!(
            "start \"\" /B \"{}\" start --foreground --port {} --bind {}",
            exe.display(),
            port,
            bind,
        );
        let mut cmd = std::process::Command::new("cmd");
        // raw_arg bypasses Rust's argument-quoting rules — required so that the
        // inner quotes around the exe path reach cmd intact.
        cmd.raw_arg("/C")
            .raw_arg(&cmdline)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

        // Wait briefly for the daemon to claim its PID file.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            if let Some(pid) = crate::daemon::pid::read_pid() {
                if crate::daemon::pid::is_process_alive(pid) {
                    println!("weclawbot started in background (pid {pid})");
                    return Ok(());
                }
            }
        }
        return Err("daemon did not start within 3s (check ~/.weclawbot/logs/)".into());
    }

    #[cfg(not(windows))]
    {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["start", "--foreground", "--port", &port.to_string(), "--bind", bind])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
        println!("weclawbot started in background (pid {})", child.id());
        Ok(())
    }
}
