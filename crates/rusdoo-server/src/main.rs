//! rusdoo — the server binary, port of `odoo-bin`.

use rusdoo_http::dispatch::OrmService;
use rusdoo_modules::manifest::Manifest;
use rusdoo_orm::registry::Registry;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_ADDR: &str = "0.0.0.0:8069";

/// How often the scheduler looks for work. Odoo's own worker wakes on a
/// similar beat; the granularity of a job is the tick, not the second.
const CRON_TICK: std::time::Duration = std::time::Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let db_url = std::env::var("RUSDOO_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| anyhow::anyhow!("set RUSDOO_DATABASE_URL or DATABASE_URL"))?;

    let pool = rusdoo_orm::db::connect(&db_url).await?;
    let mut assets = rusdoo_http::assets::AssetHub::empty();

    // a list, comma-separated, like Odoo's own `--addons-path`. One
    // directory is never enough for a real deployment: Odoo itself keeps
    // `base` in a different root from the rest, and any install that adds
    // OCA or a company's own modules has a third.
    let addons = std::env::var("RUSDOO_ADDONS_PATH").unwrap_or_else(|_| "addons".into());
    let addons_paths: Vec<std::path::PathBuf> = addons
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|path| {
            // said out loud rather than skipped in silence: a typo in the
            // list is otherwise a module that mysteriously is not there
            let there = path.is_dir();
            if !there {
                tracing::warn!("addons path {} is not a directory, ignored", path.display());
            }
            there
        })
        .collect();
    let roots: Vec<&std::path::Path> = addons_paths.iter().map(PathBuf::as_path).collect();
    let has_addons = !roots.is_empty();

    // The tree is read once, here, and every step below works off this
    // list — a reference clone has 652 manifests, and parsing them four
    // times is four times the boot.
    let manifests = if has_addons {
        install_set(rusdoo_modules::loader::discover_addons(&roots)?)?
    } else {
        Vec::new()
    };

    // A module is code plus data: the models of the addons present on
    // disk are registered here, in dependency order, before their data
    // files are allowed to speak about them.
    let mut registry = code_registry(&manifests)?;
    // the methods those same modules attach to their models
    let mut methods = code_methods(&manifests)?;
    // and the addons whose models are Python rather than a crate. Every
    // boot, not only `--init`: a model is code, and code is not installed
    // into the database — a server restarted without `--init` would
    // otherwise serve half its addons.
    if has_addons {
        let declared = rusdoo_modules::installer::register_python_models_of(
            &manifests,
            &mut registry,
            &mut methods,
        )?;
        if declared > 0 {
            tracing::info!("{declared} model(s) declared by an addon's Python");
        }
    }

    // What the addons ship to the browser is read off the filesystem, so
    // it is resolved on every boot — a server restarted without --init
    // still serves its client.
    if has_addons {
        let (bundles, asset_roots) = rusdoo_modules::assets::resolve_manifests(&manifests)?;
        tracing::info!("{} client bundle(s) resolved", bundles.names().count());
        assets = rusdoo_http::assets::AssetHub::new(bundles, asset_roots);
    }

    let mut translations = rusdoo_orm::translations::Translations::new();
    if std::env::args().any(|arg| arg == "--init") {
        use rusdoo_modules::installer::{install_manifests, XmlIds};
        let mut xml_ids = XmlIds::load(&pool).await?;
        if has_addons {
            let report =
                install_manifests(&pool, &mut registry, &manifests, &mut xml_ids).await?;
            translations = report.translations.clone();
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
            registry.init_tables(&pool).await?;
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
            "no ir.model.access rule loaded: only the superuser (uid 1) reaches \
             models; ordinary users stay blocked until the ACLs are loaded \
             (run with --init over addons that ship ir.model.access.csv)"
        );
    }
    let mut service = OrmService::new(Arc::new(registry), pool)
        .with_access(access)
        .with_rules(rules)
        .with_assets(assets)
        .with_methods(methods)
        .with_translations(translations);
    // once, at boot: a `PATH` that changes under a running server must
    // not make printing start or stop working halfway through a day
    match rusdoo_http::pdf::ExternalPdf::discover() {
        Some(converter) => {
            use rusdoo_http::pdf::PdfRenderer;
            tracing::info!("printing documents with {}", converter.name());
            service = service.with_pdf(Arc::new(converter));
        }
        // not a failure: reports still serve as HTML, which is a page
        // the browser prints. Said out loud, because a print button that
        // answers 503 should not be a surprise.
        None => tracing::warn!(
            "no PDF converter found: /report/pdf/ will refuse and /report/html/ \
             will serve. Install weasyprint, or name one in RUSDOO_PDF_BIN"
        ),
    }
    if std::env::var("RUSDOO_INSECURE_COOKIES").is_ok() {
        service = service.allow_insecure_cookies();
    }
    // the address is configurable: a machine already running an Odoo
    // has the
    // 8069 ocupada, e um container publica em outra interface
    let addr = std::env::var("RUSDOO_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    // the scheduler: it wakes on its own and runs what ir.cron says is
    // due. Nothing else in the process waits on it.
    rusdoo_http::cron::spawn(service.clone(), CRON_TICK);
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
        // depois do `base`: estende `res.partner`, que ele declara
        ("base_vat", rusdoo_base_vat::extend as ModelProvider),
        ("calendar", rusdoo_calendar::extend as ModelProvider),
        ("phone_validation", rusdoo_phone_validation::extend as ModelProvider),
        ("fleet", rusdoo_fleet::extend as ModelProvider),
        ("lunch", rusdoo_lunch::extend as ModelProvider),
        ("resource", rusdoo_resource::extend as ModelProvider),
        ("mail", rusdoo_mail::extend as ModelProvider),
        ("rating", rusdoo_rating::extend as ModelProvider),
        ("product", rusdoo_product::extend as ModelProvider),
        ("account", rusdoo_account::extend as ModelProvider),
        ("analytic", rusdoo_analytic::extend as ModelProvider),
        ("stock", rusdoo_stock::extend as ModelProvider),
        // depois de `stock` e `account`, que ele estende
        ("stock_account", rusdoo_stock_account::extend as ModelProvider),
        ("stock_picking_batch", rusdoo_stock_picking_batch::extend as ModelProvider),
        // depois de `purchase`, cujas ordens ele agrupa
        ("purchase_requisition", rusdoo_purchase_requisition::extend as ModelProvider),
        ("purchase", rusdoo_purchase::extend as ModelProvider),
        ("uom", rusdoo_uom::extend as ModelProvider),
        ("barcodes", rusdoo_barcodes::extend as ModelProvider),
        ("utm", rusdoo_utm::extend as ModelProvider),
        ("sales_team", rusdoo_sales_team::extend as ModelProvider),
        ("account_debit_note", rusdoo_account_debit_note::extend as ModelProvider),
        ("account_check_printing", rusdoo_account_check_printing::extend as ModelProvider),
        ("data_recycle", rusdoo_data_recycle::extend as ModelProvider),
        ("onboarding", rusdoo_onboarding::extend as ModelProvider),
        ("sale", rusdoo_sale::extend as ModelProvider),
        // depois de `sale` e `purchase`, que ele amarra um ao outro
        ("sale_purchase", rusdoo_sale_purchase::extend as ModelProvider),
    ]
}

