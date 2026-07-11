//! Sessions and password hashing, port of odoo/http.py sessions +
//! res.users credential checks.
//!
//! Divergence: passwords hash with Argon2id (Odoo uses pbkdf2-sha512);
//! a verification shim for migrated Odoo databases comes later.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusdoo_core::RusdooError;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// Odoo's SUPERUSER_ID — bypasses access control.
pub const SUPERUSER_ID: i64 = 1;

#[derive(Debug, Clone)]
pub struct Session {
    pub uid: i64,
    pub login: String,
    pub is_superuser: bool,
    /// res.groups ids the user belongs to (empty until group m2m read
    /// lands; superuser bypass keeps admin usable meanwhile)
    pub groups: Vec<i64>,
}

/// In-memory session store (server restarts log everyone out).
#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uid: i64, login: &str, groups: Vec<i64>) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                token.clone(),
                Session {
                    uid,
                    login: to_owned_login(login),
                    is_superuser: uid == SUPERUSER_ID,
                    groups,
                },
            );
        token
    }

    pub fn get(&self, token: &str) -> Option<Session> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }

    pub fn close(&self, token: &str) {
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(token);
    }
}

fn to_owned_login(login: &str) -> String {
    login.to_string()
}

pub fn hash_password(plain: &str) -> Result<String, RusdooError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| RusdooError::Validation(format!("password hash: {e}")))
}

/// A fixed valid Argon2 hash used to spend the same work on the
/// user-not-found path, so login timing cannot enumerate accounts.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("rusdoo-not-a-real-password").expect("hash dummy"))
}

pub fn verify_password(plain: &str, hashed: &str) -> bool {
    PasswordHash::new(hashed)
        .map(|parsed| {
            Argon2::default()
                .verify_password(plain.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}
