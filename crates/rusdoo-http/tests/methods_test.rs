//! Model methods over `call_kw`: a module's own business action, called
//! by name like any ORM method, and held to the access it declared.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_orm::access::{AccessControl, Operation};
use rusdoo_orm::methods::MethodRegistry;
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

/// A registry with the sale models, their tables fresh, and the sale
/// methods attached.
async fn fixture(url: &str, schema: &'static str) -> (OrmService, sqlx::PgPool) {
    let pool = pool(url, schema);
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut registry).unwrap();
    rusdoo_sale::extend(&mut registry).unwrap();
    for table in [
        "sale_order_line",
        "sale_order",
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
        // every registered model gets its table, like the real boot —
        // a document numbered by a sequence needs the sequence table
        "ir.sequence",
        "res.country",
        "res.company",
        "res.partner",
        "product.product",
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
    let mut methods = MethodRegistry::new();
    rusdoo_sale::extend_methods(&mut methods).unwrap();
    let service = OrmService::insecure(Arc::new(registry), pool.clone()).with_methods(methods);
    (service, pool)
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

async fn an_order(app: &axum::Router, name: &str) -> i64 {
    let partner = call(
        app.clone(),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": "Cliente"}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    call(
        app.clone(),
        json!({"model": "sale.order", "method": "create",
               "args": [{"name": name, "partner_id": partner}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap()
}

async fn state_of(app: axum::Router, id: i64) -> String {
    let answer = call(
        app,
        json!({"model": "sale.order", "method": "read", "args": [[id]],
               "kwargs": {"fields": ["state"]}}),
    )
    .await;
    answer["result"][0]["state"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_action_moves_the_order_and_refuses_the_moves_that_make_no_sense_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, _pool) = fixture(&url, "rusdoo_methods_test").await;
    let app = router(service.clone());
    let order = an_order(&app, "SO-M1").await;
    assert_eq!(state_of(router(service.clone()), order).await, "draft");

    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"], json!(true), "{answer}");
    assert_eq!(state_of(router(service.clone()), order).await, "sale");

    // confirming twice is not a no-op that quietly succeeds
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("não pode ir para"));

    // cancel, then back to a quotation
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_cancel",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert_eq!(state_of(router(service.clone()), order).await, "cancel");
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_draft",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert_eq!(state_of(router(service), order).await, "draft");
}

#[tokio::test]
async fn a_method_without_ids_says_so_instead_of_touching_everything_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, _pool) = fixture(&url, "rusdoo_methods_noids_test").await;
    let app = router(service.clone());
    let order = an_order(&app, "SO-M2").await;
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert_eq!(state_of(router(service), order).await, "draft");
}

#[tokio::test]
async fn the_acl_uses_the_operation_the_method_declared_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (insecure, _pool) = fixture(&url, "rusdoo_methods_acl_test").await;
    let app = router(insecure.clone());
    let order = an_order(&app, "SO-M3").await;

    // `action_confirm` writes, and the dispatch has no way to guess that
    // from the name: it asks the method. A user with only read access is
    // therefore refused it — the same gate `write` goes through.
    let mut access = AccessControl::new();
    access.grant("sale.order", 1, &[Operation::Read]);
    let service = insecure.with_access(access);
    assert_eq!(
        service.method_operation("sale.order", "action_confirm"),
        Some(Operation::Write)
    );
    assert_eq!(
        service.method_operation("sale.order", "read"),
        None,
        "a built-in name is not a module method"
    );
    assert_eq!(state_of(router(service), order).await, "draft");
}