/// Build the registry out of the code modules whose addon is installed.
/// The addons this server runs, out of everything on disk: `RUSDOO_INSTALL`
/// names them, comma-separated, like Odoo's `-i`, and their dependencies
/// come along. Unset installs every addon found, which is what a
/// single-purpose deployment and every test here expect.
///
/// Naming a set is not a convenience: pointed at the reference clone,
/// installing all 652 addons serves a client bundle that starts with an
/// addon `web` never depended on, and the browser stops at the first
/// `odoo.define`.
fn install_set(discovered: Vec<Manifest>) -> anyhow::Result<Vec<Manifest>> {
    let Ok(list) = std::env::var("RUSDOO_INSTALL") else {
        return Ok(discovered);
    };
    let wanted: Vec<String> = list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    if wanted.is_empty() {
        return Ok(discovered);
    }
    let selected = rusdoo_modules::loader::select(discovered, &wanted)?;
    tracing::info!(
        "install set: {} module(s) from RUSDOO_INSTALL and their dependencies",
        selected.len()
    );
    Ok(selected)
}

/// Without an addons directory only `base` is registered — enough for a
/// server to answer, and honest about what it has.
fn code_registry(manifests: &[Manifest]) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();
    let providers = code_modules();
    for name in installed_code_modules(manifests)? {
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
fn installed_code_modules(manifests: &[Manifest]) -> anyhow::Result<Vec<&'static str>> {
    let providers = code_modules();
    if manifests.is_empty() {
        return Ok(vec!["base"]);
    }
    let order = rusdoo_modules::graph::dependency_order(manifests)?;
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
fn code_methods(manifests: &[Manifest]) -> anyhow::Result<rusdoo_orm::methods::MethodRegistry> {
    let mut methods = rusdoo_orm::methods::MethodRegistry::new();
    // the framework's own scheduled work, before any module's
    rusdoo_base::extend_methods(&mut methods)?;
    let installed = installed_code_modules(manifests)?;
    if installed.contains(&"account") {
        rusdoo_account::extend_methods(&mut methods)?;
    }
    if installed.contains(&"analytic") {
        rusdoo_analytic::extend_methods(&mut methods)?;
    }
    if installed.contains(&"rating") {
        rusdoo_rating::extend_methods(&mut methods)?;
    }
    if installed.contains(&"stock") {
        rusdoo_stock::extend_methods(&mut methods)?;
    }
    if installed.contains(&"purchase") {
        rusdoo_purchase::extend_methods(&mut methods)?;
    }
    if installed.contains(&"account_check_printing") {
        rusdoo_account_check_printing::extend_methods(&mut methods)?;
    }
    if installed.contains(&"purchase_requisition") {
        rusdoo_purchase_requisition::extend_methods(&mut methods)?;
    }
    if installed.contains(&"stock_picking_batch") {
        rusdoo_stock_picking_batch::extend_methods(&mut methods)?;
    }
    if installed.contains(&"stock_account") {
        rusdoo_stock_account::extend_methods(&mut methods)?;
    }
    if installed.contains(&"sale_purchase") {
        rusdoo_sale_purchase::extend_methods(&mut methods)?;
    }
    if installed.contains(&"phone_validation") {
        rusdoo_phone_validation::extend_methods(&mut methods)?;
    }
    if installed.contains(&"fleet") {
        rusdoo_fleet::extend_methods(&mut methods)?;
    }
    if installed.contains(&"lunch") {
        rusdoo_lunch::extend_methods(&mut methods)?;
    }
    if installed.contains(&"calendar") {
        rusdoo_calendar::extend_methods(&mut methods)?;
    }
    if installed.contains(&"resource") {
        rusdoo_resource::extend_methods(&mut methods)?;
    }
    if installed.contains(&"uom") {
        rusdoo_uom::extend_methods(&mut methods)?;
    }
    if installed.contains(&"barcodes") {
        rusdoo_barcodes::extend_methods(&mut methods)?;
    }
    if installed.contains(&"utm") {
        rusdoo_utm::extend_methods(&mut methods)?;
    }
    if installed.contains(&"sales_team") {
        rusdoo_sales_team::extend_methods(&mut methods)?;
    }
    if installed.contains(&"account_debit_note") {
        rusdoo_account_debit_note::extend_methods(&mut methods)?;
    }
    if installed.contains(&"data_recycle") {
        rusdoo_data_recycle::extend_methods(&mut methods)?;
    }
    if installed.contains(&"onboarding") {
        rusdoo_onboarding::extend_methods(&mut methods)?;
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
        if installed.contains(&"stock") {
            threads.push("stock.picking");
        }
        if installed.contains(&"purchase") {
            threads.push("purchase.order");
        }
        if installed.contains(&"sales_team") {
            threads.push("crm.team");
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
        tracing::warn!("admin user created (login: admin / password: admin) — change the password");
    }
    Ok(())
}
