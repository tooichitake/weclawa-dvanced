//! Admin API key lifecycle — generation, verification, bootstrap.
//!
//! ## Key shape
//!
//! Plaintext key format: `weclawbot_<base32_prefix>_<base32_secret>`
//! where:
//!
//! - `prefix` = first 4 bytes of the secret, base32-encoded (8 chars).
//!   Stored in the clear inside the database row (`name` is operator-set,
//!   but the prefix is appended to support fast lookup before verify).
//! - `secret` = full 32 random bytes, base32-encoded (52 chars). Hashed
//!   with argon2id and stored as `key_hash`. **Never** persisted in
//!   plaintext.
//!
//! The argon2id parameters use library defaults (m=19456, t=2, p=1) which
//! the PHC team picked to balance memory cost against verify latency.
//! That's ~75 ms per verify on a modern x86 server — fine for an admin
//! API where requests-per-second is in the low hundreds.
//!
//! ## Verification
//!
//! `verify_and_load(plaintext)` does a full O(n) scan of active keys,
//! argon2-verifying each one. SQLite's `idx_admin_keys_active` partial
//! index keeps the scan cheap when most keys are revoked; an
//! installation with 1-10 active keys verifies in <100 ms even though
//! the cost is dominated by argon2 (memory-hard by design).
//!
//! A faster path keyed on the visible prefix is a deliberate
//! optimization deferred to Phase 6 — until then operators rotating
//! keys frequently is the more common shape, and constant-time scan
//! avoids leaking key existence through timing.

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use data_encoding::BASE32_NOPAD;
use rand::TryRngCore;

use crate::repo::admin_keys::{AdminKeyRecord, Role};
use crate::repo::admin_keys_async::SqlxAdminKeyRepo;

/// Decoded result of a successful `verify_and_load` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminContext {
    pub key_id: String,
    pub key_name: String,
    pub role: Role,
    /// v3 multi-tenant: 此 key 归属的 tenant。v2.2 / 单 operator 部署
    /// 都是 "default"。axum middleware 把它注入 request extensions,
    /// downstream repo 用它过滤数据 — 当前 callsite 还不需要主动用，
    /// 但字段已就位避免 v3 切换时全文件改 struct 字面。
    pub tenant_id: String,
}

/// Output of `mint_new_key`: the plaintext (to surface to the operator
/// **once**) plus the corresponding stored record.
#[derive(Debug, Clone)]
pub struct MintedKey {
    pub plaintext: String,
    pub record: AdminKeyRecord,
}

/// Async mint — `Sqlx*Repo` 版本。argon2 hash 是 CPU bound，包在
/// `spawn_blocking` 里。
pub async fn mint_new_key_async(
    pool: crate::storage::db_async::AsyncDbPool,
    name: &str,
    role: Role,
) -> Result<MintedKey, String> {
    use chrono::Utc;
    let name = name.to_string();
    let mint_inner = tokio::task::spawn_blocking(move || {
        let mut secret_bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut secret_bytes)
            .map_err(|e| format!("rng: {e}"))?;
        let prefix = BASE32_NOPAD
            .encode(&secret_bytes[..4])
            .to_ascii_lowercase();
        let secret_b32 = BASE32_NOPAD.encode(&secret_bytes).to_ascii_lowercase();
        let plaintext = format!("weclawbot_{prefix}_{secret_b32}");
        let hash = hash_key(&plaintext)?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let record = AdminKeyRecord {
            id,
            name: name.clone(),
            key_hash: hash,
            role,
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            tenant_id: crate::tenancy::DEFAULT_TENANT.to_string(),
        };
        Ok::<_, String>(MintedKey { plaintext, record })
    })
    .await
    .map_err(|e| format!("mint blocking: {e}"))??;

    // sqlx insert
    sqlx::query(
        "INSERT INTO admin_keys
             (id, name, key_hash, role, created_at, last_used_at, revoked_at, tenant_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&mint_inner.record.id)
    .bind(&mint_inner.record.name)
    .bind(&mint_inner.record.key_hash)
    .bind(mint_inner.record.role.as_str())
    .bind(&mint_inner.record.created_at)
    .bind(&mint_inner.record.last_used_at)
    .bind(&mint_inner.record.revoked_at)
    .bind(&mint_inner.record.tenant_id)
    .execute(&pool)
    .await
    .map_err(|e| format!("insert: {e}"))?;
    Ok(mint_inner)
}

/// v4.2: async verify — sqlx list_active + spawn_blocking 包 argon2 verify。
pub async fn verify_and_load_async(plaintext: &str) -> Result<Option<AdminContext>, String> {
    if !plaintext.starts_with("weclawbot_") {
        return Ok(None);
    }
    let Some(pool) = crate::storage::db_async::try_global_async_pool() else {
        return Err("state.db not initialized".into());
    };
    let repo = SqlxAdminKeyRepo::new(pool.clone());
    let actives = repo.list_active().await.map_err(|e| format!("list: {e}"))?;

    let plaintext_owned = plaintext.to_string();
    // argon2 verify loop is CPU bound — block_in_place 推到 blocking pool。
    // 也确保 timing-leak fix: 跑完整组才返回，不早退。
    let matched = tokio::task::spawn_blocking(move || {
        let mut matched: Option<AdminContext> = None;
        for rec in actives {
            let is_match = verify_key(&plaintext_owned, &rec.key_hash).unwrap_or(false);
            if is_match && matched.is_none() {
                matched = Some(AdminContext {
                    key_id: rec.id,
                    key_name: rec.name,
                    role: rec.role,
                    tenant_id: rec.tenant_id,
                });
            }
        }
        matched
    })
    .await
    .map_err(|e| format!("verify blocking: {e}"))?;

    if let Some(ref ctx) = matched {
        let _ = repo.touch_last_used(&ctx.key_id).await;
    }
    Ok(matched)
}

