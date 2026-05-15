// v7.0 audit-aware `#![allow(dead_code)]`. After the v7 housekeeping
// dead-code sweep (which deleted ~600 LoC and trimmed compiler warnings
// from 113 → 48), the residual warnings fall into four legitimate
// categories that we don't want to delete:
//
// 1. `#[derive(Deserialize)]` fields on API response structs
//    (`ret`, `errmsg`, `longpolling_timeout_ms`, `thumb_upload_param`,
//    `attachments`, …) — kept so we deserialize the full upstream JSON
//    without silently dropping fields. Removing them risks losing
//    information when iLink/Telegram/Discord adds a field we *do* need.
// 2. Multi-protocol puppet scaffold under `src/puppet/{discord,telegram,
//    feishu,mock}*.rs` — type-system commitment to v3 SaaS protocols,
//    typed but not yet wired into the runtime poller. Deleting these
//    would force a from-scratch re-design when we add the second
//    protocol; allowing dead-code preserves the abstraction.
// 3. `MessagingPlatform` trait methods (`platform_id`, `send_file`,
//    `send_typing`, `supports_qr_login`, `fetch_qr_code`,
//    `poll_qr_status`) — abstract surface. The default iLink poller
//    only calls a subset; future Discord/Telegram impls will fill in
//    the rest.
// 4. Test-only constructions of stable taxonomies (`TenantId::is_default`,
//    `WeclawError::{BadRequest,Unauthorized,Forbidden,NotFound,Conflict,
//    RateLimited}`) — `cfg(test)` constructors don't satisfy the release
//    build's "never constructed" check, but the variants are matched in
//    `http_status`/`problem_type`/`problem_title` and exercised by unit
//    tests. Deleting them would force re-adding the moment a new
//    handler needs to return 403.
//
// Real new dead code is now rare enough that a future tightening can
// switch this to per-file allows + a `cargo clippy -- -D dead_code` CI
// gate. Until then, the global allow is the pragmatic call.
#![allow(dead_code)]

mod ai;
mod api;
mod app;
mod auth;
mod automation;
mod binding;
mod cli;
mod config;
mod daemon;
mod defaults;
#[cfg(feature = "ee")]
mod ee;
mod error;
mod ids;
mod media;
mod monitor;
mod observability;
mod pii;
mod puppet;
mod repo;
mod runtime;
mod sandbox;
mod service;
mod skills;
mod storage;
mod tenancy;
mod wechat_menu;

