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
    // v5.1 N3: 真 wire — OTel 0.27 API。endpoint 默认 localhost:4317
    // (gRPC OTLP)，env `WECLAWBOT_OTLP_ENDPOINT` 可改。Collector 不可达
    // 时 daemon 启动仍跑 (warn log)，避免 OTel 故障阻塞 production。
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};

    let endpoint = std::env::var("WECLAWBOT_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let exporter_res = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build();
    let exporter = match exporter_res {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "OTel exporter build failed ({e}) — falling back to stub (deps loaded, no export)"
            );
            return Ok(());
        }
    };

    let resource = Resource::new(vec![KeyValue::new(
        "service.name",
        service_name.to_string(),
    )]);

    let provider = sdktrace::TracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_resource(resource)
        .build();

    // Set global tracer provider — batch exporter starts pushing spans to
    // OTLP collector as soon as code uses `global::tracer("name").start(...)`.
    //
    // **tracing-opentelemetry bridge layer**: 装 layer 需要拿到现有
    // `tracing_subscriber::Registry`，但 daemon::log::init 已经 set 了一份
    // FmtSubscriber 没 Registry expose 接口。v5.2 refactor 把 log init
    // 改成 Registry-based，能把 OTel layer 加进去；当前**直接调** OTel
    // tracer 路径仍 work（global::tracer("weclawbot").start(...)），只是
    // tracing::info_span!() macro 不自动桥接。
    let _tracer = provider.tracer("weclawbot");
    global::set_tracer_provider(provider);

    tracing::info!(
        service = %service_name,
        endpoint = %endpoint,
        "OTel OTLP exporter initialized"
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
