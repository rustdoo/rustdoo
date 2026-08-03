//! The chatter over RPC: posting on a record, reading its thread back,
//! and the two ways a message must not be posted.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
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

async fn fixture(url: &str, schema: &'static str) -> OrmService {
    let pool = pool(url, schema);
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_mail::extend(&mut registry).unwrap();
    for table in [
        "mail_followers",
        "mail_message",
        "res_users",
        "res_partner",
        "res_company",
        "res_country",
        "res_groups",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "res.country",
        "res.company",
        "res.partner",
        "res.groups",
        "res.users",
        "mail.message",
        "mail.followers",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    let mut methods = MethodRegistry::new();
    rusdoo_mail::extend_methods(&mut methods, &["res.partner"]).unwrap();
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

async fn a_partner(service: &OrmService, name: &str) -> i64 {
    call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "create",
               "args": [{"name": name}], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn a_message_is_posted_and_comes_back_newest_first_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_chatter_test")).await;
    let partner = a_partner(&service, "Ana").await;
    let other = a_partner(&service, "Bia").await;

    for body in ["primeira", "segunda"] {
        let answer = call(
            router(service.clone()),
            json!({"model": "res.partner", "method": "message_post",
                   "args": [[partner]], "kwargs": {"body": body}}),
        )
        .await;
        assert!(answer["result"].is_number(), "{answer}");
    }
    // a message on another record is not in this thread
    call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "message_post",
               "args": [[other]], "kwargs": {"body": "de outro registro"}}),
    )
    .await;

    let answer = call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "message_fetch",
               "args": [[partner]], "kwargs": {}}),
    )
    .await;
    let messages = answer["result"].as_array().expect("a thread: {answer}");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["body"], "segunda", "newest first");
    assert_eq!(messages[1]["body"], "primeira");
    // the author is resolved for the client, and the date is the moment
    // the row was written
    assert!(messages[0]["author"].is_string());
    assert!(messages[0]["date"].is_string());

    // and a record nobody wrote about has an empty thread, not an error
    let answer = call(
        router(service),
        json!({"model": "res.partner", "method": "message_fetch",
               "args": [[other + 1000]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"], json!([]));
}

#[tokio::test]
async fn an_empty_message_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_chatter_empty_test")).await;
    let partner = a_partner(&service, "Ana").await;
    for body in [json!(""), json!("   "), Value::Null] {
        let answer = call(
            router(service.clone()),
            json!({"model": "res.partner", "method": "message_post",
                   "args": [[partner]], "kwargs": {"body": body}}),
        )
        .await;
        assert!(answer.get("result").is_none(), "{answer}");
    }
    let answer = call(
        router(service),
        json!({"model": "res.partner", "method": "message_fetch",
               "args": [[partner]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"], json!([]), "nothing was posted");
}

#[tokio::test]
async fn a_body_is_stored_as_written_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_chatter_body_test")).await;
    let partner = a_partner(&service, "Ana").await;
    // the client renders a body as text, so the server keeps what was
    // typed instead of "sanitizing" it into something nobody wrote
    let written = "<b>negrito</b> & <script>alert(1)</script>";
    call(
        router(service.clone()),
        json!({"model": "res.partner", "method": "message_post",
               "args": [[partner]], "kwargs": {"body": written}}),
    )
    .await;
    let answer = call(
        router(service),
        json!({"model": "res.partner", "method": "message_fetch",
               "args": [[partner]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"][0]["body"], written);
}

#[tokio::test]
async fn a_thread_reads_one_record_at_a_time_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_chatter_many_test")).await;
    let one = a_partner(&service, "Ana").await;
    let two = a_partner(&service, "Bia").await;
    // posting "on both" would have to invent which record it is about
    let answer = call(
        router(service),
        json!({"model": "res.partner", "method": "message_post",
               "args": [[one, two]], "kwargs": {"body": "para os dois"}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
}
