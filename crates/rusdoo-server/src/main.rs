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

    let mut registry = base_registry()?;
    let pool = rusdoo_orm::db::connect(&db_url).await?;

    if std::env::args().any(|arg| arg == "--init") {
        use rusdoo_modules::installer::{install_modules, XmlIds};
        let addons = std::env::var("RUSDOO_ADDONS_PATH").unwrap_or_else(|_| "addons".into());
        let addons_path = std::path::Path::new(&addons);
        let mut xml_ids = XmlIds::load(&pool).await?;
        if addons_path.is_dir() {
            let report =
                install_modules(&pool, &mut registry, &[addons_path], &mut xml_ids).await?;
            tracing::info!("installed {} module(s)", report.modules.len());
            seed_admin(&registry, &pool).await?;
        } else {
            for model in registry.models() {
                model.init_table(&pool).await?;
            }
            tracing::info!("schema initialized (no addons directory found)");
            seed_admin(&registry, &pool).await?;
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
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean),
        ],
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

/// First boot: create the admin user (login admin / password admin).
async fn seed_admin(registry: &Registry, pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use rusdoo_orm::crud::SearchOptions;
    let domain = rusdoo_orm::domain::parse_domain(&serde_json::json!([["login", "=", "admin"]]))?;
    let existing = registry
        .search(pool, "res.users", &domain, &SearchOptions::default())
        .await?;
    if existing.is_empty() {
        let hash = rusdoo_http::session::hash_password("admin")?;
        registry
            .create(
                pool,
                "res.users",
                vec![
                    ("login", serde_json::json!("admin")),
                    ("password", serde_json::json!(hash)),
                    ("active", serde_json::json!(true)),
                ],
            )
            .await?;
        tracing::warn!("usuário admin criado (login: admin / senha: admin) — troque a senha");
    }
    Ok(())
}
