//! rusdoo — the server binary, port of `odoo-bin`.

use rusdoo_http::dispatch::OrmService;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use std::sync::Arc;

const DEFAULT_ADDR: &str = "0.0.0.0:8069";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let db_url = std::env::var("RUSDOO_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| anyhow::anyhow!("set RUSDOO_DATABASE_URL or DATABASE_URL"))?;

    let registry = base_registry()?;
    let pool = rusdoo_orm::db::connect(&db_url).await?;

    if std::env::args().any(|arg| arg == "--init") {
        for name in ["res.company", "res.partner"] {
            registry
                .get(name)
                .expect("registered above")
                .init_table(&pool)
                .await?;
            tracing::info!("initialized table for {name}");
        }
    }

    let service = OrmService::new(Arc::new(registry), pool);
    tracing::info!("rusdoo listening on {DEFAULT_ADDR} (/jsonrpc, /web/dataset/call_kw)");
    rusdoo_http::serve(DEFAULT_ADDR, service).await?;
    Ok(())
}

/// Minimal built-in models so the server is usable end-to-end.
/// Placeholder until module loading (Phase 3) registers real addons.
fn base_registry() -> anyhow::Result<Registry> {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.company".into(),
            table: "res_company".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None }).required()],
    ))?;
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("email", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "res.company".into(),
                },
            ),
        ],
    ))?;
    Ok(reg)
}
