//! rusdoo-core — port of `odoo/` core primitives: registry, environment,
//! exceptions (`odoo/exceptions.py`), and shared types.

pub mod error;

pub use error::RusdooError;

/// Mirrors `odoo.api.Environment`: carries db cursor, user id and context.
#[derive(Debug, Clone)]
pub struct Environment {
    pub uid: i64,
    pub context: serde_json::Map<String, serde_json::Value>,
}
