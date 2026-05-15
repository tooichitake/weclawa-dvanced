//! Process-wide typed error — the canonical error type at every module
//! boundary except CLI entry points (`src/cli/*.rs`).
//!
//! ## v2.2 L0.2: 删 #![allow(dead_code)]
//!
//! 之前 v2 落地时只搭了 WeclawError 类型骨架，callsite 还在用
//! `Result<T, String>`。v2.2 重构正式让 WeclawError 在 module 边界
//! 真用起来：
//!
//! - service/* handler 全部返回 `Result<T, WeclawError>` + 走 IntoResponse
//! - repo/* 返回 `Result<T, DbError>`（已是 thiserror，DbError → WeclawError 自动）
//! - monitor/* / ai/* / sandbox/* 内部仍可 `Result<_, String>`，但**边界**
//!   函数必须用 WeclawError
//! - cli/* 保留 `Result<(), String>` —— 直接面向人类，结构化没必要
//!
//! 所有常见外部错误都有 `From` impl：io / reqwest / serde_json / DbError /
//! rusqlite。新代码只用 `?` 就能转换。
//!
//! Design goals:
//!
//! 1. **Categorize for retry logic.** The poller needs to distinguish
//!    "iLink returned 429, sleep and retry" from "iLink token is dead,
//!    give up". `is_retriable()` answers that without string-matching.
//! 2. **Categorize for HTTP responses.** Admin API handlers shouldn't
//!    care whether a failure came from SQLite or a missing FS path —
//!    `http_status()` maps every variant to a sensible 4xx/5xx.
//! 3. **RFC 7807 problem+json output.** GUI clients receive structured
//!    error bodies with a stable `type` URL so they can switch on it.
//! 4. **Cheap to construct from existing types.** Every common error
//!    (`io::Error`, `rusqlite::Error`, `reqwest::Error`, `DbError`) has
//!    a `From` impl so call sites only need `?`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::storage::db::DbError;

#[derive(Debug, thiserror::Error)]
pub enum WeclawError {
    #[error("database: {0}")]
    Db(#[from] DbError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("network: {0}")]
    Network(#[from] reqwest::Error),

    #[error("json: {0}")]
    Serde(#[from] serde_json::Error),

    /// Upstream iLink API returned an error. Carries the original HTTP
    /// status (when applicable) and a `retriable` hint the poller uses
    /// to decide whether to back off vs immediately retry.
    #[error("ilink api: {code} {message}")]
    IlinkApi {
        code: i32,
        message: String,
        retriable: bool,
    },

    /// Caller-supplied input failed validation (path traversal, bad
    /// JSON shape, missing field, etc.). Maps to HTTP 400.
    #[error("invalid input: {0}")]
    BadRequest(String),

    /// The caller tried to authenticate but the credentials were missing
    /// or invalid. Maps to HTTP 401.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Authentication succeeded but the role doesn't permit this
    /// action. Maps to HTTP 403.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Resource (user, account, key) not found. Maps to HTTP 404.
    #[error("not found: {0}")]
    NotFound(String),

    /// State conflict — e.g. trying to revoke an already-revoked key.
    /// Maps to HTTP 409.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Caller exceeded a rate limit. Maps to HTTP 429.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// Anything we can't categorize. Maps to HTTP 500.
    #[error("internal: {0}")]
    Internal(String),
}

// v2.2 L0.2: 兼容性 From impl，让 `?` 在 callsite 自动 work。
// 几个常用的 ad-hoc error 形式（&str / String / rusqlite::Error /
// r2d2::Error）都映射进 Internal —— 接受信息损失换迁移便利性。
// 长期目标是各 module 自己定义 thiserror enum，但 v2.2 优先把
// `Result<_, String>` 转完。

impl From<String> for WeclawError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

impl From<&str> for WeclawError {
    fn from(s: &str) -> Self {
        Self::Internal(s.to_string())
    }
}

// v4.2: rusqlite::Error / r2d2::Error From impls removed — sqlx::Error
// goes through DbError::Pool via repo's manual map_err.
impl From<sqlx::Error> for WeclawError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(DbError::Pool(format!("sqlx: {e}")))
    }
}

impl WeclawError {
    /// Whether the poller should retry after backoff. Anything network /
    /// rate-limit / upstream-with-retriable-hint is retriable; logic
    /// errors (400/401/403/404/409) are not.
    pub fn is_retriable(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::RateLimited(_) => true,
            Self::IlinkApi { retriable, .. } => *retriable,
            Self::Internal(_) => true, // unknown — retry once is reasonable
            _ => false,
        }
    }

