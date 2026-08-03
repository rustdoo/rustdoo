//! rusdoo-orm — port of `odoo/orm/` (models, fields, domains) on PostgreSQL.

pub mod access;
pub mod config;
pub mod context;
pub use context::DEFAULT_LANG;
pub mod crud;
pub mod defaults;
pub mod db;
pub mod ddl;
pub mod domain;
pub mod fields;
pub mod group;
pub mod methods;
pub mod model;
pub mod registry;
pub mod rules;
pub mod sequence;
pub mod sql;
pub mod translations;
pub mod unlink;
