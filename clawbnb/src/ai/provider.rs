//! `ReplyProvider` — v2.2 L2.2 抽象。
//!
//! `monitor::handler::dispatch_reply` 之前是 80 行 if-else 链：webhook →
//! claude/codex → API → echo → none。改业务规则要改这个函数；新增
//! provider（v3 的 Gemini / Mistral 等）必须重新串 if-else。
//!
//! 这层把每个 provider 抽成 `dyn ReplyProvider`，handler 用优先级链遍历。
//! 加新 provider = 新写一个 impl + 在 dispatcher 里 push 进 vec。
//!
//! ## 语义保留
//!
//! 现行行为 1:1 复刻：
//! - **gating，不是 fallback**：第一个 `enabled() == true` 的 provider 处理
//!   该消息（成功或失败都终止链），不会因为 webhook 出错就跌到 claude
//!   —— 这是 operator 的预期：webhook 配了就是想用它，失败也应该报告
//!   而不是悄悄换 provider
//! - Provider 失败 → 用户收到通用文案 "(AI 暂时无法回复，请稍后重试)"，
//!   细节 warn! 进 daemon log
//! - 非文本消息走 API provider 时返回特定文案

use async_trait::async_trait;

use crate::ai::cli_provider::{AttachedUrl, GeneratedFile};
use crate::api::types::WeixinMessage;
use crate::config::Config;
use crate::media::inbound::InboundContent;
use crate::sandbox::Sandbox;

/// 给 provider 跑一次的所有上下文。多模态字段（content）+ msg metadata
/// 都在这里 —— 不同 provider 各取所需。
///
/// v3.1: 新增 `inbound: Option<&CommonInbound>` 字段做 protocol-agnostic
/// 标准化。新代码 / Telegram 走的路径优先看 `inbound`；遗留 iLink path
/// 仍可读 `msg` 取 context_token / session_id 等 iLink-only 字段。
/// 两者并存让迁移可增量做：webhook payload 一次性切到 CommonInbound，
/// reply::send_text / forward::* 维持 iLink-specific 不动。
pub struct DispatchContext<'a> {
    pub account_id: &'a str,
    pub sandbox: &'a Sandbox,
    pub content: &'a InboundContent,
    pub config: &'a Config,
    /// iLink-specific raw message — only used by provider impls that need
    /// iLink fields (`context_token`, `session_id`). v3 protocol handlers
    /// 可以传 None。
    pub msg: Option<&'a WeixinMessage>,
    /// Protocol-agnostic inbound projection — 所有 provider 应该优先
    /// 用这个；只有需要 iLink 特定 field 才 fallback 到 `msg`。
    pub inbound: Option<&'a crate::monitor::common::CommonInbound>,
}

/// Provider 跑完一次的产物。`text` = 要回 WeChat 的话；`files` /
/// `urls` = AI 声明的附件（v2 之后 forward 链会处理）。
#[derive(Debug, Default)]
pub struct ProviderOutput {
    pub text: Option<String>,
    pub files: Vec<GeneratedFile>,
    pub urls: Vec<AttachedUrl>,
    /// CLI provider 跑通且产出有效，gates diff fallback in handler.
    pub cli_succeeded: bool,
    /// 哪个 provider 处理的（metric/audit label）。
    pub provider_name: &'static str,
}

#[async_trait]
pub trait ReplyProvider: Send + Sync {
    /// Stable identifier (metric label / log)。
    fn name(&self) -> &'static str;

    /// 当前 config 下该 provider 是否能跑。返回 false → dispatcher 跳过此
    /// provider 继续往下尝试。
    fn enabled(&self, cfg: &Config) -> bool;

    /// 处理这条消息。返回结果即终止 dispatch chain（gating 语义）。
    /// 失败应转成 ProviderOutput { text: Some("(AI 暂时无法回复...)"), .. }
    /// 而不是 panic / 抛 Err —— 用户体验由 provider 自己负责。
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> ProviderOutput;
}

// =============================================================================
// Concrete providers
// =============================================================================

pub struct WebhookProvider;

#[async_trait]
impl ReplyProvider for WebhookProvider {
    fn name(&self) -> &'static str {
        "webhook"
    }
    fn enabled(&self, cfg: &Config) -> bool {
        !cfg.webhook.url.is_empty()
    }
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> ProviderOutput {
        // v3.1: webhook payload 改用 protocol-agnostic CommonInbound 作为输入。
        // 没有 inbound（极少数 legacy 路径）时跌回 iLink-specific shim。
        let text = if let Some(common) = ctx.inbound {
            crate::monitor::webhook::dispatch_common(
                &ctx.config.webhook.url,
                common,
                ctx.content,
            )
            .await
        } else if let Some(msg) = ctx.msg {
            crate::monitor::webhook::dispatch(
                &ctx.config.webhook.url,
                ctx.account_id,
                msg,
                ctx.content,
            )
            .await
        } else {
            tracing::warn!("WebhookProvider: neither inbound nor msg set; skipping");
            None
        };
        ProviderOutput {
            text,
            provider_name: "webhook",
            ..Default::default()
        }
    }
}

pub struct AiCliProvider;

