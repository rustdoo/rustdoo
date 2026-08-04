//! The whole thing, once: install the real `addons/` tree and check what
//! a booted server would have — the models of the code modules, the data
//! of their addons, the ACL rows, the client bundle, and an order that
//! adds up.

use rusdoo_modules::installer::{install_modules, XmlIds};
use rusdoo_orm::access::{AccessControl, Operation};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use std::path::Path;

/// The addons tree of the repository, from this crate's directory.
const ADDONS: &str = "../../addons";

/// Everything runs in a schema of its own: the install creates the real
/// system tables (`ir_model_data`, `ir_model_access`, …) under their
/// fixed names.
async fn schema_pool(url: &str) -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                let schema = rusdoo_testing::schema_for("rusdoo_full_boot");
                sqlx::Executor::execute(
                    conn,
                    &format!(
                        "CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}"
                    ) as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap();
    // a previous run must not decide this one's answers
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", rusdoo_testing::schema_for("rusdoo_full_boot")) as &str)
        .execute(&pool)
        .await
        .unwrap();
    // the pool's own hook may have recreated it on the way here
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", rusdoo_testing::schema_for("rusdoo_full_boot")) as &str)
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// The models of every code module of the repository — what the server
/// registers before letting the addons' data speak about them.
fn registry() -> Registry {
    let mut registry = rusdoo_base::registry().expect("base models");
    rusdoo_mail::extend(&mut registry).expect("mail models");
    rusdoo_rating::extend(&mut registry).expect("rating models");
    rusdoo_product::extend(&mut registry).expect("product models");
    rusdoo_account::extend(&mut registry).expect("account models");
    rusdoo_analytic::extend(&mut registry).expect("analytic models");
    rusdoo_stock::extend(&mut registry).expect("stock models");
    rusdoo_purchase::extend(&mut registry).expect("purchase models");
    // the pilot batch's modules
    rusdoo_base_vat::extend(&mut registry).expect("base_vat models");
    rusdoo_calendar::extend(&mut registry).expect("calendar models");
    rusdoo_phone_validation::extend(&mut registry).expect("phone_validation models");
    rusdoo_fleet::extend(&mut registry).expect("fleet models");
    rusdoo_lunch::extend(&mut registry).expect("lunch models");
    rusdoo_resource::extend(&mut registry).expect("resource models");
    rusdoo_hr::extend(&mut registry).expect("hr models");
    rusdoo_hr_attendance::extend(&mut registry).expect("hr_attendance models");
    rusdoo_maintenance::extend(&mut registry).expect("maintenance models");
    rusdoo_project::extend(&mut registry).expect("project models");
    rusdoo_stock_account::extend(&mut registry).expect("stock_account models");
    rusdoo_stock_picking_batch::extend(&mut registry).expect("stock_picking_batch models");
    rusdoo_purchase_requisition::extend(&mut registry).expect("purchase_requisition models");
    rusdoo_account_check_printing::extend(&mut registry).expect("account_check_printing models");
    rusdoo_uom::extend(&mut registry).expect("uom models");
    rusdoo_barcodes::extend(&mut registry).expect("barcodes models");
    rusdoo_utm::extend(&mut registry).expect("utm models");
    rusdoo_sales_team::extend(&mut registry).expect("sales_team models");
    rusdoo_crm::extend(&mut registry).expect("crm models");
    rusdoo_account_debit_note::extend(&mut registry).expect("account_debit_note models");
    rusdoo_data_recycle::extend(&mut registry).expect("data_recycle models");
    rusdoo_onboarding::extend(&mut registry).expect("onboarding models");
    rusdoo_sale::extend(&mut registry).expect("sale models");
    // depois de `sale` e `purchase`: ele estende as linhas dos dois
    rusdoo_sale_purchase::extend(&mut registry).expect("sale_purchase models");
    rusdoo_sale_crm::extend(&mut registry).expect("sale_crm models");
    registry
}

#[tokio::test]
async fn the_addons_tree_installs_and_adds_up_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url).await;
    let mut registry = registry();
    let mut xml_ids = XmlIds::new();
    let report = install_modules(&pool, &mut registry, &[Path::new(ADDONS)], &mut xml_ids)
        .await
        .expect("the addons tree installs");

    // every addon of the repository loaded, in dependency order
    let installed: Vec<&str> = report
        .modules
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    for module in [
        "base", "base_vat", "web", "mail", "product", "account", "stock", "purchase",
        "sale", "sale_purchase", "calendar", "resource", "stock_account",
        "stock_picking_batch", "purchase_requisition", "account_check_printing",
        "phone_validation", "fleet", "lunch", "hr", "hr_attendance", "maintenance", "project", "crm", "sale_crm",
        "contacts", "uom",
        "barcodes", "utm", "sales_team",
        "account_debit_note", "data_recycle", "onboarding",
    ] {
        assert!(installed.contains(&module), "installed: {installed:?}");
    }
    for earlier in ["base", "product", "account", "stock"] {
        assert!(
            installed.iter().position(|m| m == &earlier)
                < installed.iter().position(|m| *m == "sale"),
            "{earlier} loads before what depends on it: {installed:?}"
        );
    }

    // the client bundle is there, in load order, with the boot last
    let backend: Vec<&str> = report
        .bundles
        .files_with_extension("web.assets_backend", &["js"])
        .map(|file| file.path.as_str())
        .collect();
    assert!(
        backend.first().is_some_and(|first| first.ends_with("utils.js")),
        "bundle: {backend:?}"
    );
    assert!(
        backend
            .last()
            .is_some_and(|last| last.ends_with("webclient.js")),
        "the boot runs when the rest exists: {backend:?}"
    );

    // the ACL of both addons survived as rows, which is what the next
    // boot reads instead of re-installing
    let access = AccessControl::load(&pool).await.expect("acl loads");
    let sale_user = xml_ids
        .get("sale.group_sale_user")
        .expect("the sale groups published")
        .1;
    let base_user = xml_ids.get("base.group_user").expect("base groups").1;
    assert!(access
        .check("sale.order", Operation::Create, &[sale_user], false)
        .is_ok());
    assert!(
        access
            .check("sale.order", Operation::Unlink, &[sale_user], false)
            .is_err(),
        "a salesperson does not delete orders"
    );
    assert!(access
        .check("res.partner", Operation::Read, &[base_user], false)
        .is_ok());
    assert!(
        access
            .check("sale.order", Operation::Read, &[base_user], false)
            .is_err(),
        "the base group says nothing about sales"
    );

    // the company every install has, and an admin who belongs to it:
    // without them the web client cannot draw the switcher at the top of
    // the screen, which is where this gap first showed
    let company = xml_ids
        .get("base.main_company")
        .expect("base ships the main company")
        .1;
    let rows = registry
        .read(&pool, "res.company", &[company], &["name"])
        .await
        .expect("the company reads");
    assert!(!rows[0]["name"].as_str().unwrap_or_default().is_empty());

    // the demo products of the sale addon are real records
    let products = registry
        .search(
            &pool,
            "product.product",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("products readable");
    assert!(products.len() >= 3, "demo products: {products:?}");

    // every one of them is measured in something: the ones that said
    // nothing default to the reference unit `uom` installs, and the
    // service that named hours got hours. Both answers come from the
    // template, through the delegation.
    let rows = registry
        .read(&pool, "product.product", &products, &["name", "uom_id"])
        .await
        .expect("products read");
    for row in &rows {
        assert!(
            row["uom_id"].is_array(),
            "a product with no unit: {}",
            row["name"]
        );
    }
    let service = rows
        .iter()
        .find(|row| row["name"].as_str() == Some("Montagem e instalação"))
        .expect("the demo service");
    assert_eq!(service["uom_id"][1], json!("Horas"));

    // and an order adds up: two lines, subtotals, total — the whole
    // point of the module
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Cliente Teste"))])
        .await
        .unwrap();
    let order = registry
        .create(
            &pool,
            "sale.order",
            vec![
                // no name: the sequence of the sale module numbers it
                ("partner_id", json!(partner)),
                (
                    "order_line",
                    json!([
                        [0, 0, {"product_uom_qty": 2, "price_unit": 1250}],
                        [0, 0, {"product_uom_qty": 4, "price_unit": 890.5}],
                    ]),
                ),
            ],
        )
        .await
        .expect("the order saves");
    let rows = registry
        .read(&pool, "sale.order", &[order], &["amount_total", "name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(6062.0));
    assert_eq!(
        rows[0]["name"], "SO00001",
        "the order carries the number its module's sequence gave it"
    );

    // removing a line moves the total with it
    let lines = registry
        .read(&pool, "sale.order", &[order], &["order_line"])
        .await
        .unwrap();
    let first_line = lines[0]["order_line"][0].as_i64().unwrap();
    registry
        .write(
            &pool,
            "sale.order",
            &[order],
            vec![("order_line", json!([[2, first_line, 0]]))],
        )
        .await
        .unwrap();
    let rows = registry
        .read(&pool, "sale.order", &[order], &["amount_total"])
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(3562.0));
}
