//! The client's context reaches the method, instead of being read and
//! thrown away — which is what made `with_context` a silence.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};
use tower::ServiceExt;

/// A method that answers nothing but the context it was given: if the
/// dispatch loses it on the way, this test is what notices.
fn echo_context<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        Ok(json!({
            "lang": ctx.context.lang(),
            "tz": ctx.context.tz(),
            "active_test": ctx.context.active_test(),
            "todo": ctx.context.to_value(),
        }))
    })
}

async fn call(service: &OrmService, model: &str, method: &str, args: Value, kwargs: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "call",
        "params": {"model": model, "method": method, "args": args, "kwargs": kwargs}
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
async fn the_clients_context_reaches_the_method_live() {
    let Some(case) = TransactionCase::open("context", &["base"]).await else {
        return;
    };
    let mut methods = MethodRegistry::new();
    methods
        .register("res.partner", "echo_context", Operation::Read, echo_context)
        .unwrap();
    let service = OrmService::insecure(case.registry(), case.pool()).with_methods(methods);

    // with no context: the defaults, and no error
    let answer = call(
        &service,
        "res.partner",
        "echo_context",
        json!([[]]),
        json!({}),
    )
    .await;
    assert_eq!(answer["result"]["lang"], json!("en_US"), "{answer}");
    assert_eq!(answer["result"]["active_test"], json!(true));

    // with a context: it arrives whole, including what the framework
    // does not know
    let answer = call(
        &service,
        "res.partner",
        "echo_context",
        json!([[]]),
        json!({"context": {
            "lang": "pt_BR",
            "tz": "America/Sao_Paulo",
            "active_test": false,
            "meu_modulo_flag": 42
        }}),
    )
    .await;
    let result = &answer["result"];
    assert_eq!(result["lang"], json!("pt_BR"), "{answer}");
    assert_eq!(result["tz"], json!("America/Sao_Paulo"));
    assert_eq!(result["active_test"], json!(false));
    assert_eq!(
        result["todo"]["meu_modulo_flag"],
        json!(42),
        "the key the framework does not know crosses unchanged: {answer}"
    );

    case.close().await;
}

#[tokio::test]
async fn active_test_off_shows_the_archived_records_live() {
    let Some(case) = TransactionCase::open("context_active", &["base"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    for (name, active) in [("Viva", true), ("Arquivada", false)] {
        call(
            &service,
            "res.partner",
            "create",
            json!([{"name": name, "active": active}]),
            json!({}),
        )
        .await;
    }

    let visible = call(
        &service,
        "res.partner",
        "search_count",
        json!([[]]),
        json!({}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    assert_eq!(visible, 1, "archived records stay out by default");

    // and the forms Python would treat as false all switch it off
    for off in [json!(false), json!(0), json!("")] {
        let all = call(
            &service,
            "res.partner",
            "search_count",
            json!([[]]),
            json!({"context": {"active_test": off}}),
        )
        .await["result"]
            .as_i64()
            .unwrap();
        assert_eq!(all, 2, "{off} switches active_test off, as in Python");
    }

    case.close().await;
}
