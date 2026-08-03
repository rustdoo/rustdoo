//! A wizard (`TransientModel`): the dialog a method opens, the button
//! inside it, and what closing it leaves behind.
//!
//! Written on `TransactionCase`, the port of `odoo.tests.common`: the
//! case brings its own schema with the modules installed, and drops it
//! at the end.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The modules a sales order needs to exist and to be talked about.
const MODULES: [&str; 6] = ["base", "mail", "product", "account", "stock", "sale"];

fn service(case: &TransactionCase) -> OrmService {
    OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods())
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

async fn a_confirmed_order(service: &OrmService) -> i64 {
    let partner = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Ana"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
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
    let Some(case) = TransactionCase::open("wizard", &MODULES).await else {
        return;
    };
    let service = service(&case);
    let order = a_confirmed_order(&service).await;

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

    case.close().await;
}

#[tokio::test]
async fn a_wizard_model_is_transient_live() {
    let Some(case) = TransactionCase::open("wizard_kind", &MODULES).await else {
        return;
    };
    assert!(
        case.models()
            .get("sale.order.cancel")
            .expect("registered")
            .is_transient(),
        "as linhas do diálogo não são dados que o negócio guarda"
    );
    assert!(
        !case
            .models()
            .get("sale.order")
            .expect("registered")
            .is_transient(),
        "um pedido não é um diálogo"
    );
    case.close().await;
}
