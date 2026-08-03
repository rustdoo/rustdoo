//! Billing a sales order: the module that owns the order creates the
//! document, and answers with where the client should go next.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn pool(url: &str, schema: &'static str) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap()
}

fn registry() -> Registry {
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut registry).unwrap();
    rusdoo_account::extend(&mut registry).unwrap();
    rusdoo_sale::extend(&mut registry).unwrap();
    registry
}

async fn fixture(url: &str, schema: &'static str) -> OrmService {
    let pool = pool(url, schema);
    let registry = registry();
    for table in [
        "sale_order_line",
        "sale_order",
        "account_move_line",
        "account_move",
        "product_product",
        "res_partner",
        "res_company",
        "res_users",
        "res_groups",
        "res_country",
        "ir_sequence",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        // every registered model gets its table, like the real boot —
        // a document numbered by a sequence needs the sequence table
        "ir.sequence",
        "res.country",
        "res.company",
        "res.partner",
        // the company default reads the creating user
        "res.groups",
        "res.users",
        "product.product",
        "account.move",
        "account.move.line",
        "sale.order",
        "sale.order.line",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    // the sequences these modules ship, like their data files load
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("sale.order")),
                ("code", json!("sale.order")),
                ("prefix", json!("SO")),
                ("padding", json!(5)),
                ("number_next", json!(1)),
            ],
        )
        .await
        .unwrap();
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("account.move")),
                ("code", json!("account.move")),
                ("prefix", json!("FAT/")),
                ("padding", json!(5)),
                ("number_next", json!(1)),
            ],
        )
        .await
        .unwrap();
    let mut methods = MethodRegistry::new();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_sale::extend_methods(&mut methods).unwrap();
    OrmService::insecure(Arc::new(registry), pool).with_methods(methods)
}

async fn call(app: axum::Router, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": params});
    let response = app
        .oneshot(
            Request::post("/web/dataset/call_kw")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// A confirmed order for two products, ready to be billed.
async fn a_confirmed_order(service: &OrmService, name: &str) -> i64 {
    let partner = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Cliente"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let product = call(
        router(service.clone()),
        json!({"model": "product.product", "method": "create",
               "args": [{"name": "Mesa", "list_price": 1250}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let order = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "create",
               "args": [{"name": name, "partner_id": partner, "order_line": [
                   [0, 0, {"product_id": product, "name": "Mesa", "product_uom_qty": 2, "price_unit": 1250}],
                   [0, 0, {"name": "Montagem", "product_uom_qty": 1, "price_unit": 300}],
               ]}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    order
}

#[tokio::test]
async fn a_confirmed_order_becomes_an_invoice_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_invoicing_test")).await;
    let order = a_confirmed_order(&service, "SO-INV").await;

    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_create_invoice",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let action = &answer["result"];
    // the answer is where to go next, in the shape Odoo uses
    assert_eq!(action["type"], "ir.actions.act_window", "{answer}");
    assert_eq!(action["res_model"], "account.move");
    let invoice = action["res_id"].as_i64().expect("an invoice id");

    let rows = call(
        router(service.clone()),
        json!({"model": "account.move", "method": "read", "args": [[invoice]],
               "kwargs": {"fields": ["state", "move_type", "invoice_origin", "amount_total"]}}),
    )
    .await;
    let invoice_row = &rows["result"][0];
    assert_eq!(invoice_row["state"], "draft", "an invoice is born a draft");
    assert_eq!(invoice_row["move_type"], "out_invoice");
    assert_eq!(invoice_row["invoice_origin"], "SO-INV");
    // the lines were copied, so the total is the order's total
    assert_eq!(invoice_row["amount_total"], json!(2800.0));

    // billing the same order twice would issue a duplicate document
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_create_invoice",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("already invoiced"));

    // and the invoice posts, once
    let answer = call(
        router(service.clone()),
        json!({"model": "account.move", "method": "action_post",
               "args": [[invoice]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"], json!(true), "{answer}");
    let rows = call(
        router(service),
        json!({"model": "account.move", "method": "read", "args": [[invoice]],
               "kwargs": {"fields": ["state"]}}),
    )
    .await;
    assert_eq!(rows["result"][0]["state"], "posted");
}

#[tokio::test]
async fn an_unconfirmed_order_is_not_billed_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_invoicing_draft_test")).await;
    let partner = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Cliente"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let order = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "create",
               "args": [{"name": "SO-DRAFT", "partner_id": partner,
                         "order_line": [[0, 0, {"product_uom_qty": 1, "price_unit": 10}]]}],
               "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_create_invoice",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("is not confirmed"));

    // an order with no lines has nothing to bill either
    let empty = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "create",
               "args": [{"name": "SO-EMPTY", "partner_id": partner}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[empty]], "kwargs": {}}),
    )
    .await;
    let answer = call(
        router(service),
        json!({"model": "sale.order", "method": "action_create_invoice",
               "args": [[empty]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
}

#[tokio::test]
async fn an_invoice_without_lines_is_not_posted_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_invoicing_empty_test")).await;
    let partner = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Cliente"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let invoice = call(
        router(service.clone()),
        json!({"model": "account.move", "method": "create",
               "args": [{"name": "FAT-VAZIA", "partner_id": partner}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let answer = call(
        router(service),
        json!({"model": "account.move", "method": "action_post",
               "args": [[invoice]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("has no lines"));
}

#[tokio::test]
async fn a_line_that_sells_nothing_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_invoicing_constraint_test")).await;
    let partner = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Cliente"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    // the rule travels with the record, so it also refuses a line
    // created through its order's x2many commands
    for line in [
        json!({"product_uom_qty": 0, "price_unit": 100}),
        json!({"product_uom_qty": 1, "price_unit": -5}),
    ] {
        let answer = call(
            router(service.clone()),
            json!({"model": "sale.order", "method": "create",
                   "args": [{"partner_id": partner, "order_line": [[0, 0, line]]}],
                   "kwargs": {}}),
        )
        .await;
        assert!(answer.get("result").is_none(), "{answer}");
    }
    // and nothing was left behind by the refused creates
    let orders = call(
        router(service),
        json!({"model": "sale.order", "method": "search_read", "args": [[]],
               "kwargs": {"fields": ["name"]}}),
    )
    .await;
    assert_eq!(orders["result"], json!([]), "{orders}");
}