use clap::{Parser, Subcommand};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "weclawbot", about = "WeChat-to-AI message bridge with multi-tenant isolation")]
#[command(version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the current version
    Version,
    /// Add a WeChat account via QR code scan
    Login,
    /// Start the message bridge
    Start {
        /// Run in foreground (default is background)
        #[arg(short, long)]
        foreground: bool,
        /// API/console listen port
        #[arg(long, default_value = "18011")]
        port: u16,
        /// API/console listen address
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
    /// Stop the background process
    Stop,
    /// Check if weclawbot is running
    Status,
    /// Restart the background process
    Restart {
        #[arg(long, default_value = "18011")]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
    /// Send a message to a WeChat user
    Send {
        /// Target user ID
        #[arg(long)]
        to: String,
        /// Message text
        #[arg(long)]
        text: String,
    },
    /// Show or update config (~/.weclawbot/config.json)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Configure AI reply provider.
    ///
    /// CLI mode (uses your Claude Code / Codex subscription — no API key needed):
    ///   weclawbot ai --provider claude
    ///   weclawbot ai --provider codex
    ///
    /// API mode (OpenAI-compatible — DeepSeek, Kimi, OpenAI, Ollama, ...):
    ///   weclawbot ai --base-url https://api.deepseek.com/v1 --api-key sk-... --model deepseek-chat
    Ai {
        /// CLI provider name: "claude" or "codex" (uses installed CLI subscription)
        #[arg(long)]
        provider: Option<String>,
        /// Override CLI binary path (defaults to `claude` / `codex` in PATH)
        #[arg(long)]
        binary: Option<String>,
        /// Base URL of OpenAI-compatible API (API mode)
        #[arg(long)]
        base_url: Option<String>,
        /// API key / Bearer token (API mode)
        #[arg(long)]
        api_key: Option<String>,
        /// Model name (API mode)
        #[arg(long)]
        model: Option<String>,
        /// System prompt
        #[arg(long)]
        system_prompt: Option<String>,
        /// Disable AI (clears enabled flag)
        #[arg(long)]
        off: bool,
    },
    /// Open the web management console in browser
    Console {
        /// Console listen port
        #[arg(long, default_value = "18011")]
        port: u16,
        /// Console listen address
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
    /// Run environment self-check (bwrap, claude, skills, policy)
    Doctor,
    /// Manage per-user sandboxes
    Users {
        #[command(subcommand)]
        action: UsersAction,
    },
    /// Update weclawbot to the latest version
    Update,
    /// Take an online snapshot of state.db to `<out>`
    Backup {
        /// Output path for the snapshot file
        #[arg(long, short = 'o')]
        out: String,
    },
    /// Restore state.db from a previously taken snapshot. Requires daemon stopped.
    Restore {
        /// Input snapshot path
        snapshot: String,
    },
    /// v5.5: Export one tenant's data as a self-contained `.tar.gz`.
    ///
    /// The bundle contains: state-db dump (subset for this tenant) +
    /// config.json + per-user workspace tarballs + admin_keys for the
    /// tenant. Use case: SaaS tenant migrates to self-hosting; backup
    /// before suspending; compliance audit response.
    ExportTenant {
        /// Tenant id to export. Use `default` for single-tenant deployments.
        #[arg(long)]
        tenant: String,
        /// Output path (`.tar.gz`).
        #[arg(long, short = 'o')]
        out: String,
    },
    // v7.0: ImportSqlite removed. Legacy SQLite users must `git checkout
    // v5.6 && weclawbot import-sqlite ./state.db` first, then upgrade.
}

#[derive(Subcommand)]
enum UsersAction {
    /// List all known WeChat user sandboxes
    List,
    /// Delete a single user's sandbox (forces fresh state on next message)
    Reset { hash: String },
    /// Remove sandboxes whose last_seen is older than N days (default 30)
    Prune {
        #[arg(long, default_value = "30")]
        older_than_days: u64,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current config
    Show,
    /// Set a config value (e.g. `config set echo.enabled true`)
    Set { key: String, value: String },
    /// Toggle echo reply (alias for `config set echo.enabled <on>`)
    Echo {
        /// true or false
        on: String,
    },
    /// Set webhook URL (alias for `config set webhook.url <url>`)
    Webhook {
        /// URL (pass empty string "" to clear)
        #[arg(default_value = "")]
        url: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Version => {
            cli::version::run();
            Ok(())
        }
        Commands::Login => cli::login::run().await,
        Commands::Start { foreground, port, bind } => {
            cli::start::run(foreground, &bind, port).await
        }
        Commands::Stop => cli::stop::run().await,
        Commands::Status => cli::status::run().await,
        Commands::Restart { port, bind } => cli::restart::run(&bind, port).await,
        Commands::Send { to, text } => cli::send::run(&to, &text).await,
        Commands::Config { action } => match action {
            ConfigAction::Show => cli::config::show().await,
            ConfigAction::Set { key, value } => cli::config::set(&key, &value).await,
            ConfigAction::Echo { on } => {
                let parsed = matches!(on.to_lowercase().as_str(), "true" | "on" | "1" | "yes");
                cli::config::echo(parsed).await
            }
            ConfigAction::Webhook { url } => cli::config::webhook(&url).await,
        },
        Commands::Ai {
            provider,
            binary,
            base_url,
            api_key,
            model,
            system_prompt,
            off,
        } => {
            cli::ai_setup::run(
                provider,
                binary,
                base_url,
                api_key,
                model,
                system_prompt,
                off,
            )
            .await
        }
        Commands::Console { port, bind } => cli::open_browser::run(&bind, port).await,
        Commands::Doctor => cli::doctor::run().await,
        Commands::Users { action } => match action {
            UsersAction::List => cli::users::list().await,
            UsersAction::Reset { hash } => cli::users::reset(&hash).await,
            UsersAction::Prune { older_than_days } => cli::users::prune(older_than_days).await,
        },
        Commands::Update => cli::update::run().await,
        Commands::Backup { out } => cli::backup::backup(std::path::Path::new(&out)),
        Commands::Restore { snapshot } => {
            cli::backup::restore(std::path::Path::new(&snapshot))
        }
        Commands::ExportTenant { tenant, out } => {
            cli::export_tenant::run(&tenant, std::path::Path::new(&out)).await
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
