//! Mock `MessagingPlatform` impl for unit tests (v2.2 L2.1).
//!
//! handler.rs 之前没法单测因为 `ILinkClient` 是具体类型；mock 让我们
//! 可以注入"发了什么消息"的捕获通道 + 预设的 inbound 队列。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::api::types::GetUpdatesResp;
use crate::error::WeclawError;
use crate::puppet::{MessagingPlatform, OutboundMessage};

/// Capture-and-respond mock — 测试 handler 时这是 ILinkClient 的替身。
#[derive(Default, Clone)]
pub struct MockPlatform {
    /// 所有 send_text 调用的 (target, text) 顺序入队这里
    pub sent_texts: Arc<Mutex<Vec<(String, String)>>>,
    /// send_file 调用，元素 = (target, path display, file_name?)
    pub sent_files: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    /// 预设的 inbound updates；poll_updates 每次 pop 一个返回，空了
    /// 返回空 resp（模拟 long-poll timeout）
    pub queued_inbounds: Arc<Mutex<Vec<GetUpdatesResp>>>,
}

impl MockPlatform {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn captured_text(&self) -> Vec<(String, String)> {
        self.sent_texts.lock().unwrap().clone()
    }

    pub fn captured_files(&self) -> Vec<(String, String, Option<String>)> {
        self.sent_files.lock().unwrap().clone()
    }

    pub fn queue_inbound(&self, resp: GetUpdatesResp) {
        self.queued_inbounds.lock().unwrap().push(resp);
    }
}

#[async_trait]
impl MessagingPlatform for MockPlatform {
    fn platform_id(&self) -> &'static str {
        "mock"
    }

    async fn poll_updates(
        &self,
        _token: &str,
        _base_url: &str,
        _buf: &str,
    ) -> Result<GetUpdatesResp, WeclawError> {
        // 队列空时返回 default(没消息)，模拟 long-poll timeout
        let mut q = self.queued_inbounds.lock().unwrap();
        if q.is_empty() {
            return Ok(GetUpdatesResp::default());
        }
        Ok(q.remove(0))
    }

    async fn send_text(
        &self,
        _token: &str,
        _base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        if let Some(t) = out.text {
            self.sent_texts
                .lock()
                .unwrap()
                .push((out.target.to_string(), t.to_string()));
        }
        Ok(())
    }

    async fn send_file(
        &self,
        _token: &str,
        _base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        if let Some(p) = out.file {
            self.sent_files.lock().unwrap().push((
                out.target.to_string(),
                p.display().to_string(),
                out.file_name.map(|s| s.to_string()),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_captures_send_text() {
        let m = MockPlatform::new();
        m.send_text(
            "tok",
            "https://example.test",
            OutboundMessage {
                target: "u1",
                text: Some("hi"),
                file: None,
                file_name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(m.captured_text(), vec![("u1".into(), "hi".into())]);
    }

    #[tokio::test]
    async fn mock_inbound_queue_drains_then_empty() {
        let m = MockPlatform::new();
        m.queue_inbound(GetUpdatesResp::default());
        // 第一次 poll 拿队列里那个
        let r1 = m.poll_updates("", "", "").await.unwrap();
        assert!(r1.msgs.is_none());
        // 第二次 poll 队列空 → default
        let r2 = m.poll_updates("", "", "").await.unwrap();
        assert!(r2.msgs.is_none());
    }

    #[tokio::test]
    async fn mock_default_qr_login_returns_unsupported() {
        let m = MockPlatform::new();
        assert!(!m.supports_qr_login());
        let r = m.fetch_qr_code("https://x", "3", &[]).await;
        assert!(r.is_err());
    }
}
