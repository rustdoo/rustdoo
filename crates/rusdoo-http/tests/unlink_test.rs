//! Apagar um registro: o que o negócio recusa, e o que o banco faz com
//! as referências.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};
use tower::ServiceExt;

const MODULES: [&str; 4] = ["base", "mail", "product", "sale"];

async fn call(service: &OrmService, model: &str, method: &str, args: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "call",
        "params": {"model": model, "method": method, "args": args, "kwargs": {}}
    });
    let response = router(service.clone())
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

#[tokio::test]
async fn a_confirmed_order_refuses_to_be_deleted_live() {
    let Some(case) = TransactionCase::open("unlink_hook", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods());

    let partner = call(&service, "res.partner", "create", json!([{"name": "Ana"}])).await["result"]
        .as_i64()
        .unwrap();
    let order = call(
        &service,
        "sale.order",
        "create",
        json!([{"partner_id": partner}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    // em rascunho o pedido é apagável
    let other = call(
        &service,
        "sale.order",
        "create",
        json!([{"partner_id": partner}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let answer = call(&service, "sale.order", "unlink", json!([[other]])).await;
    assert_eq!(answer["result"], json!(true), "um rascunho some: {answer}");

    // confirmado, não
    call(&service, "sale.order", "action_confirm", json!([[order]])).await;
    let answer = call(&service, "sale.order", "unlink", json!([[order]])).await;
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("is not in draft"),
        "o hook recusa e diz por quê: {answer}"
    );

    // e a recusa é uma recusa: o registro continua lá
    let left = call(&service, "sale.order", "search_count", json!([[]])).await["result"]
        .as_i64()
        .unwrap();
    assert_eq!(left, 1, "o pedido confirmado não foi apagado");

    case.close().await;
}

#[tokio::test]
async fn a_referenced_record_cannot_be_deleted_out_from_under_it_live() {
    let Some(case) = TransactionCase::open("unlink_fk", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods());

    let partner = call(&service, "res.partner", "create", json!([{"name": "Bia"}])).await["result"]
        .as_i64()
        .unwrap();
    call(
        &service,
        "sale.order",
        "create",
        json!([{"partner_id": partner}]),
    )
    .await;

    // `sale.order.partner_id` é obrigatório: a regra do Odoo para uma
    // referência obrigatória é recusar o apagamento do alvo
    let answer = call(&service, "res.partner", "unlink", json!([[partner]])).await;
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("records depend on this one"),
        "o banco recusa, e a frase diz de que lado está o problema: {answer}"
    );
    let left = call(&service, "res.partner", "search_count", json!([[]])).await["result"]
        .as_i64()
        .unwrap();
    assert_eq!(left, 1);

    case.close().await;
}

#[tokio::test]
async fn a_reference_to_a_record_that_does_not_exist_is_refused_live() {
    let Some(case) = TransactionCase::open("unlink_dangling", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods());

    // o que as chaves estrangeiras compram: a referência inventada não
    // entra mais, em vez de virar um formulário que abre no vazio
    let answer = call(
        &service,
        "sale.order",
        "create",
        json!([{"partner_id": 424242}]),
    )
    .await;
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("does not exist"),
        "a referência inventada é recusada: {answer}"
    );

    case.close().await;
}

#[tokio::test]
async fn the_lines_of_a_deleted_document_go_with_it_live() {
    let Some(case) = TransactionCase::open("unlink_cascade", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods());

    let partner = call(&service, "res.partner", "create", json!([{"name": "Caio"}])).await["result"]
        .as_i64()
        .unwrap();
    let product = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 100.0}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let order = call(
        &service,
        "sale.order",
        "create",
        json!([{
            "partner_id": partner,
            "order_line": [[0, 0, {"product_id": product, "product_uom_qty": 2,
                                   "price_unit": 100.0, "name": "Mesa"}]]
        }]),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    assert_eq!(
        call(&service, "sale.order.line", "search_count", json!([[]])).await["result"],
        json!(1)
    );

    // o pedido está em rascunho, então some — e as linhas não são
    // registros com vida própria: vão junto
    let answer = call(&service, "sale.order", "unlink", json!([[order]])).await;
    assert_eq!(answer["result"], json!(true), "{answer}");
    assert_eq!(
        call(&service, "sale.order.line", "search_count", json!([[]])).await["result"],
        json!(0),
        "as linhas não ficaram órfãs"
    );

    case.close().await;
}
