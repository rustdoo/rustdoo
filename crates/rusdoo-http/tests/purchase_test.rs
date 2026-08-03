//! Buying: a confirmed purchase order produces a receipt and a vendor
//! bill, each numbered by its own sequence and each only once.

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
    rusdoo_stock::extend(&mut registry).unwrap();
    rusdoo_purchase::extend(&mut registry).unwrap();
    registry
}

async fn fixture(url: &str, schema: &'static str) -> OrmService {
    let pool = pool(url, schema);
    let registry = registry();
    for table in [
        "purchase_order_line",
        "purchase_order",
        "stock_move",
        "stock_picking",
        "stock_location",
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
        "stock.location",
        "stock.picking",
        "stock.move",
        "purchase.order",
        "purchase.order.line",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    // the sequences the modules ship, like their data files load
    for (code, prefix) in [
        ("purchase.order", "PO"),
        ("account.move", "FAT/"),
        ("stock.picking.out", "WH/OUT/"),
        ("stock.picking.in", "WH/IN/"),
    ] {
        registry
            .create(
                &pool,
                "ir.sequence",
                vec![
                    ("name", json!(code)),
                    ("code", json!(code)),
                    ("prefix", json!(prefix)),
                    ("padding", json!(5)),
                    ("number_next", json!(1)),
                ],
            )
            .await
            .unwrap();
    }
    let mut methods = MethodRegistry::new();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_stock::extend_methods(&mut methods).unwrap();
    rusdoo_purchase::extend_methods(&mut methods).unwrap();
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

async fn create(service: &OrmService, model: &str, values: Value) -> i64 {
    call(
        router(service.clone()),
        json!({"model": model, "method": "create", "args": [values], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .expect("created")
}

async fn a_confirmed_order(service: &OrmService) -> i64 {
    let supplier = create(service, "res.partner", json!({"name": "Fornecedor"})).await;
    let table = create(
        service,
        "product.product",
        json!({"name": "Mesa", "type": "consu"}),
    )
    .await;
    let freight = create(
        service,
        "product.product",
        json!({"name": "Frete", "type": "service"}),
    )
    .await;
    let order = create(
        service,
        "purchase.order",
        json!({"partner_id": supplier, "order_line": [
            [0, 0, {"product_id": table, "name": "Mesa", "product_qty": 10, "price_unit": 700}],
            [0, 0, {"product_id": freight, "name": "Frete", "product_qty": 1, "price_unit": 100}],
        ]}),
    )
    .await;
    call(
        router(service.clone()),
        json!({"model": "purchase.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    order
}

#[tokio::test]
async fn a_purchase_order_produces_a_receipt_and_a_bill_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_purchase_test")).await;
    let order = a_confirmed_order(&service).await;

    let rows = call(
        router(service.clone()),
        json!({"model": "purchase.order", "method": "read", "args": [[order]],
               "kwargs": {"fields": ["name", "amount_total"]}}),
    )
    .await;
    assert_eq!(rows["result"][0]["name"], "PO00001");
    assert_eq!(rows["result"][0]["amount_total"], json!(7100.0));

    // the receipt: only the storable line, numbered as an incoming
    // document rather than as a delivery
    let answer = call(
        router(service.clone()),
        json!({"model": "purchase.order", "method": "action_create_receipt",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let picking = answer["result"]["res_id"].as_i64().expect("a picking");
    let rows = call(
        router(service.clone()),
        json!({"model": "stock.picking", "method": "read", "args": [[picking]],
               "kwargs": {"fields": ["name", "picking_type", "origin", "move_ids"]}}),
    )
    .await;
    let row = &rows["result"][0];
    assert_eq!(row["picking_type"], "incoming");
    assert_eq!(row["origin"], "PO00001");
    assert_eq!(row["name"], "WH/IN/00001", "a receipt is not a delivery");
    assert_eq!(row["move_ids"].as_array().unwrap().len(), 1);

    // the bill: an incoming invoice, same model, told apart by its type
    let answer = call(
        router(service.clone()),
        json!({"model": "purchase.order", "method": "action_create_bill",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let bill = answer["result"]["res_id"].as_i64().expect("a bill");
    let rows = call(
        router(service.clone()),
        json!({"model": "account.move", "method": "read", "args": [[bill]],
               "kwargs": {"fields": ["move_type", "invoice_origin", "amount_total"]}}),
    )
    .await;
    let row = &rows["result"][0];
    assert_eq!(row["move_type"], "in_invoice");
    assert_eq!(row["invoice_origin"], "PO00001");
    // the freight is billed even though it is not received
    assert_eq!(row["amount_total"], json!(7100.0));

    // and neither happens twice
    for method in ["action_create_receipt", "action_create_bill"] {
        let answer = call(
            router(service.clone()),
            json!({"model": "purchase.order", "method": method,
                   "args": [[order]], "kwargs": {}}),
        )
        .await;
        assert!(answer.get("result").is_none(), "{method}: {answer}");
    }
}

#[tokio::test]
async fn an_unconfirmed_purchase_produces_nothing_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_purchase_draft_test")).await;
    let supplier = create(&service, "res.partner", json!({"name": "Fornecedor"})).await;
    let order = create(
        &service,
        "purchase.order",
        json!({"partner_id": supplier, "order_line": [
            [0, 0, {"name": "Mesa", "product_qty": 1, "price_unit": 700}],
        ]}),
    )
    .await;
    for method in ["action_create_receipt", "action_create_bill"] {
        let answer = call(
            router(service.clone()),
            json!({"model": "purchase.order", "method": method,
                   "args": [[order]], "kwargs": {}}),
        )
        .await;
        assert!(answer.get("result").is_none(), "{method}: {answer}");
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not confirmed"));
    }
}
