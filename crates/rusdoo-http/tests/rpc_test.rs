//! JSON-RPC 2.0 endpoints, mirroring odoo/http.py dispatch:
//! `/web/dataset/call_kw` (web client) and `/jsonrpc` (classic RPC).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn test_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_rpc_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("color", FieldType::Integer),
            Field::new("active", FieldType::Boolean),
        ],
    ))
    .unwrap();
    reg
}

fn test_service() -> OrmService {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    OrmService::insecure(
        Arc::new(test_registry()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
}

async fn rpc(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn unknown_model_returns_jsonrpc_error() {
    let app = router(test_service());

    let (status, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "call",
            "params": {"model": "res.nope", "method": "search", "args": [[]], "kwargs": {}}
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["jsonrpc"], json!("2.0"));
    assert_eq!(resp["id"], json!(7));
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown model"));
}

#[tokio::test]
async fn unknown_orm_method_is_method_not_found() {
    let app = router(test_service());

    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "explode", "args": [], "kwargs": {}}
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn malformed_envelope_is_invalid_request() {
    let app = router(test_service());

    let (_, resp) = rpc(app, "/web/dataset/call_kw", json!({"hello": "world"})).await;

    assert_eq!(resp["error"]["code"], json!(-32600));
    assert_eq!(resp["id"], Value::Null);
}

#[tokio::test]
async fn missing_model_param_is_invalid_params() {
    let app = router(test_service());

    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({"jsonrpc": "2.0", "id": 2, "method": "call", "params": {"method": "search"}}),
    )
    .await;

    assert_eq!(resp["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn classic_jsonrpc_execute_kw_reaches_dispatch() {
    let app = router(test_service());

    let (_, resp) = rpc(
        app,
        "/jsonrpc",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {
                "service": "object", "method": "execute_kw",
                "args": ["db", 1, "pwd", "res.nope", "search", [[]]]
            }
        }),
    )
    .await;

    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown model"));
}

#[tokio::test]
async fn unknown_service_is_method_not_found() {
    let app = router(test_service());

    let (_, resp) = rpc(
        app,
        "/jsonrpc",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "call",
            "params": {"service": "common", "method": "version", "args": []}
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn call_kw_crud_roundtrip_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // Arrange: fresh table + service over a real pool
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_rpc_partner""#)
        .execute(&pool)
        .await
        .unwrap();
    let reg = test_registry();
    reg.get("res.partner")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let service = OrmService::insecure(Arc::new(reg), pool);

    // create
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 10, "method": "call",
            "params": {"model": "res.partner", "method": "create",
                       "args": [{"name": "Gemini", "color": 7}], "kwargs": {}}
        }),
    )
    .await;
    let id = resp["result"].as_i64().expect("create returns the new id");

    // search_read
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 11, "method": "call",
            "params": {"model": "res.partner", "method": "search_read", "args": [],
                       "kwargs": {"domain": [["color", ">", 3]], "fields": ["name", "color"]}}
        }),
    )
    .await;
    assert_eq!(resp["result"][0]["id"], json!(id));
    assert_eq!(resp["result"][0]["name"], json!("Gemini"));
    assert_eq!(resp["result"][0]["color"], json!(7));

    // write
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 12, "method": "call",
            "params": {"model": "res.partner", "method": "write",
                       "args": [[id], {"color": 9}], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!(true));

    // read confirms the write
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 13, "method": "call",
            "params": {"model": "res.partner", "method": "read",
                       "args": [[id], ["color"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"][0]["color"], json!(9));

    // unlink + empty search
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 14, "method": "call",
            "params": {"model": "res.partner", "method": "unlink", "args": [[id]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!(true));
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 15, "method": "call",
            "params": {"model": "res.partner", "method": "search", "args": [[]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([]));
}

#[tokio::test]
async fn index_page_is_visible_in_a_browser() {
    let app = router(test_service());

    let response = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(page.contains("rusdoo"));
}

async fn rpc_full(
    app: axum::Router,
    uri: &str,
    body: Value,
    cookie: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut request = Request::post(uri).header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap(), set_cookie)
}

#[tokio::test]
async fn session_auth_flow_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_auth_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_auth_users""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("res.users")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let hash = rusdoo_http::session::hash_password("segredo").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("ana")), ("password", json!(hash))],
    )
    .await
    .unwrap();
    // auth REQUIRED on this service
    let service = OrmService::new(Arc::new(reg), pool);

    // without a session, call_kw is rejected with Odoo's code 100
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":1,"method":"call","params":{
            "model":"res.users","method":"search","args":[[]],"kwargs":{}}}),
        None,
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(100));

    // wrong password: error, no cookie
    let (_, resp, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":2,"method":"call","params":{
            "db":"x","login":"ana","password":"errada"}}),
        None,
    )
    .await;
    assert!(resp.get("error").is_some());
    assert!(cookie.is_none());

    // right password: uid + session cookie
    let (_, resp, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":3,"method":"call","params":{
            "db":"x","login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let uid = resp["result"]["uid"].as_i64().expect("uid in result");
    assert!(uid >= 1);
    let cookie = cookie.expect("session cookie set");
    let session = cookie.split(';').next().unwrap().to_string();

    // with the session, call_kw works
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":4,"method":"call","params":{
            "model":"res.users","method":"search_read","args":[],
            "kwargs":{"fields":["login"]}}}),
        Some(&session),
    )
    .await;
    assert_eq!(resp["result"][0]["login"], json!("ana"));

    // destroy: the session stops working
    rpc_full(
        router(service.clone()),
        "/web/session/destroy",
        json!({"jsonrpc":"2.0","id":5,"method":"call","params":{}}),
        Some(&session),
    )
    .await;
    let (_, resp, _) = rpc_full(
        router(service),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":6,"method":"call","params":{
            "model":"res.users","method":"search","args":[[]],"kwargs":{}}}),
        Some(&session),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(100));
}

#[tokio::test]
async fn password_field_is_never_readable_over_rpc() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_secret_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }),
            Field::new("password", FieldType::Char { size: None }).private(),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_secret_users""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("res.users")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let service = OrmService::insecure(Arc::new(reg), pool);

    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":1,"method":"call","params":{
            "model":"res.users","method":"search_read","args":[],
            "kwargs":{"fields":["login","password"]}}}),
    )
    .await;

    assert_eq!(resp["error"]["code"], json!(-32602));
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("password"));
}