fn hash_key(plaintext: &str) -> Result<String, String> {
    // Generate 16 bytes of salt entropy via the OS RNG, then base64-encode
    // to the SaltString format argon2 expects. Using rand::rngs::OsRng
    // matches the rand 0.9 + argon2 0.5 surface (the rand_core re-export
    // moved between releases — go through OsRng directly to stay portable).
    let mut salt_raw = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut salt_raw)
        .map_err(|e| format!("rng: {e}"))?;
    let salt = SaltString::encode_b64(&salt_raw)
        .map_err(|e| format!("salt encode: {e}"))?;
    let argon = Argon2::default();
    argon
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("argon2 hash: {e}"))
}

fn verify_key(plaintext: &str, stored_hash: &str) -> Result<bool, String> {
    let parsed = match PasswordHash::new(stored_hash) {
        Ok(p) => p,
        Err(_) => return Ok(false), // corrupt row — fail closed
    };
    Ok(Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

/// Idempotent bootstrap: if the daemon has zero active super_admin
/// keys, mint one and surface it to the operator. Returns the plaintext
/// when a fresh key is created; `None` means an active super_admin
/// already exists and this is a no-op.
///
/// The plaintext is also written to `~/.weclawbot/INITIAL-ADMIN-KEY.txt`
/// with mode 0600 so the operator can recover it from disk if they miss
/// the stdout banner.
pub async fn ensure_bootstrap_super_admin(
    pool: crate::storage::db_async::AsyncDbPool,
) -> Result<Option<String>, String> {
    let repo = SqlxAdminKeyRepo::new(pool.clone());
    let n = repo
        .count_active_super_admin()
        .await
        .map_err(|e| format!("count: {e}"))?;
    if n > 0 {
        return Ok(None);
    }
    let minted = mint_new_key_async(pool, "bootstrap", Role::SuperAdmin).await?;
    write_bootstrap_file(&minted.plaintext);
    print_bootstrap_banner(&minted.plaintext);
    Ok(Some(minted.plaintext))
}

fn print_bootstrap_banner(plaintext: &str) {
    eprintln!();
    eprintln!("================================================================");
    eprintln!(" weclawbot: initial admin API key (super_admin)");
    eprintln!();
    eprintln!("   {plaintext}");
    eprintln!();
    eprintln!(" This key is shown ONCE. Save it now — you'll need it to log");
    eprintln!(" into the admin GUI and to call /api/v1/*. A copy was also");
    eprintln!(" written to ~/.weclawbot/INITIAL-ADMIN-KEY.txt (mode 0600).");
    eprintln!("================================================================");
    eprintln!();
}

fn write_bootstrap_file(plaintext: &str) {
    let path = crate::storage::state_dir::state_dir().join("INITIAL-ADMIN-KEY.txt");
    if let Err(e) = std::fs::write(&path, format!("{plaintext}\n")) {
        tracing::warn!("write INITIAL-ADMIN-KEY.txt: {e}");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("chmod 0600 INITIAL-ADMIN-KEY.txt: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn install_pool() -> crate::storage::db_async::AsyncDbPool {
        let pool = db_async::open_in_memory().await.unwrap();
        db_async::set_global_async_pool(pool.clone());
        pool
    }

    #[tokio::test]
    async fn mint_returns_plaintext_with_expected_shape() {
        let pool = db_async::open_in_memory().await.unwrap();
        let minted = mint_new_key_async(pool, "ci", Role::ReadOnly)
            .await
            .unwrap();
        assert!(minted.plaintext.starts_with("weclawbot_"));
        let parts: Vec<&str> = minted.plaintext.split('_').collect();
        assert_eq!(parts.len(), 3, "weclawbot_<prefix>_<secret>");
        assert_eq!(parts[1].len(), 7);
        assert_eq!(parts[2].len(), 52);
    }

    // 后续测试用 global pool — verify_and_load_async 通过全局 sqlx pool 取 repo
    // (`try_global_async_pool`)。每个测试要先 install 一份新 in-mem pool。
    // 注意：全局 OnceLock — 整个测试进程共享，但单进程内 install 顺序对
    // 不同测试无所谓（只有 default tenant 一个 row）。

    #[tokio::test]
    async fn verify_round_trip_with_correct_role() {
        let pool = install_pool().await;
        let minted = mint_new_key_async(pool, "alice", Role::ReadWrite)
            .await
            .unwrap();
        let ctx = verify_and_load_async(&minted.plaintext).await.unwrap();
        let ctx = ctx.expect("should verify");
        assert_eq!(ctx.key_name, "alice");
        assert_eq!(ctx.role, Role::ReadWrite);
    }
}
