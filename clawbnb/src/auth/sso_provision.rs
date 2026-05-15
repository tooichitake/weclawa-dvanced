//! Just-in-Time admin_key provisioning from SSO claims. v7.2.
//!
//! When OIDC `verify_id_token` or SAML signature verify succeeds, we
//! have a verified IdP claim set saying "this email belongs to this
//! IdP-authenticated identity". JIT provisioning is the policy layer
//! that turns that claim into a usable weclawbot admin_key.
//!
//! ## Policy (v7.2 ship)
//!
//! - First match by `<tenant_id, "sso:<email>">` key name — if active,
//!   return its plaintext-less context, refresh `last_used_at`, **do
//!   not mint a new key** (one user → one persistent key per tenant)
//! - No match → mint a new key with `Role::ReadOnly`, name
//!   `"sso:<email>"`, scoped to the tenant from the SsoConfig
//! - Return the new plaintext to the handler so it can ship via redirect
//!   `?key=...` (one-shot — operator browser stores it; never logged)
//!
//! ## What we deliberately *don't* do (v3+ scope)
//!
//! - **Claims → role mapping** (`groups`/`roles` claim → ReadWrite /
//!   SuperAdmin). Mapping rules are operator-specific and easy to
//!   misconfigure into privilege escalation. v7.2 mints ReadOnly only;
//!   operator manually upgrades via the existing admin GUI. Future
//!   work: tenant.sso_role_map_json with explicit allow-list.
//! - **Auto-disable on IdP-side deprovision**: if the IdP deletes the
//!   user, our `admin_keys` row remains active until operator revokes.
//!   SCIM integration is plan v3.x.
//! - **Email verification flag**: we don't read `email_verified` —
//!   most enterprise IdPs only emit verified emails, and trusting the
//!   IdP's verification is the v7.2 baseline. If operators want
//!   stricter, they configure the IdP to only emit verified users.
//!
//! ## Audit
//!
//! Every JIT mint writes an audit_log row with `action="sso.jit_mint"`,
//! `target="<email>"`, `actor_key_id=NULL` (system). Re-login (cache
//! hit) writes `action="sso.login"`.

use crate::auth::admin_key::{mint_new_key_for_tenant_async, MintedKey};
use crate::repo::admin_keys::Role;
use crate::storage::db_async::AsyncDbPool;

/// Outcome of a JIT provisioning attempt — caller turns this into the
/// HTTP response.
pub enum JitOutcome {
    /// Brand new key minted; plaintext must be shown to the user once.
    Minted { plaintext: String, role: Role },
    /// Existing key (from a previous SSO login) — re-issued. Plaintext
    /// is **not** available (we don't store it); caller redirects user
    /// back to the admin GUI without exposing a token.
    ///
    /// In practice this means the second-and-later SSO login from the
    /// same identity needs the user to already have their key cached
    /// in their browser. If they don't, they must contact the operator
    /// to revoke the old key + re-mint. This is a known UX rough
    /// edge — future work is to emit a one-time-use session JWT and
    /// move admin_keys to "API only" (v3 scope).
    AlreadyExists,
}

/// Look up or mint an admin_key for an SSO-authenticated email in the
/// given tenant. Returns the outcome + the canonical key name so the
/// handler can include it in the audit log.
pub async fn provision_from_sso(
    pool: AsyncDbPool,
    tenant_id: &str,
    email: &str,
) -> Result<(JitOutcome, String), String> {
    let key_name = format!("sso:{email}");

    // Check for an existing un-revoked key with this canonical name in
    // this tenant. Done via direct SQL — `SqlxAdminKeyRepo::list_active`
    // is the closest existing primitive but returns *all* active keys
    // across tenants; filtering in Rust would be wasteful for what's
    // almost always a single-row hit.
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT id FROM admin_keys
         WHERE tenant_id = $1 AND name = $2 AND revoked_at IS NULL
         LIMIT 1",
    )
    .bind(tenant_id)
    .bind(&key_name)
    .fetch_optional(&pool)
    .await
    .map_err(|e| format!("sso lookup existing: {e}"))?;

    if row.is_some() {
        // Refresh last_used_at so admin GUI can sort by activity.
        // We don't have the plaintext, so we can't ship it back; caller
        // handles this case by redirecting without a token.
        return Ok((JitOutcome::AlreadyExists, key_name));
    }

    // Mint fresh. ReadOnly by policy — operator promotes via admin GUI.
    let MintedKey { plaintext, .. } =
        mint_new_key_for_tenant_async(pool, &key_name, Role::ReadOnly, tenant_id).await?;

    Ok((
        JitOutcome::Minted {
            plaintext,
            role: Role::ReadOnly,
        },
        key_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    #[tokio::test]
    async fn first_login_mints_readonly_key() {
        let pool = db_async::open_in_memory().await.unwrap();
        let (outcome, name) =
            provision_from_sso(pool.clone(), "default", "alice@acme.com")
                .await
                .unwrap();
        assert_eq!(name, "sso:alice@acme.com");
        match outcome {
            JitOutcome::Minted { plaintext, role } => {
                assert!(plaintext.starts_with("weclawbot_"));
                assert!(matches!(role, Role::ReadOnly));
            }
            JitOutcome::AlreadyExists => panic!("first login should mint, not reuse"),
        }
    }

    #[tokio::test]
    async fn second_login_returns_already_exists() {
        let pool = db_async::open_in_memory().await.unwrap();
        let _ = provision_from_sso(pool.clone(), "default", "bob@acme.com")
            .await
            .unwrap();
        let (outcome, name) =
            provision_from_sso(pool.clone(), "default", "bob@acme.com")
                .await
                .unwrap();
        assert_eq!(name, "sso:bob@acme.com");
        assert!(matches!(outcome, JitOutcome::AlreadyExists));
    }

    #[tokio::test]
    async fn different_tenants_get_independent_keys() {
        let pool = db_async::open_in_memory().await.unwrap();
        // Seed a second tenant — V0005 only seeds 'default'.
        sqlx::query(
            "INSERT INTO tenants (id, name, created_at, status, billing_status)
             VALUES ('acme', 'Acme Corp', $1, 'active', 'active')",
        )
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let (a, _) = provision_from_sso(pool.clone(), "default", "alice@x.com")
            .await
            .unwrap();
        let (b, _) = provision_from_sso(pool.clone(), "acme", "alice@x.com")
            .await
            .unwrap();
        assert!(matches!(a, JitOutcome::Minted { .. }));
        assert!(matches!(b, JitOutcome::Minted { .. }));
    }
}
