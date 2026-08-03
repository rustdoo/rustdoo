//! A wizard (`TransientModel`): the dialog a method opens, the button
//! inside it, and what closing it leaves behind.

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
    rusdoo_mail::extend(&mut registry).unwrap();
    rusdoo_product::extend(&mut registry).unwrap();
    rusdoo_account::extend(&mut registry).unwrap();
    rusdoo_stock::extend(&mut registry).unwrap();
    rusdoo_sale::extend(&mut registry).unwrap();
    registry
}

async fn fixture(url: &str, schema: &'static str) -> (OrmService, i64) {
    let pool = pool(url, schema);
    let registry = registry();
    for table in [
        "sale_order_cancel",
        "sale_order_line",
        "sale_order",
        "stock_move",
        "stock_picking",
        "stock_location",
        "account_move_line",
        "account_move",
        "mail_message",
        "product_product",
        "res_partner",
        "res_company",
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
        "mail.message",
        "product.product",
        "account.move",
        "account.move.line",
        "stock.location",
        "stock.picking",
        "stock.move",
        "sale.order",
        "sale.order.line",
        "sale.order.cancel",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("Pedido")),
                ("code", json!("sale.order")),
                ("prefix", json!("SO")),
                ("padding", json!(5)),
            ],
        )
        .await
        .unwrap();
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let mut methods = MethodRegistry::new();
    rusdoo_sale::extend_methods(&mut methods).unwrap();
    (
        OrmService::insecure(Arc::new(registry), pool).with_methods(methods),
        partner,
    )
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

async fn a_confirmed_order(service: &OrmService, partner: i64) -> i64 {
    let order = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "create",
               "args": [{"partner_id": partner, "order_line": [
                   [0, 0, {"name": "Mesa", "product_uom_qty": 1, "price_unit": 100}]]}],
               "kwargs": {}}),
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

async fn state_of(service: &OrmService, order: i64) -> String {
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "read", "args": [[order]],
               "kwargs": {"fields": ["state"]}}),
    )
    .await["result"][0]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn the_dialog_cancels_the_order_and_says_why_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, partner) = fixture(&url, "rusdoo_wizard_test").await;
    let order = a_confirmed_order(&service, partner).await;

    // the button opens a dialog: an action with target "new", pointing
    // at a wizard record that already knows its order
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_cancel_wizard",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let action = &answer["result"];
    assert_eq!(action["target"], "new", "{answer}");
    assert_eq!(action["res_model"], "sale.order.cancel");
    let wizard = action["res_id"].as_i64().expect("a wizard record");

    // pressing the button with no reason refuses, and the order stands
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order.cancel", "method": "action_confirm_cancel",
               "args": [[wizard]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("por que"));
    assert_eq!(state_of(&service, order).await, "sale");

    // with a reason: the order is cancelled, the dialog says it is done,
    // and the reason is in the order's thread
    call(
        router(service.clone()),
        json!({"model": "sale.order.cancel", "method": "write",
               "args": [[wizard], {"reason": "Cliente desistiu"}], "kwargs": {}}),
    )
    .await;
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order.cancel", "method": "action_confirm_cancel",
               "args": [[wizard]], "kwargs": {}}),
    )
    .await;
    assert_eq!(
        answer["result"]["type"], "ir.actions.act_window_close",
        "{answer}"
    );
    assert_eq!(state_of(&service, order).await, "cancel");

    let messages = call(
        router(service.clone()),
        json!({"model": "mail.message", "method": "search_read",
               "args": [[["res_id", "=", order], ["model", "=", "sale.order"]]],
               "kwargs": {"fields": ["body"]}}),
    )
    .await;
    let body = messages["result"][0]["body"].as_str().unwrap_or_default();
    assert!(body.contains("Cliente desistiu"), "{messages}");

    // and cancelling again is refused rather than repeated
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_cancel_wizard",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
}

#[tokio::test]
async fn a_wizard_model_is_transient_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, _partner) = fixture(&url, "rusdoo_wizard_kind_test").await;
    let registry = registry();
    assert!(
        registry
            .get("sale.order.cancel")
            .expect("registered")
            .is_transient(),
        "the dialog's rows are not data the business keeps"
    );
    assert!(
        !registry.get("sale.order").expect("registered").is_transient(),
        "an order is not a dialog"
    );
    // the flag survives registration, which folds fields and rebuilds
    let _ = service;
}