#[async_trait]
impl ReplyProvider for AiCliProvider {
    fn name(&self) -> &'static str {
        "ai-cli"
    }
    fn enabled(&self, cfg: &Config) -> bool {
        crate::ai::cli_provider::CliConfig::from_global(&cfg.ai).is_some()
    }
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> ProviderOutput {
        let cli_cfg = match crate::ai::cli_provider::CliConfig::from_global(&ctx.config.ai) {
            Some(c) => c,
            None => {
                // enabled() 应该已挡住；理论不可达，留 trace 防回归。
                tracing::warn!("AiCliProvider::dispatch with no CliConfig — config raced?");
                return ProviderOutput {
                    text: Some("(AI 暂时无法回复，请稍后重试)".into()),
                    provider_name: "ai-cli",
                    ..Default::default()
                };
            }
        };
        match crate::ai::cli_provider::complete_with_content(&cli_cfg, ctx.sandbox, ctx.content)
            .await
        {
            Ok(out) => ProviderOutput {
                text: Some(out.text),
                files: out.generated_files,
                urls: out.generated_urls,
                cli_succeeded: true,
                provider_name: "ai-cli",
            },
            Err(e) => {
                // v2.1.A1: raw e 含敏感信息只 warn 进 log，用户看通用文案。
                tracing::warn!(
                    "[{}] AI CLI ({}) failed: {e}",
                    ctx.account_id,
                    cli_cfg.provider
                );
                ProviderOutput {
                    text: Some("(AI 暂时无法回复，请稍后重试)".into()),
                    provider_name: "ai-cli",
                    ..Default::default()
                }
            }
        }
    }
}

pub struct AiApiProvider;

#[async_trait]
impl ReplyProvider for AiApiProvider {
    fn name(&self) -> &'static str {
        "ai-api"
    }
    fn enabled(&self, cfg: &Config) -> bool {
        crate::ai::chat::ChatProviderConfig::from_global(&cfg.ai).is_some()
    }
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> ProviderOutput {
        let api_cfg = match crate::ai::chat::ChatProviderConfig::from_global(&ctx.config.ai) {
            Some(c) => c,
            None => {
                return ProviderOutput {
                    text: Some("(AI 暂时无法回复，请稍后重试)".into()),
                    provider_name: "ai-api",
                    ..Default::default()
                };
            }
        };
        if ctx.content.text.is_empty() {
            return ProviderOutput {
                text: Some(
                    "(我看到你发了非文本消息，但当前 API 模式不支持多模态。)".into(),
                ),
                provider_name: "ai-api",
                ..Default::default()
            };
        }
        match crate::ai::chat::complete(&api_cfg, ctx.sandbox, &ctx.content.text).await {
            Ok(r) => ProviderOutput {
                text: Some(r),
                provider_name: "ai-api",
                ..Default::default()
            },
            Err(e) => {
                tracing::warn!("[{}] AI API failed: {e}", ctx.account_id);
                ProviderOutput {
                    text: Some("(AI 暂时无法回复，请稍后重试)".into()),
                    provider_name: "ai-api",
                    ..Default::default()
                }
            }
        }
    }
}

pub struct EchoProvider;

#[async_trait]
impl ReplyProvider for EchoProvider {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn enabled(&self, cfg: &Config) -> bool {
        cfg.echo.enabled
    }
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> ProviderOutput {
        let body = if ctx.content.text.is_empty() {
            "(non-text)".to_string()
        } else {
            ctx.content.text.clone()
        };
        ProviderOutput {
            text: Some(format!("{}{}", ctx.config.echo.prefix, body)),
            provider_name: "echo",
            ..Default::default()
        }
    }
}

/// 默认链：webhook > AI-CLI > AI-API > echo。如果都不 enabled 返回空
/// `ProviderOutput`（handler 见到 text=None 就不回消息）。
///
/// 加新 provider：写一个 `impl ReplyProvider` + 在这里 push 进 vec 即可。
pub async fn run_default_chain(ctx: &DispatchContext<'_>) -> ProviderOutput {
    let providers: [&dyn ReplyProvider; 4] = [
        &WebhookProvider,
        &AiCliProvider,
        &AiApiProvider,
        &EchoProvider,
    ];
    for p in providers {
        if p.enabled(ctx.config) {
            return p.dispatch(ctx).await;
        }
    }
    ProviderOutput {
        provider_name: "none",
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 一个极简 mock provider，验证 chain 的 gating 语义。
    struct AlwaysReply {
        name: &'static str,
        text: &'static str,
    }
    #[async_trait]
    impl ReplyProvider for AlwaysReply {
        fn name(&self) -> &'static str {
            self.name
        }
        fn enabled(&self, _cfg: &Config) -> bool {
            true
        }
        async fn dispatch(&self, _ctx: &DispatchContext<'_>) -> ProviderOutput {
            ProviderOutput {
                text: Some(self.text.into()),
                provider_name: self.name,
                ..Default::default()
            }
        }
    }

    struct Disabled;
    #[async_trait]
    impl ReplyProvider for Disabled {
        fn name(&self) -> &'static str {
            "disabled"
        }
        fn enabled(&self, _cfg: &Config) -> bool {
            false
        }
        async fn dispatch(&self, _ctx: &DispatchContext<'_>) -> ProviderOutput {
            panic!("disabled provider should never dispatch");
        }
    }

    #[tokio::test]
    async fn first_enabled_wins() {
        // 不构造完整 DispatchContext —— enabled() 完全不看 ctx，dispatch 也不看。
        // 这里只验证 trait shape + gating 语义。
        let disabled = Disabled;
        let first = AlwaysReply {
            name: "first",
            text: "hi from first",
        };
        let second = AlwaysReply {
            name: "second",
            text: "hi from second",
        };
        let chain: [&dyn ReplyProvider; 3] = [&disabled, &first, &second];

        // 模拟 run_default_chain 但不需要 ctx：直接遍历。
        let cfg = Config::default();
        let mut picked: Option<&'static str> = None;
        for p in chain {
            if p.enabled(&cfg) {
                picked = Some(p.name());
                break;
            }
        }
        assert_eq!(picked, Some("first"));
    }
}
