//! rodoo-orm — port of `odoo/orm/` (models, fields, domains) on PostgreSQL.

pub mod domain;
pub mod fields;

/// Mirrors `odoo.models.BaseModel` metadata (`_name`, `_table`, `_inherit`).
#[derive(Debug, Clone)]
pub struct ModelMeta {
    pub name: String,
    pub table: String,
    pub inherit: Vec<String>,
}