    /// HTTP status for this error when surfaced through the admin API.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::IlinkApi { .. } => StatusCode::BAD_GATEWAY,
            // Everything else is server-side.
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable URL identifier for the error category. GUI clients switch
    /// on this so they can render category-specific UI without parsing
    /// the human-readable `detail`.
    pub fn problem_type(&self) -> &'static str {
        match self {
            Self::Db(_) => "https://weclawbot.dev/errors/database",
            Self::Io(_) => "https://weclawbot.dev/errors/io",
            Self::Network(_) => "https://weclawbot.dev/errors/network",
            Self::Serde(_) => "https://weclawbot.dev/errors/json",
            Self::IlinkApi { .. } => "https://weclawbot.dev/errors/ilink-api",
            Self::BadRequest(_) => "https://weclawbot.dev/errors/bad-request",
            Self::Unauthorized(_) => "https://weclawbot.dev/errors/unauthorized",
            Self::Forbidden(_) => "https://weclawbot.dev/errors/forbidden",
            Self::NotFound(_) => "https://weclawbot.dev/errors/not-found",
            Self::Conflict(_) => "https://weclawbot.dev/errors/conflict",
            Self::RateLimited(_) => "https://weclawbot.dev/errors/rate-limited",
            Self::Internal(_) => "https://weclawbot.dev/errors/internal",
        }
    }

    /// Short human-readable title for the error category.
    pub fn problem_title(&self) -> &'static str {
        match self {
            Self::Db(_) => "Database error",
            Self::Io(_) => "I/O error",
            Self::Network(_) => "Network error",
            Self::Serde(_) => "Invalid JSON",
            Self::IlinkApi { .. } => "Upstream WeChat API error",
            Self::BadRequest(_) => "Bad request",
            Self::Unauthorized(_) => "Unauthorized",
            Self::Forbidden(_) => "Forbidden",
            Self::NotFound(_) => "Not found",
            Self::Conflict(_) => "Conflict",
            Self::RateLimited(_) => "Rate limited",
            Self::Internal(_) => "Internal error",
        }
    }

    /// RFC 7807-compatible body.
    pub fn problem_json(&self) -> Value {
        json!({
            "type": self.problem_type(),
            "title": self.problem_title(),
            "status": self.http_status().as_u16(),
            "detail": format!("{self}"),
        })
    }
}

/// Axum response: every handler that returns `Result<_, WeclawError>`
/// gets RFC 7807 problem+json on the error path automatically.
impl IntoResponse for WeclawError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        let body = self.problem_json();
        // Log server-class errors so operators see them even when the
        // client UI swallows the response.
        if status.is_server_error() {
            tracing::error!("api error: {}", self);
        } else {
            tracing::debug!("api error: {}", self);
        }
        (status, Json(body)).into_response()
    }
}

/// Convenience alias for the common case.
pub type WeclawResult<T> = std::result::Result<T, WeclawError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn http_status_categorization() {
        assert_eq!(
            WeclawError::BadRequest("x".into()).http_status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            WeclawError::Unauthorized("x".into()).http_status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            WeclawError::Forbidden("x".into()).http_status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            WeclawError::NotFound("x".into()).http_status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            WeclawError::Conflict("x".into()).http_status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            WeclawError::RateLimited("x".into()).http_status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            WeclawError::Internal("x".into()).http_status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            WeclawError::IlinkApi {
                code: 401,
                message: "expired".into(),
                retriable: false
            }
            .http_status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn is_retriable_classification() {
        assert!(!WeclawError::BadRequest("x".into()).is_retriable());
        assert!(!WeclawError::NotFound("x".into()).is_retriable());
        assert!(WeclawError::RateLimited("x".into()).is_retriable());
        assert!(WeclawError::IlinkApi {
            code: 503,
            message: "service unavailable".into(),
            retriable: true
        }
        .is_retriable());
        assert!(!WeclawError::IlinkApi {
            code: 401,
            message: "token revoked".into(),
            retriable: false
        }
        .is_retriable());
    }

    #[test]
    fn problem_json_shape() {
        let e = WeclawError::NotFound("user u-abc".into());
        let body = e.problem_json();
        assert_eq!(body["status"], 404);
        assert!(body["type"]
            .as_str()
            .unwrap()
            .contains("not-found"));
        assert_eq!(body["title"], "Not found");
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("user u-abc"));
    }

    #[test]
    fn from_db_error_via_question_mark() {
        // The conversion compiles — that's the actual coverage we want
        // from this test. Constructing a `DbError` directly through
        // public constructors is the cleanest way.
        let dbe = DbError::NotFound;
        let we: WeclawError = dbe.into();
        assert!(matches!(we, WeclawError::Db(_)));
        assert_eq!(we.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
