//! Strongly-typed identifiers used across module boundaries.
//!
//! ## Adoption status (post Phase B)
//!
//! These newtypes are **defined and tested but not yet threaded through all
//! 25+ existing call sites** that still take `&str`. Rationale: a one-shot
//! bulk retrofit is wide and high-risk for marginal benefit while the rest
//! of the daemon is still being reshaped. New code should accept newtypes
//! at module boundaries; old `&str` signatures get converted opportunistically
//! when a function is touched for another reason.
//!
//! `BotToken` is the highest-priority candidate (its redacting `Debug` /
//! `Display` is the only concrete security win) and should be threaded
//! first whenever a token-handling function is next modified.
//!
//! ## Background
//!
//! Previously the codebase passed everything as `&str`:
//!
//! ```ignore
//! handle_inbound_message(client, account_id, token, base_url, msg)
//! ```
//!
//! Easy to swap two by accident (e.g. pass the user's `WeixinUserId` where a
//! `BotToken` is expected) and the compiler is silent. The newtype wrappers
//! here make each kind distinct at the type level so the compiler rejects
//! mismatches.
//!
//! Conventions:
//! - All wrap a `String` and provide cheap `as_str() -> &str` access.
//! - `Display` prints the inner value verbatim — fine for logs of non-secret
//!   IDs (`AccountId`, `UserHash`, `WeixinUserId`, `BaseUrl`).
//! - `BotToken` deliberately does NOT print verbatim; its `Debug` and
//!   `Display` only show the first/last 4 chars, matching the old
//!   `redact_token` behavior (which had 0 callers so we removed the function
//!   and made the type itself enforce redaction).
//! - All types implement `Serialize` / `Deserialize` transparently so they
//!   slot into the existing JSON config files without schema changes.
//! - `From<String>` and `From<&str>` for ergonomics at construction sites;
//!   `AsRef<str>` for read-only consumers (reqwest URL builder etc.).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[inline]
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            #[inline]
            pub fn as_str(&self) -> &str { &self.0 }
            #[inline]
            pub fn into_string(self) -> String { self.0 }
            // v7.0 housekeeping: macro-generated `is_empty()` removed —
            // zero callers on any of the four newtypes (`AccountId`,
            // `UserHash`, `WeixinUserId`, `BaseUrl`). Callers test the
            // underlying `String` directly via `.as_str().is_empty()`
            // when needed.
        }

        impl AsRef<str> for $name {
            #[inline]
            fn as_ref(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl From<String> for $name {
            #[inline]
            fn from(s: String) -> Self { Self(s) }
        }

        impl From<&str> for $name {
            #[inline]
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
    };
}

id_newtype!(
    /// The local registry key used in `~/.weclawbot/accounts/<account_id>.json`
    /// and in agent-binding routing. Format: `<short-hash>-im-bot`, e.g.
    /// `0085bbeff2be-im-bot`.
    AccountId
);

id_newtype!(
    /// SHA-1 derived per-user directory name, e.g. `u-44a160bb371f`. Used as
    /// the on-disk key for `~/.weclawbot/users/<UserHash>/`.
    UserHash
);

id_newtype!(
    /// The opaque iLink user identifier as WeChat reports it, e.g.
    /// `o9cq80w4SV2dM86NKEesugLM7vjU@im.wechat`. Stable per user.
    WeixinUserId
);

id_newtype!(
    /// Base URL of the iLink endpoint, e.g. `https://ilinkai.weixin.qq.com`.
    /// Stored per-account because future regions could split.
    BaseUrl
);

/// OAuth-like bot token returned by iLink at login time. Sensitive — Debug
/// and Display deliberately show only first 4 + last 4 chars so the value
/// can't leak into logs accidentally.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BotToken(String);

impl BotToken {
    #[inline]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// **DANGER**: returns the full token. Only use to pass to the iLink
    /// HTTP client. Never log or display the result.
    #[inline]
    pub fn expose(&self) -> &str {
        &self.0
    }
    // v7.0 housekeeping: `is_empty()` removed — zero callers.
}

impl fmt::Debug for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BotToken({})", redact(&self.0))
    }
}

impl fmt::Display for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact(&self.0))
    }
}

impl From<String> for BotToken {
    #[inline]
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BotToken {
    #[inline]
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

fn redact(s: &str) -> String {
    let n = s.chars().count();
    if n <= 8 {
        return "[hidden]".to_string();
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_round_trip() {
        let a = AccountId::new("0085bbeff2be-im-bot");
        assert_eq!(a.as_str(), "0085bbeff2be-im-bot");
        assert_eq!(format!("{a}"), "0085bbeff2be-im-bot");
    }

    #[test]
    fn bot_token_never_leaks_in_display() {
        let t = BotToken::new("super_secret_token_abcdef0123456789");
        let s = format!("{t}");
        let d = format!("{t:?}");
        assert!(!s.contains("secret"));
        assert!(!d.contains("secret"));
        // ...but the head + tail markers are visible:
        assert!(s.contains("supe"));
        assert!(s.contains("6789"));
        // expose() returns the real value
        assert_eq!(t.expose(), "super_secret_token_abcdef0123456789");
    }

    #[test]
    fn bot_token_short_input_fully_hidden() {
        let t = BotToken::new("short");
        assert_eq!(format!("{t}"), "[hidden]");
    }

    #[test]
    fn serde_transparent() {
        let a = AccountId::new("hello");
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#""hello""#);
        let b: AccountId = serde_json::from_str(r#""world""#).unwrap();
        assert_eq!(b.as_str(), "world");
    }

    #[test]
    fn ids_are_distinct_types() {
        let _a = AccountId::new("x");
        let _u = UserHash::new("x");
        // Compile-time guarantee: the following would NOT compile:
        // let _: AccountId = _u;
    }
}
