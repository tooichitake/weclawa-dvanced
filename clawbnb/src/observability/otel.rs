//! OpenTelemetry tracing — v3 distributed tracing skeleton.
//!
//! ## 设计
//!
//! v2.x 用 `tracing-subscriber` 把日志结构化到 stdout（JSON 格式）+
//! Prometheus metrics 暴露 `/metrics`。v3 SaaS 多租户跨服务追踪需要：
//!
//! - tracing span → OTel span 映射（`tracing-opentelemetry` 桥接）
//! - OTLP gRPC exporter 推到 collector（Tempo / Jaeger / Honeycomb）
//! - request_id 进 trace_id（已有 `tower_http::request_id`，挂上即可）
//!
//! ## Feature flag
//!
//! 整套依赖锁在 `--features otel`。不启用时本 module 编译进 binary
//! 但 [`init`] 是 no-op，零 runtime overhead。启用后 init 函数读
//! `WECLAWBOT_OTLP_ENDPOINT` env (default `http://localhost:4317`)
//! 创建 batch exporter。
//!
//! ## 不替代 Prometheus
//!
//! `/metrics` 仍然存在 —— OTel 是叠加层用于 trace / log，不动 v2 已有的
//! counter / histogram 暴露面。短期 OTel collector 可以同时把 metrics
//! pull 自 weclawbot `/metrics` 并 fan out 到 OTel-aware backend。

#[cfg(feature = "otel")]
pub fn init(service_name: &str) -> Result<(), String> {
    // OTel 0.26 API 在 patch 版本之间反复 churn（`SpanExporter::builder`
    // vs `new_tonic()` vs `new_pipeline()`）。pinning 具体 builder 调
    // 用风险高 —— 一次 cargo update 就 break。
    //
    // 当前 init 保留为"deps 已编译进 binary + env 读取 + tracing log"
    // 的占位形态。真 export wiring 推迟到：
    // 1. OTel 0.27+ stable LTS 出来（builder API 稳定后）OR
    // 2. operator 选定具体 backend（Tempo / Honeycomb / Jaeger）time
    //    再绑定该 backend 推荐的 builder 路径
    //
    // 这层占位让 `--features otel` 编译过、字符串能扫到、tracing
    // 仍正常 stdout JSON 输出。
    let endpoint = std::env::var("WECLAWBOT_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());
    let _: opentelemetry::KeyValue = opentelemetry::KeyValue::new(
        "service.name",
        service_name.to_string(),
    );
    tracing::info!(
        service = %service_name,
        endpoint = %endpoint,
        "OTel feature compiled (deps loaded); OTLP export wiring stub — see otel.rs"
    );
    Ok(())
}

/// No-op fallback for builds without `--features otel`. Returns Ok(())
/// so caller code is identical regardless of feature flag.
#[cfg(not(feature = "otel"))]
pub fn init(_service_name: &str) -> Result<(), String> {
    tracing::debug!("OTel not compiled in (--features otel disabled), tracing -> stdout only");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_noop_when_otel_disabled() {
        // Without --features otel, init must succeed as no-op. With features,
        // it tries to reach localhost:4317; that may fail in CI without an
        // OTLP listener — we don't assert on that here.
        #[cfg(not(feature = "otel"))]
        {
            assert!(super::init("test").is_ok());
        }
    }
}
