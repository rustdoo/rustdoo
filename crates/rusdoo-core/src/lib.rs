//! rusdoo-core — port of `odoo/` core primitives: registry, environment,
//! exceptions (`odoo/exceptions.py`), and shared types.

pub mod error;

pub use error::RusdooError;

/// Odoo's `SUPERUSER_ID`: the acting user for install, seeding and any
/// unattributed ORM write. Bypasses access control and is stamped on the
/// `create_uid`/`write_uid` audit columns when no session user is known.
pub const SUPERUSER_ID: i64 = 1;

/// Mirrors `odoo.api.Environment`: carries db cursor, user id and context.
#[derive(Debug, Clone)]
pub struct Environment {
    pub uid: i64,
    pub context: serde_json::Map<String, serde_json::Value>,
}
