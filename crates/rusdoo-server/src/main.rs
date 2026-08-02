//! rusdoo — the server binary, port of `odoo-bin`.

use rusdoo_http::dispatch::OrmService;
use rusdoo_orm::registry::Registry;
use std::sync::Arc;

const DEFAULT_ADDR: &str = "0.0.0.0:8069";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let db_url = std::env::var("RUSDOO_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| anyhow::anyhow!("set RUSDOO_DATABASE_URL or DATABASE_URL"))?;

    let pool = rusdoo_orm::db::connect(&db_url).await?;
    let mut assets = rusdoo_http::assets::AssetHub::empty();

    let addons = std::env::var("RUSDOO_ADDONS_PATH").unwrap_or_else(|_| "addons".into());
    let addons_path = std::path::Path::new(&addons);

    // A module is code plus data: the models of the addons present on
    // disk are registered here, in dependency order, before their data
    // files are allowed to speak about them.
    let mut registry = code_registry(addons_path)?;
    // the methods those same modules attach to their models
    let methods = code_methods(addons_path)?;

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
            // the groups the base addon just published, so the admin is
            // a member of them and not only a uid the ACL waves through
            let groups: Vec<i64> = ["base.group_user", "base.group_system"]
                .iter()
                .filter_map(|xml_id| xml_ids.get(xml_id).map(|(_, id)| *id))
                .collect();
            seed_admin(&registry, &pool, &groups).await?;
        } else {
            for model in registry.models() {
                model.init_table(&pool).await?;
            }
            tracing::info!("schema initialized (no addons directory found)");
            seed_admin(&registry, &pool, &[]).await?;
        }
    }

    // The ACL and the record rules are rows, not install-time state:
    // every boot reads them back, so a restart without --init is still a
    // server that knows who may do what.
    let access = rusdoo_orm::access::AccessControl::load(&pool).await?;
    let rules = rusdoo_orm::rules::RecordRules::load(&pool).await?;

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
        .with_assets(assets)
        .with_methods(methods);
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

/// What a compiled-in module does to the registry.
type ModelProvider = fn(&mut Registry) -> Result<(), rusdoo_core::RusdooError>;

/// The models a compiled-in module contributes. A module whose addon is
/// not on disk registers nothing: its tables have no reason to exist.
fn code_modules() -> Vec<(&'static str, ModelProvider)> {
    vec![
        ("base", rusdoo_base::extend as ModelProvider),
        ("mail", rusdoo_mail::extend as ModelProvider),
        ("product", rusdoo_product::extend as ModelProvider),
        ("account", rusdoo_account::extend as ModelProvider),
        ("sale", rusdoo_sale::extend as ModelProvider),
    ]
}

/// Build the registry out of the code modules whose addon is installed.
/// Without an addons directory only `base` is registered — enough for a
/// server to answer, and honest about what it has.
fn code_registry(addons_path: &std::path::Path) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();
    let providers = code_modules();
    for name in installed_code_modules(addons_path)? {
        let extend = providers
            .iter()
            .find(|(module, _)| *module == name)
            .expect("named a known module")
            .1;
        extend(&mut registry)?;
        tracing::debug!("registered the models of module {name}");
    }
    Ok(registry)
}

/// The compiled-in modules whose addon is on disk, in dependency order.
/// `base` is always among them: a server without it has no user to log
/// in as.
fn installed_code_modules(addons_path: &std::path::Path) -> anyhow::Result<Vec<&'static str>> {
    let providers = code_modules();
    if !addons_path.is_dir() {
        return Ok(vec!["base"]);
    }
    let manifests = rusdoo_modules::loader::discover_addons(&[addons_path])?;
    let order = rusdoo_modules::graph::dependency_order(&manifests)?;
    let mut wanted: Vec<&'static str> = order
        .iter()
        .filter_map(|name| {
            providers
                .iter()
                .find(|(module, _)| module == name)
                .map(|(module, _)| *module)
        })
        .collect();
    if !wanted.contains(&"base") {
        wanted.insert(0, "base");
    }
    Ok(wanted)
}

/// The model methods of the installed code modules — the business
/// actions a client calls by name (`action_confirm`, …).
fn code_methods(
    addons_path: &std::path::Path,
) -> anyhow::Result<rusdoo_orm::methods::MethodRegistry> {
    let mut methods = rusdoo_orm::methods::MethodRegistry::new();
    let installed = installed_code_modules(addons_path)?;
    if installed.contains(&"account") {
        rusdoo_account::extend_methods(&mut methods)?;
    }
    if installed.contains(&"sale") {
        rusdoo_sale::extend_methods(&mut methods)?;
    }
    if installed.contains(&"mail") {
        // which records carry a discussion: the module that owns a model
        // is the one that says so, like `_inherit = ['mail.thread']`
        let mut threads = vec!["res.partner"];
        if installed.contains(&"sale") {
            threads.push("sale.order");
        }
        if installed.contains(&"account") {
            threads.push("account.move");
        }
        rusdoo_mail::extend_methods(&mut methods, &threads)?;
    }
    Ok(methods)
}

/// First boot: create the admin user (login admin / password admin),
/// a member of `groups`.
async fn seed_admin(
    registry: &Registry,
    pool: &sqlx::PgPool,
    groups: &[i64],
) -> anyhow::Result<()> {
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
                    ("name", serde_json::json!("Administrador")),
                    ("active", serde_json::json!(true)),
                    // command 6: replace the membership with these groups
                    ("groups_id", serde_json::json!([[6, 0, groups]])),
                ],
            )
            .await?;
        tracing::warn!("usuário admin criado (login: admin / senha: admin) — troque a senha");
    }
    Ok(())
}
