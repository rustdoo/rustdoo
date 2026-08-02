//! rusdoo — the server binary, port of `odoo-bin`.

use rusdoo_http::dispatch::OrmService;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
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
    let mut access = rusdoo_orm::access::AccessControl::new();
    let mut rules = rusdoo_orm::rules::RecordRules::new();
    let mut assets = rusdoo_http::assets::AssetHub::empty();

    let addons = std::env::var("RUSDOO_ADDONS_PATH").unwrap_or_else(|_| "addons".into());
    let addons_path = std::path::Path::new(&addons);

    // What the addons ship to the browser is read off the filesystem, so
    // it is resolved on every boot — a server restarted without --init
    // still serves its client.
    if addons_path.is_dir() {
        let (bundles, roots) = rusdoo_modules::assets::resolve_installed(&[addons_path])?;
        tracing::info!("{} client bundle(s) resolved", bundles.names().count());
        assets = rusdoo_http::assets::AssetHub::new(bundles, roots);
    }

    if std::env::args().any(|arg| arg == "--init") {
        use rusdoo_modules::installer::{install_modules, XmlIds};
        let mut xml_ids = XmlIds::load(&pool).await?;
        if addons_path.is_dir() {
            let report =
                install_modules(&pool, &mut registry, &[addons_path], &mut xml_ids).await?;
            tracing::info!(
                "installed {} module(s), {} client bundle(s)",
                report.modules.len(),
                report.bundles.names().count()
            );
            access = report.access;
            rules = report.rules;
            seed_admin(&registry, &pool).await?;
        } else {
            for model in registry.models() {
                model.init_table(&pool).await?;
            }
            tracing::info!("schema initialized (no addons directory found)");
            seed_admin(&registry, &pool).await?;
        }
    }

    if access.is_empty() {
        // fail-closed: with no ACL rules only the superuser reaches models
        tracing::warn!(
            "nenhuma regra ir.model.access carregada: apenas o superusuário (uid 1) \
             acessa modelos; usuários comuns ficam bloqueados até as ACLs serem carregadas \
             (rode com --init sobre addons que tragam ir.model.access.csv)"
        );
    }
    let mut service = OrmService::new(Arc::new(registry), pool)
        .with_access(access)
        .with_rules(rules)
        .with_assets(assets);
    if std::env::var("RUSDOO_INSECURE_COOKIES").is_ok() {
        service = service.allow_insecure_cookies();
    }
    // o endereço é configurável: uma máquina que já roda um Odoo tem a
    // 8069 ocupada, e um container publica em outra interface
    let addr = std::env::var("RUSDOO_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    tracing::info!("rusdoo listening on {addr} (/web, /jsonrpc, /web/dataset/call_kw)");
    rusdoo_http::serve(&addr, service).await?;
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
            name: "ir.ui.view".into(),
            table: "ir_ui_view".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("model", FieldType::Char { size: None }),
            // form/list/kanban/...: which view the client asks for by type
            Field::new("type", FieldType::Char { size: None }).default_value(json!("form")),
            // lowest wins when several views share a model and type
            Field::new("priority", FieldType::Integer).default_value(json!(16)),
            Field::new("arch", FieldType::Text),
        ],
    ))?;
    reg.register(Model::new(
        ModelMeta {
            name: "ir.actions.act_window".into(),
            table: "ir_act_window".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("res_model", FieldType::Char { size: None }).required(),
            Field::new("view_mode", FieldType::Char { size: None }),
            Field::new("domain", FieldType::Text),
        ],
    ))?;
    reg.register(Model::new(
        ModelMeta {
            name: "ir.ui.menu".into(),
            table: "ir_ui_menu".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "ir.ui.menu".into(),
                },
            ),
            Field::new("sequence", FieldType::Integer),
            // external id of the act_window this menu opens (Odoo uses a
            // reference field; we bridge with the xml_id string for now)
            Field::new("action", FieldType::Char { size: None }),
        ],
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
            Field::new("password", FieldType::Char { size: None }).private(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // the context every call of this user inherits
            Field::new("lang", FieldType::Char { size: None }).default_value(json!("en_US")),
            Field::new("tz", FieldType::Char { size: None }),
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
            // como todo modelo arquivável do Odoo: nasce ativo, senão
            // cada registro novo já viria arquivado
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
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
