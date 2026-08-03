//! Campos `Binary`: base64 pelo JSON-RPC, bytes pela rota de imagem.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Um PNG de 1×1 transparente — bytes de verdade, com a assinatura que a
/// rota usa para decidir o content-type.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

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
async fn an_image_survives_the_round_trip_live() {
    let Some(case) = TransactionCase::open("binary", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 100.0, "image_1920": b64(PNG)}]),
    )
    .await["result"]
        .as_i64()
        .expect("o produto é criado com imagem");

    let rows = call(
        &service,
        "product.product",
        "read",
        json!([[id], ["name", "image_1920"]]),
    )
    .await;
    let encoded = rows["result"][0]["image_1920"]
        .as_str()
        .unwrap_or_else(|| panic!("sem imagem: {rows}"));
    let back = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    assert_eq!(back, PNG, "os bytes voltam idênticos");

    // and a product with no image answers null, not an empty string:
    // the screen draws a placeholder from that
    let other = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Cadeira", "list_price": 50.0}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let rows = call(
        &service,
        "product.product",
        "read",
        json!([[other], ["image_1920"]]),
    )
    .await;
    assert_eq!(rows["result"][0]["image_1920"], json!(null));

    // clearing is writing `false`, as in Odoo
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"image_1920": false}]),
    )
    .await;
    let rows = call(
        &service,
        "product.product",
        "read",
        json!([[id], ["image_1920"]]),
    )
    .await;
    assert_eq!(rows["result"][0]["image_1920"], json!(null), "a imagem saiu");

    case.close().await;
}

#[tokio::test]
async fn malformed_base64_is_refused_with_a_sentence_live() {
    let Some(case) = TransactionCase::open("binary_bad", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    let answer = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 1.0, "image_1920": "isto não é base64!!"}]),
    )
    .await;
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("invalid base64"),
        "a recusa nomeia o problema, não uma função de SQL: {answer}"
    );

    // a number is not an image either
    let answer = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 1.0, "image_1920": 42}]),
    )
    .await;
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("takes base64"),
        "{answer}"
    );

    case.close().await;
}

#[tokio::test]
async fn the_image_route_serves_bytes_with_a_type_it_sniffed_live() {
    let Some(case) = TransactionCase::open("binary_route", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 100.0, "image_1920": b64(PNG)}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    let response = router(service.clone())
        .oneshot(
            Request::get(format!("/web/image/product.product/{id}/image_1920"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "image/png",
        "o tipo vem dos bytes"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], PNG, "os bytes, não o base64 deles");

    // a field that is not binary is not served here
    let response = router(service.clone())
        .oneshot(
            Request::get(format!("/web/image/product.product/{id}/name"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // and a record with no image is not an error
    let empty = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Cadeira", "list_price": 1.0}]),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    let response = router(service)
        .oneshot(
            Request::get(format!("/web/image/product.product/{empty}/image_1920"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    case.close().await;
}
