//! A password written over RPC is a credential, not a string: it is
//! hashed on its way in, and what comes back out never matches what was
//! typed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
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

async fn fixture(url: &str, schema: &'static str) -> (OrmService, sqlx::PgPool) {
    let pool = pool(url, schema);
    let registry = rusdoo_base::registry().unwrap();
    for table in ["res_users", "res_partner", "res_company", "res_groups"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(r#"DROP TABLE IF EXISTS "res_groups_users_rel""#)
        .execute(&pool)
        .await
        .unwrap();
    for model in ["res.country", "res.company", "res.partner", "res.groups", "res.users"] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    (
        OrmService::insecure(Arc::new(registry), pool.clone()),
        pool,
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

/// Log in through the real endpoint: the uid on success, None on
/// failure. This is the path a password must actually satisfy.
async fn authenticate(app: axum::Router, login: &str, password: &str) -> Option<i64> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "call",
                      "params": {"login": login, "password": password}});
    let response = app
        .oneshot(
            Request::post("/web/session/authenticate")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    answer["result"]["uid"].as_i64()
}

async fn stored_password(pool: &sqlx::PgPool, login: &str) -> String {
    sqlx::query_scalar::<_, Option<String>>(r#"SELECT "password" FROM "res_users" WHERE "login" = $1"#)
        .bind(login)
        .fetch_one(pool)
        .await
        .unwrap()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_password_is_hashed_on_create_and_on_write_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, pool) = fixture(&url, "rusdoo_credentials_test").await;

    let created = call(
        router(service.clone()),
        json!({"model": "res.users", "method": "create",
               "args": [{"login": "joana", "name": "Joana", "password": "segredo123"}],
               "kwargs": {}}),
    )
    .await;
    let uid = created["result"].as_i64().expect("user created");

    let hash = stored_password(&pool, "joana").await;
    assert!(hash.starts_with("$argon2id$"), "stored: {hash}");
    assert!(!hash.contains("segredo123"), "the plaintext must not survive");
    // and the hash is what the login path verifies against
    assert_eq!(
        authenticate(router(service.clone()), "joana", "segredo123").await,
        Some(uid)
    );
    assert_eq!(
        authenticate(router(service.clone()), "joana", "outra-senha").await,
        None
    );

    // a write goes through the same hashing
    call(
        router(service.clone()),
        json!({"model": "res.users", "method": "write",
               "args": [[uid], {"password": "trocada456"}], "kwargs": {}}),
    )
    .await;
    let rehashed = stored_password(&pool, "joana").await;
    assert_ne!(rehashed, hash, "a new password is a new hash");
    assert_eq!(
        authenticate(router(service.clone()), "joana", "trocada456").await,
        Some(uid)
    );
    assert_eq!(
        authenticate(router(service), "joana", "segredo123").await,
        None
    );
}

#[tokio::test]
async fn an_empty_password_leaves_the_stored_one_alone_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, pool) = fixture(&url, "rusdoo_credentials_empty_test").await;
    let created = call(
        router(service.clone()),
        json!({"model": "res.users", "method": "create",
               "args": [{"login": "rui", "password": "inicial123"}], "kwargs": {}}),
    )
    .await;
    let uid = created["result"].as_i64().unwrap();
    let hash = stored_password(&pool, "rui").await;

    // a form that submits an untouched password field must not wipe it
    for empty in [json!(""), json!(false), Value::Null] {
        call(
            router(service.clone()),
            json!({"model": "res.users", "method": "write",
                   "args": [[uid], {"password": empty, "name": "Rui"}], "kwargs": {}}),
        )
        .await;
        assert_eq!(stored_password(&pool, "rui").await, hash);
    }
    assert_eq!(
        authenticate(router(service), "rui", "inicial123").await,
        Some(uid)
    );
}

#[tokio::test]
async fn the_hash_is_never_readable_over_rpc_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, _pool) = fixture(&url, "rusdoo_credentials_read_test").await;
    call(
        router(service.clone()),
        json!({"model": "res.users", "method": "create",
               "args": [{"login": "ana", "password": "segredo123"}], "kwargs": {}}),
    )
    .await;
    let answer = call(
        router(service),
        json!({"model": "res.users", "method": "search_read", "args": [[]],
               "kwargs": {"fields": ["id", "login", "password"]}}),
    )
    .await;
    assert!(
        answer.get("result").is_none(),
        "the password column must not be readable: {answer}"
    );
}

#[tokio::test]
async fn id_may_be_asked_for_like_any_other_field_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, _pool) = fixture(&url, "rusdoo_credentials_id_test").await;
    call(
        router(service.clone()),
        json!({"model": "res.groups", "method": "create",
               "args": [{"name": "Leitores"}], "kwargs": {}}),
    )
    .await;
    // the web client sends `id` in its field lists; it is always in the
    // answer, and asking for it is not an unknown-field error
    let answer = call(
        router(service),
        json!({"model": "res.groups", "method": "search_read", "args": [[]],
               "kwargs": {"fields": ["id", "name"]}}),
    )
    .await;
    let rows = answer["result"].as_array().expect("rows: {answer}");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Leitores");
    assert!(rows[0]["id"].is_number());
}
