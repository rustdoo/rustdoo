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

#[derive(Debug, Clone)]
pub struct Session {
    pub uid: i64,
    pub login: String,
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

    pub fn open(&self, uid: i64, login: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                token.clone(),
                Session {
                    uid,
                    login: to_owned_login(login),
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
