//! Admin key types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::admin_keys_async`].

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    SuperAdmin,
    ReadWrite,
    ReadOnly,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::SuperAdmin => "super_admin",
            Role::ReadWrite => "read_write",
            Role::ReadOnly => "read_only",
        }
    }

    /// Unknown values map to ReadOnly (fail-closed).
    pub fn from_str(s: &str) -> Role {
        match s {
            "super_admin" => Role::SuperAdmin,
            "read_write" => Role::ReadWrite,
            _ => Role::ReadOnly,
        }
    }

    /// Returns true if `self` is at least as privileged as `min`.
    pub fn allows(self, min: Role) -> bool {
        let rank = |r: Role| match r {
            Role::ReadOnly => 1,
            Role::ReadWrite => 2,
            Role::SuperAdmin => 3,
        };
        rank(self) >= rank(min)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminKeyRecord {
    pub id: String,
    pub name: String,
    pub key_hash: String,
    pub role: Role,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub tenant_id: String,
}

impl AdminKeyRecord {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}
