//! The Settings screen: a visit that reads what is in force, a Save that
//! changes it, and nothing kept in between.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 1] = ["base"];

/// The screen opening: Odoo creates the transient record, and the
/// dynamic defaults are what fill it in.
async fn open_settings(case: &TransactionCase) -> i64 {
    case.models()
        .create(&case.pool(), "res.config.settings", vec![])
        .await
        .expect("the settings page opens")
}

async fn read(case: &TransactionCase, id: i64, fields: &[&str]) -> Value {
    Value::Object(
        case.models()
            .read(&case.pool(), "res.config.settings", &[id], fields)
            .await
            .expect("the page reads")
            .into_iter()
            .next()
            .unwrap(),
    )
}

async fn save(case: &TransactionCase, id: i64) -> Result<Value, String> {
    let methods = case.methods();
    let entry = methods
        .get("res.config.settings", "execute")
        .expect("execute is registered");
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, "res.config.settings", vec![id]);
    entry
        .call(ctx, &[], &Map::new())
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn the_page_opens_on_what_is_in_force_and_saving_changes_it_live() {
    let Some(case) = TransactionCase::open("res_config", &MODULES).await else {
        return;
    };
    // the catalogue the module switches read
    for (name, state) in [("crm", "uninstalled"), ("stock", "installed")] {
        case.models()
            .create(
                &case.pool(),
                "ir.module.module",
                vec![("name", json!(name)), ("state", json!(state))],
            )
            .await
            .expect("the catalogue row saves");
    }

    // opening: Odoo's own defaults, because nobody has set a parameter
    let page = open_settings(&case).await;
    let row = read(
        &case,
        page,
        &["max_file_upload_size", "active_ids_limit", "module_crm", "module_stock"],
    )
    .await;
    assert_eq!(row["max_file_upload_size"], json!(128 * 1024 * 1024), "{row}");
    assert_eq!(row["active_ids_limit"], json!(20_000), "{row}");
    assert_eq!(row["module_crm"], json!(false), "{row}");
    assert_eq!(
        row["module_stock"],
        json!(true),
        "um módulo instalado abre marcado: {row}"
    );

    // the visit is edited, as a form does, and saved
    case.models()
        .write(
            &case.pool(),
            "res.config.settings",
            &[page],
            vec![
                ("max_file_upload_size", json!(64 * 1024 * 1024)),
                ("module_crm", json!(true)),
            ],
        )
        .await
        .expect("the edit saves");
    let answer = save(&case, page).await.expect("saving works");
    assert_eq!(
        answer["modules_to_install"],
        json!(["crm"]),
        "a resposta diz o que o próximo boot vai fazer: {answer}"
    );

    // what survives: the parameter is in force...
    let stored = case
        .models()
        .get_param(&case.pool(), "web.max_file_upload_size")
        .await
        .expect("the parameter reads");
    assert_eq!(stored.as_deref(), Some("67108864"));

    // ...and the module is marked, which is the same decision the Apps
    // screen's Install button records
    let rows = case
        .models()
        .read(
            &case.pool(),
            "ir.module.module",
            &case
                .models()
                .search(
                    &case.pool(),
                    "ir.module.module",
                    &rusdoo_orm::domain::parse_domain(&json!([["name", "=", "crm"]])).unwrap(),
                    &rusdoo_orm::crud::SearchOptions::default(),
                )
                .await
                .unwrap(),
            &["state"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["state"], json!("to install"), "{:?}", rows[0]);

    // A second visit: `stock` is already running and is never re-asked,
    // while `crm` is still *pending* — it only becomes installed at the
    // next boot — so saying so again is the truth, not noise. That
    // distinction is the whole difference between this port and Odoo,
    // where installing happens inside the request.
    let page = open_settings(&case).await;
    case.models()
        .write(
            &case.pool(),
            "res.config.settings",
            &[page],
            vec![("module_stock", json!(true))],
        )
        .await
        .expect("the edit saves");
    let answer = save(&case, page).await.expect("saving again works");
    assert_eq!(
        answer["modules_to_install"],
        json!(["crm"]),
        "o que está no ar não é pedido de novo; o pendente continua pendente: {answer}"
    );

    // and the second visit opens on the value the first one wrote
    let row = read(&case, page, &["max_file_upload_size", "module_crm"]).await;
    assert_eq!(row["max_file_upload_size"], json!(64 * 1024 * 1024), "{row}");
    assert_eq!(
        row["module_crm"],
        json!(true),
        "um módulo já pedido abre marcado, senão a visita seguinte o desmarcaria: {row}"
    );

    case.close().await;
}

#[tokio::test]
async fn a_switch_for_a_module_nobody_has_says_so_live() {
    let Some(case) = TransactionCase::open("res_config_missing", &MODULES).await else {
        return;
    };
    let page = open_settings(&case).await;
    case.models()
        .write(
            &case.pool(),
            "res.config.settings",
            &[page],
            vec![("module_fleet", json!(true))],
        )
        .await
        .expect("the edit saves");

    // the catalogue is written from the filesystem, so an empty one means
    // the addon is not on this server at all — and the message says that
    // rather than recording a decision nothing can honour
    let refused = save(&case, page).await;
    let message = refused.expect_err("ligou um módulo que não existe");
    assert!(message.contains("fleet"), "{message}");
    assert!(message.contains("catalogue") || message.contains("addon"), "{message}");

    case.close().await;
}
