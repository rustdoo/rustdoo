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
            // like every archivable Odoo model: the field defaults to true,
            // otherwise every new record would be born archived
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
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

#[tokio::test]
async fn access_control_enforced_live() {
    use rusdoo_orm::access::{AccessControl, Operation};

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_acl_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }).private(),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_acl_users""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("res.users")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    // first user is uid 1 (superuser), second is uid 2 (regular)
    let admin_hash = rusdoo_http::session::hash_password("admin").unwrap();
    let ana_hash = rusdoo_http::session::hash_password("segredo").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("admin")), ("password", json!(admin_hash))],
    )
    .await
    .unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("ana")), ("password", json!(ana_hash))],
    )
    .await
    .unwrap();

    // res.users is readable only by group 99 (which nobody belongs to)
    let mut acl = AccessControl::new();
    acl.grant("res.users", 99, &[Operation::Read]);
    let service = OrmService::new(Arc::new(reg), pool).with_access(acl);

    // ana (uid 2, no groups): read denied
    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":1,"method":"call","params":{"login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let ana = cookie.unwrap().split(';').next().unwrap().to_string();
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":2,"method":"call","params":{
            "model":"res.users","method":"search","args":[[]],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32000));
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not allowed"));

    // admin (uid 1, superuser): allowed
    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":3,"method":"call","params":{"login":"admin","password":"admin"}}),
        None,
    )
    .await;
    let admin = cookie.unwrap().split(';').next().unwrap().to_string();
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":4,"method":"call","params":{
            "model":"res.users","method":"search","args":[[]],"kwargs":{}}}),
        Some(&admin),
    )
    .await;
    assert!(resp.get("result").is_some(), "superuser bypasses ACL");

    // fields_get is ACL-gated (mapped to read) and omits private fields
    // ana (no grant) is denied
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":7,"method":"call","params":{
            "model":"res.users","method":"fields_get","args":[],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32000),
        "ana denied fields_get without a read grant"
    );
    // admin is allowed, and the private password field is NOT disclosed
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":8,"method":"call","params":{
            "model":"res.users","method":"fields_get","args":[],"kwargs":{}}}),
        Some(&admin),
    )
    .await;
    assert!(
        resp["result"]["login"].is_object(),
        "admin sees login metadata"
    );
    assert!(
        resp["result"].get("password").is_none(),
        "private password field must be omitted from fields_get"
    );

    // the classic /jsonrpc path enforces the ACL too (was a 100% bypass)
    let (_, resp) = rpc(
        router(service.clone()),
        "/jsonrpc",
        json!({"jsonrpc":"2.0","id":5,"method":"call","params":{
            "service":"object","method":"execute_kw",
            "args":["db", 2, "segredo", "res.users", "search", [[]]]}}),
    )
    .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32000),
        "ana denied over /jsonrpc"
    );

    // superuser over /jsonrpc is allowed
    let (_, resp) = rpc(
        router(service),
        "/jsonrpc",
        json!({"jsonrpc":"2.0","id":6,"method":"call","params":{
            "service":"object","method":"execute_kw",
            "args":["db", 1, "admin", "res.users", "search", [[]]]}}),
    )
    .await;
    assert!(
        resp.get("result").is_some(),
        "superuser bypasses ACL over /jsonrpc"
    );
}

async fn get_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// End-to-end navigation: an ir.ui.menu links to an ir.actions.act_window
/// which opens its res_model in a view. GET /web shows the menu; clicking
/// through GET /web/action/<xml_id> renders the model's records.
#[tokio::test]
async fn action_and_menu_navigation_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // isolate in a dedicated schema: this test uses the fixed table names
    // (ir_ui_view, ir_act_window, ...) that dispatch queries literally, so
    // it must not share `public` with the module-install tests
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(&mut *conn, "CREATE SCHEMA IF NOT EXISTS rusdoo_nav_test")
                    .await?;
                sqlx::Executor::execute(&mut *conn, "SET search_path TO rusdoo_nav_test").await?;
                Ok(())
            })
        })
        .connect_lazy(&url)
        .unwrap();

    let mut reg = Registry::new();
    for (name, table, fields) in [
        (
            "ir.actions.act_window",
            "ir_act_window",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new("res_model", FieldType::Char { size: None }).required(),
                Field::new("domain", FieldType::Text),
            ],
        ),
        (
            "ir.ui.menu",
            "ir_ui_menu",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new(
                    "parent_id",
                    FieldType::Many2one {
                        comodel: "ir.ui.menu".into(),
                    },
                ),
                Field::new("sequence", FieldType::Integer),
                Field::new("action", FieldType::Char { size: None }),
            ],
        ),
        (
            "ir.ui.view",
            "ir_ui_view",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new("model", FieldType::Char { size: None }),
                Field::new("arch", FieldType::Text),
            ],
        ),
        (
            "res.partner",
            "res_partner",
            vec![Field::new("name", FieldType::Char { size: None }).required()],
        ),
    ] {
        reg.register(Model::new(
            ModelMeta {
                name: name.into(),
                table: table.into(),
                inherit: vec![],
                inherits: vec![],
            },
            fields,
        ))
        .unwrap();
    }

    for t in ["ir_act_window", "ir_ui_menu", "ir_ui_view", "res_partner"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in [
        "ir.actions.act_window",
        "ir.ui.menu",
        "ir.ui.view",
        "res.partner",
    ] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    // ir_model_data is a raw system table (not a registered model here)
    sqlx::query(r#"DROP TABLE IF EXISTS "ir_model_data""#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "ir_model_data" ("module" varchar, "name" varchar,
           "model" varchar, "res_id" int4)"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    // seed: two partners, a listing view, an action scoped by a domain,
    // its external id, a menu
    reg.create(&pool, "res.partner", vec![("name", json!("Alice"))])
        .await
        .unwrap();
    reg.create(&pool, "res.partner", vec![("name", json!("Bob"))])
        .await
        .unwrap();
    reg.create(
        &pool,
        "ir.ui.view",
        vec![
            ("name", json!("Parceiros")),
            ("model", json!("res.partner")),
            (
                "arch",
                json!(r#"<div><t t-foreach="records" t-as="p"><span t-esc="p.name"/></t></div>"#),
            ),
        ],
    )
    .await
    .unwrap();
    let action = reg
        .create(
            &pool,
            "ir.actions.act_window",
            vec![
                ("name", json!("Parceiros")),
                ("res_model", json!("res.partner")),
                // scope the action to Alice only
                ("domain", json!(r#"[["name", "=", "Alice"]]"#)),
            ],
        )
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id")
           VALUES ('test', 'act_partners', 'ir.actions.act_window', $1)"#,
    )
    .bind(action as i32)
    .execute(&pool)
    .await
    .unwrap();
    reg.create(
        &pool,
        "ir.ui.menu",
        vec![
            ("name", json!("Parceiros")),
            ("action", json!("test.act_partners")),
            ("sequence", json!(1)),
        ],
    )
    .await
    .unwrap();

    let service = OrmService::insecure(Arc::new(reg), pool);

    // clicking the action renders the partner list, scoped by the domain:
    // Alice is shown, Bob (filtered out by the domain) is not
    let (status, html) = get_html(router(service.clone()), "/web/action/test.act_partners").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Alice"),
        "action page must render records: {html}"
    );
    assert!(
        !html.contains("Bob"),
        "action domain must scope the rows (Bob filtered out): {html}"
    );

    // /web shows the menu linking to that action
    let (status, html) = get_html(router(service), "/web").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("/web/action/test.act_partners"),
        "menu must link to the action: {html}"
    );
}

#[tokio::test]
async fn fields_get_returns_field_metadata() {
    // fields_get reads only the registry — no live DB needed
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "fields_get", "args": [], "kwargs": {}}
        }),
    )
    .await;
    let fields = &resp["result"];
    assert_eq!(fields["name"]["type"], json!("char"));
    assert_eq!(fields["name"]["required"], json!(true));
    assert_eq!(fields["color"]["type"], json!("integer"));
    // injected LOG_ACCESS fields surface as readonly many2one to res.users
    assert_eq!(fields["create_uid"]["type"], json!("many2one"));
    assert_eq!(fields["create_uid"]["relation"], json!("res.users"));
    assert_eq!(fields["create_uid"]["readonly"], json!(true));
}

#[tokio::test]
async fn fields_get_rejects_malformed_allfields() {
    // a bare string instead of a list is a client bug — surface it rather
    // than silently returning every field
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {"model": "res.partner", "method": "fields_get",
                       "args": ["name"], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "malformed allfields");
}

#[tokio::test]
async fn default_get_returns_empty_when_no_defaults() {
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "res.partner", "method": "default_get",
                       "args": [["name", "color"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!({}));
}

// ---------------------------------------------------------------------------
// web_read / web_search_read — the Odoo 19 web client's read path
// ---------------------------------------------------------------------------

/// Registry with relational fields, for the web_read shaping tests.
fn web_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_web_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new(
                "country_id",
                FieldType::Many2one {
                    comodel: "res.country".into(),
                },
            ),
            Field::new(
                "category_ids",
                FieldType::Many2many {
                    comodel: "res.partner.category".into(),
                    relation: "rusdoo_test_web_pc_rel".into(),
                    column1: "partner_id".into(),
                    column2: "category_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.country".into(),
            table: "rusdoo_test_web_country".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("code", FieldType::Char { size: None }),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner.category".into(),
            table: "rusdoo_test_web_category".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg
}

#[tokio::test]
async fn web_read_refuses_unsupported_spec_keys() {
    // `context`/`order` in a sub-spec change which rows come back; until
    // they are implemented the server must refuse, never silently ignore
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "web_search_read", "args": [],
                       "kwargs": {"domain": [],
                                  "specification": {"name": {"context": {"lang": "pt_BR"}}}}}
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602));
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("context"));
}

#[tokio::test]
async fn web_spec_wider_than_the_node_cap_is_refused() {
    // depth is capped elsewhere; width must be too, or one request fans
    // out into an unbounded number of recursive reads
    let mut spec = serde_json::Map::new();
    for i in 0..201 {
        spec.insert(format!("f{i}"), json!({}));
    }
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[1], spec], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602));
    assert!(resp["error"]["message"].as_str().unwrap().contains("200"));
}

#[tokio::test]
async fn web_read_nested_acl_and_exposure_enforced_live() {
    use rusdoo_orm::access::{AccessControl, Operation};

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_webacl_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_webacl_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }).private(),
            Field::new(
                "groups_id",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "rusdoo_test_webacl_ug_rel".into(),
                    column1: "user_id".into(),
                    column2: "group_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_webacl_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new(
                "user_id",
                FieldType::Many2one {
                    comodel: "res.users".into(),
                },
            ),
            Field::new(
                "category_ids",
                FieldType::Many2many {
                    comodel: "res.partner.category".into(),
                    relation: "rusdoo_test_webacl_pc_rel".into(),
                    column1: "partner_id".into(),
                    column2: "category_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner.category".into(),
            table: "rusdoo_test_webacl_category".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_webacl_ug_rel",
        "rusdoo_test_webacl_pc_rel",
        "rusdoo_test_webacl_users",
        "rusdoo_test_webacl_groups",
        "rusdoo_test_webacl_partner",
        "rusdoo_test_webacl_category",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in [
        "res.groups",
        "res.users",
        "res.partner",
        "res.partner.category",
    ] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    // uid 1 is the superuser; ana is uid 2, member of one group
    let admin_hash = rusdoo_http::session::hash_password("admin").unwrap();
    let ana_hash = rusdoo_http::session::hash_password("segredo").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("admin")), ("password", json!(admin_hash))],
    )
    .await
    .unwrap();
    let ana_uid = reg
        .create(
            &pool,
            "res.users",
            vec![
                ("name", json!("Ana")),
                ("login", json!("ana")),
                ("password", json!(ana_hash)),
            ],
        )
        .await
        .unwrap();
    let group = reg
        .create(&pool, "res.groups", vec![("name", json!("vendas"))])
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "rusdoo_test_webacl_ug_rel" VALUES ($1, $2)"#)
        .bind(ana_uid)
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    let cat = reg
        .create(&pool, "res.partner.category", vec![("name", json!("vip"))])
        .await
        .unwrap();
    let shop = reg
        .create(
            &pool,
            "res.partner",
            vec![("name", json!("Loja")), ("user_id", json!(ana_uid))],
        )
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "rusdoo_test_webacl_pc_rel" VALUES ($1, $2)"#)
        .bind(shop)
        .bind(cat)
        .execute(&pool)
        .await
        .unwrap();

    // ana's group may read partners — NOT users, NOT categories
    let mut acl = AccessControl::new();
    acl.grant("res.partner", group, &[Operation::Read]);
    let service = OrmService::new(Arc::new(reg), pool).with_access(acl);

    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":1,"method":"call","params":{"login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let ana = cookie.unwrap().split(';').next().unwrap().to_string();
    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":2,"method":"call","params":{"login":"admin","password":"admin"}}),
        None,
    )
    .await;
    let admin = cookie.unwrap().split(';').next().unwrap().to_string();

    // ana reads partners, and the m2o display_name resolves without read
    // access on res.users (Odoo computes display_name with sudo)
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":3,"method":"call","params":{
            "model":"res.partner","method":"web_search_read","args":[],
            "kwargs":{"domain":[],
                      "specification":{"name":{},
                                       "user_id":{"fields":{"display_name":{}}}}}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(1), "resp: {resp}");
    assert_eq!(
        resp["result"]["records"][0]["user_id"],
        json!({"id": ana_uid, "display_name": "Ana"})
    );

    // any real sub-field beyond display_name needs read access on the
    // comodel — ana has none on res.users
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":4,"method":"call","params":{
            "model":"res.partner","method":"web_read",
            "args":[[shop], {"user_id":{"fields":{"display_name":{},"login":{}}}}],
            "kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32000), "resp: {resp}");

    // same for an x2many that asks for real fields
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":5,"method":"call","params":{
            "model":"res.partner","method":"web_read",
            "args":[[shop], {"category_ids":{"fields":{"name":{}}}}],
            "kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32000), "resp: {resp}");

    // but `fields: {}` reads nothing from the comodel: ana still gets the
    // {id} stubs, exactly like Odoo's unreadable-comodel degrade
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":6,"method":"call","params":{
            "model":"res.partner","method":"web_read",
            "args":[[shop], {"category_ids":{"fields":{}}}],
            "kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["result"][0]["category_ids"], json!([{"id": cat}]));

    // field exposure survives nesting even for the superuser: a private
    // field (password) is not readable through a m2o sub-spec
    let (_, resp, _) = rpc_full(
        router(service),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":7,"method":"call","params":{
            "model":"res.partner","method":"web_read",
            "args":[[shop], {"user_id":{"fields":{"password":{}}}}],
            "kwargs":{}}}),
        Some(&admin),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "resp: {resp}");
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("password"));
}

#[tokio::test]
async fn web_read_shapes_records_by_specification_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    for t in [
        "rusdoo_test_web_pc_rel",
        "rusdoo_test_web_partner",
        "rusdoo_test_web_country",
        "rusdoo_test_web_category",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let reg = web_registry();
    for m in ["res.partner", "res.country", "res.partner.category"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let country = reg.get("res.country").unwrap();
    let cat = reg.get("res.partner.category").unwrap();
    let partner = reg.get("res.partner").unwrap();
    let br = country
        .create(
            &pool,
            vec![("name", json!("Brasil")), ("code", json!("BR"))],
        )
        .await
        .unwrap();
    let vip = cat
        .create(&pool, vec![("name", json!("vip"))])
        .await
        .unwrap();
    let dev = cat
        .create(&pool, vec![("name", json!("dev"))])
        .await
        .unwrap();
    let ana = partner
        .create(
            &pool,
            vec![("name", json!("Ana")), ("country_id", json!(br))],
        )
        .await
        .unwrap();
    let bob = partner
        .create(&pool, vec![("name", json!("Bob"))])
        .await
        .unwrap();
    for c in [vip, dev] {
        sqlx::query(r#"INSERT INTO "rusdoo_test_web_pc_rel" VALUES ($1, $2)"#)
            .bind(ana)
            .bind(c)
            .execute(&pool)
            .await
            .unwrap();
    }
    let service = OrmService::insecure(Arc::new(reg), pool);

    // relational fields shaped by their sub-spec: m2o as {id, display_name},
    // x2many as a list of shaped records
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 20, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[ana, bob],
                                {"name": {},
                                 "country_id": {"fields": {"display_name": {}}},
                                 "category_ids": {"fields": {"display_name": {}}}}],
                       "kwargs": {}}
        }),
    )
    .await;
    let records = resp["result"]
        .as_array()
        .unwrap_or_else(|| panic!("web_read must return a list, got: {resp}"));
    assert_eq!(records.len(), 2);
    let rec_ana = records.iter().find(|r| r["id"] == json!(ana)).unwrap();
    let rec_bob = records.iter().find(|r| r["id"] == json!(bob)).unwrap();
    assert_eq!(rec_ana["name"], json!("Ana"));
    assert_eq!(
        rec_ana["country_id"],
        json!({"id": br, "display_name": "Brasil"})
    );
    let cats = rec_ana["category_ids"].as_array().unwrap();
    assert_eq!(cats.len(), 2);
    assert!(cats.contains(&json!({"id": vip, "display_name": "vip"})));
    assert!(cats.contains(&json!({"id": dev, "display_name": "dev"})));
    // an empty m2o is false, an empty x2many is []
    assert_eq!(rec_bob["country_id"], json!(false));
    assert_eq!(rec_bob["category_ids"], json!([]));

    // no `fields` sub-spec: m2o degrades to the raw id, x2many to id lists
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 21, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[ana], {"country_id": {}, "category_ids": {}}],
                       "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"][0]["country_id"], json!(br));
    let ids = resp["result"][0]["category_ids"].as_array().unwrap();
    let mut ids: Vec<i64> = ids.iter().map(|v| v.as_i64().unwrap()).collect();
    ids.sort();
    let mut expected = vec![vip, dev];
    expected.sort();
    assert_eq!(ids, expected);

    // m2o sub-spec may ask for more than display_name
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 22, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[ana],
                                {"country_id": {"fields": {"display_name": {}, "code": {}}}}],
                       "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(
        resp["result"][0]["country_id"],
        json!({"id": br, "display_name": "Brasil", "code": "BR"})
    );

    // an empty specification reads just the ids
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 23, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[bob], {}], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([{"id": bob}]));

    // the web client routinely asks for `id` explicitly — it is not a
    // registry field, but must never error
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 24, "method": "call",
            "params": {"model": "res.partner", "method": "web_read",
                       "args": [[bob], {"id": {}, "name": {}}], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([{"id": bob, "name": "Bob"}]));
}

#[tokio::test]
async fn web_search_read_returns_length_and_records_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_wsr_partner""#)
        .execute(&pool)
        .await
        .unwrap();
    // own table — the crud roundtrip test runs in parallel on the shared one
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_wsr_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("color", FieldType::Integer),
        ],
    ))
    .unwrap();
    let partner = reg.get("res.partner").unwrap();
    partner.init_table(&pool).await.unwrap();
    for (name, color) in [("P1", 1), ("P2", 2), ("P3", 3)] {
        partner
            .create(&pool, vec![("name", json!(name)), ("color", json!(color))])
            .await
            .unwrap();
    }
    let service = OrmService::insecure(Arc::new(reg), pool);

    // page 1: the limit is hit, so length is the full count
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 30, "method": "call",
            "params": {"model": "res.partner", "method": "web_search_read", "args": [],
                       "kwargs": {"domain": [], "specification": {"name": {}},
                                  "limit": 2}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(3));
    let records = resp["result"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["name"], json!("P1"));

    // last page: fewer rows than the limit, length = offset + rows
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 31, "method": "call",
            "params": {"model": "res.partner", "method": "web_search_read", "args": [],
                       "kwargs": {"domain": [], "specification": {"name": {}},
                                  "limit": 2, "offset": 2}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(3));
    assert_eq!(resp["result"]["records"].as_array().unwrap().len(), 1);

    // count_limit caps the count query
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 32, "method": "call",
            "params": {"model": "res.partner", "method": "web_search_read", "args": [],
                       "kwargs": {"domain": [], "specification": {"name": {}},
                                  "limit": 1, "count_limit": 2}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(2));

    // no match: an empty result with length 0
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 33, "method": "call",
            "params": {"model": "res.partner", "method": "web_search_read", "args": [],
                       "kwargs": {"domain": [["color", ">", 99]],
                                  "specification": {"name": {}}}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!({"length": 0, "records": []}));
}

// ---------------------------------------------------------------------------
// name_search / web_name_search — many2one dropdown suggestions
// ---------------------------------------------------------------------------

/// Fresh partner table for the name_search tests. Each test passes its own
/// table name — live tests run in parallel and share the database.
async fn name_search_service(pool: sqlx::PgPool, table: &str) -> (OrmService, Vec<i64>) {
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: table.into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("color", FieldType::Integer),
        ],
    ))
    .unwrap();
    let partner = reg.get("res.partner").unwrap();
    partner.init_table(&pool).await.unwrap();
    let mut ids = Vec::new();
    for (name, color) in [("Ana Silva", 1), ("Anastácia", 2), ("Bob", 3)] {
        ids.push(
            partner
                .create(&pool, vec![("name", json!(name)), ("color", json!(color))])
                .await
                .unwrap(),
        );
    }
    (OrmService::insecure(Arc::new(reg), pool), ids)
}

#[tokio::test]
async fn name_search_returns_id_name_pairs_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let (service, ids) = name_search_service(pool, "rusdoo_test_ns_partner").await;
    let (ana, anastacia, bob) = (ids[0], ids[1], ids[2]);

    // default ilike: substring, case-insensitive, [id, display_name] pairs
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 40, "method": "call",
            "params": {"model": "res.partner", "method": "name_search",
                       "args": ["ana"], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(
        resp["result"],
        json!([[ana, "Ana Silva"], [anastacia, "Anastácia"]]),
        "resp: {resp}"
    );

    // limit caps the suggestions
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 41, "method": "call",
            "params": {"model": "res.partner", "method": "name_search",
                       "args": [], "kwargs": {"name": "ana", "limit": 1}}
        }),
    )
    .await;
    assert_eq!(resp["result"].as_array().unwrap().len(), 1);

    // an extra domain restricts further (dropdowns pass the field's domain)
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 42, "method": "call",
            "params": {"model": "res.partner", "method": "name_search",
                       "args": [], "kwargs": {"name": "ana",
                                              "domain": [["color", "=", 1]]}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([[ana, "Ana Silva"]]));

    // operator "=" is exact match
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 43, "method": "call",
            "params": {"model": "res.partner", "method": "name_search",
                       "args": [], "kwargs": {"name": "Bob", "operator": "="}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([[bob, "Bob"]]));

    // an empty pattern matches everything (the dropdown's initial state)
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 44, "method": "call",
            "params": {"model": "res.partner", "method": "name_search",
                       "args": [], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn web_name_search_shapes_by_specification_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let (service, ids) = name_search_service(pool, "rusdoo_test_wns_partner").await;
    let bob = ids[2];

    // display_name-only spec: the compact {id, display_name} shape
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 45, "method": "call",
            "params": {"model": "res.partner", "method": "web_name_search",
                       "args": [], "kwargs": {"name": "bob",
                                              "specification": {"display_name": {}}}}
        }),
    )
    .await;
    let rec = &resp["result"][0];
    assert_eq!(rec["id"], json!(bob), "resp: {resp}");
    assert_eq!(rec["display_name"], json!("Bob"));

    // a wider spec goes through web_read shaping
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 46, "method": "call",
            "params": {"model": "res.partner", "method": "web_name_search",
                       "args": [], "kwargs": {"name": "bob",
                                              "specification": {"display_name": {}, "color": {}}}}
        }),
    )
    .await;
    assert_eq!(
        resp["result"],
        json!([{"id": bob, "display_name": "Bob", "color": 3}])
    );
}

/// Order/line registry with distinct table names, the shape a form view
/// saves: scalar fields plus one2many command tuples.
fn save_registry(prefix: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.save.line".into(),
            table: format!("{prefix}_line"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("qty", FieldType::Integer),
            Field::new(
                "order_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.save.order".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.save.order".into(),
            table: format!("{prefix}_order"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "line_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.save.line".into(),
                    inverse: "order_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

#[tokio::test]
async fn web_save_creates_then_writes_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let reg = save_registry("rusdoo_test_ws");
    for t in ["rusdoo_test_ws_line", "rusdoo_test_ws_order"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["rusdoo.test.save.order", "rusdoo.test.save.line"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let service = OrmService::insecure(Arc::new(reg), pool.clone());
    let spec = json!({"name": {}, "line_ids": {"fields": {"name": {}, "qty": {}}}});

    // no ids: web_save creates, then reads the new record back through
    // the same specification (one record, in a list)
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "web_save",
                       "args": [[],
                                {"name": "SO001",
                                 "line_ids": [[0, 0, {"name": "l1", "qty": 2}]]},
                                spec],
                       "kwargs": {}}
        }),
    )
    .await;
    let records = resp["result"]
        .as_array()
        .unwrap_or_else(|| panic!("web_save must return a list, got: {resp}"));
    assert_eq!(records.len(), 1);
    let order = records[0]["id"].as_i64().unwrap();
    assert_eq!(records[0]["name"], json!("SO001"));
    let lines = records[0]["line_ids"].as_array().unwrap();
    assert_eq!(lines.len(), 1, "the create command produced the line");
    assert_eq!(lines[0]["name"], json!("l1"));
    assert_eq!(lines[0]["qty"], json!(2));
    let line = lines[0]["id"].as_i64().unwrap();

    // with ids: web_save writes, and the reply reflects the new state
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "web_save",
                       "args": [[order],
                                {"name": "SO001-rev",
                                 "line_ids": [[1, line, {"qty": 9}],
                                              [0, 0, {"name": "l2", "qty": 1}]]},
                                spec],
                       "kwargs": {}}
        }),
    )
    .await;
    let record = &resp["result"][0];
    assert_eq!(record["id"], json!(order));
    assert_eq!(record["name"], json!("SO001-rev"));
    let lines = record["line_ids"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    let l1 = lines.iter().find(|l| l["id"] == json!(line)).unwrap();
    assert_eq!(l1["qty"], json!(9), "update command applied");

    // an empty save is a no-op that still reads the record back
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "web_save",
                       "args": [[order], {}, {"name": {}}], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!([{"id": order, "name": "SO001-rev"}]));

    // next_id: the pager moves on, so another record is read back
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "web_save",
                       "args": [[], {"name": "SO002"}, {"name": {}}], "kwargs": {}}
        }),
    )
    .await;
    let other = resp["result"][0]["id"].as_i64().unwrap();
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "web_save",
                       "args": [[order], {"name": "saved"}, {"name": {}}, other],
                       "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(
        resp["result"],
        json!([{"id": other, "name": "SO002"}]),
        "next_id decides which record comes back"
    );
    // ...and the write still landed on the saved record
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 6, "method": "call",
            "params": {"model": "rusdoo.test.save.order", "method": "read",
                       "args": [[order], ["name"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"][0]["name"], json!("saved"));
}

#[tokio::test]
async fn web_save_and_commands_enforce_access_live() {
    use rusdoo_orm::access::{AccessControl, Operation};

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = save_registry("rusdoo_test_wsa");
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_wsa_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_wsa_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }).private(),
            Field::new(
                "groups_id",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "rusdoo_test_wsa_ug_rel".into(),
                    column1: "user_id".into(),
                    column2: "group_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_wsa_ug_rel",
        "rusdoo_test_wsa_users",
        "rusdoo_test_wsa_groups",
        "rusdoo_test_wsa_line",
        "rusdoo_test_wsa_order",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in [
        "res.groups",
        "res.users",
        "rusdoo.test.save.order",
        "rusdoo.test.save.line",
    ] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let admin_hash = rusdoo_http::session::hash_password("admin").unwrap();
    let ana_hash = rusdoo_http::session::hash_password("segredo").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("admin")), ("password", json!(admin_hash))],
    )
    .await
    .unwrap();
    let ana_uid = reg
        .create(
            &pool,
            "res.users",
            vec![("login", json!("ana")), ("password", json!(ana_hash))],
        )
        .await
        .unwrap();
    let group = reg
        .create(&pool, "res.groups", vec![("name", json!("vendas"))])
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "rusdoo_test_wsa_ug_rel" VALUES ($1, $2)"#)
        .bind(ana_uid)
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    let order = reg
        .create(&pool, "rusdoo.test.save.order", vec![("name", json!("SO"))])
        .await
        .unwrap();
    let loose = reg
        .create(
            &pool,
            "rusdoo.test.save.line",
            vec![("name", json!("loose"))],
        )
        .await
        .unwrap();

    // ana may read and write orders, and write (link) lines — she may
    // neither create the order nor create/delete lines
    let mut acl = AccessControl::new();
    acl.grant(
        "rusdoo.test.save.order",
        group,
        &[Operation::Read, Operation::Write],
    );
    acl.grant("rusdoo.test.save.line", group, &[Operation::Write]);
    let service = OrmService::new(Arc::new(reg), pool.clone()).with_access(acl);

    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":1,"method":"call","params":{"login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let ana = cookie.unwrap().split(';').next().unwrap().to_string();

    // no ids means create — and ana has no create grant on the order
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":2,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[], {"name":"nova"}, {"name":{}}],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32000));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("create"),
        "web_save without ids needs create access: {resp}"
    );

    // writing an existing order is granted
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":3,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[order], {"name":"SO-rev"}, {"name":{}}],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert_eq!(resp["result"], json!([{"id": order, "name": "SO-rev"}]));

    // linking an existing line writes the line — granted
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":4,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[order], {"line_ids": [[4, loose, 0]]}, {"name":{}}],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert!(resp.get("result").is_some(), "link command allowed: {resp}");

    // creating a line through a command needs create access ON THE LINE
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":5,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[order], {"line_ids": [[0, 0, {"name":"nova"}]]}, {"name":{}}],
            "kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("create on rusdoo.test.save.line"),
        "a create command must be checked on the comodel: {resp}"
    );

    // ...and deleting one needs unlink access on the line
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":6,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[order], {"line_ids": [[2, loose, 0]]}, {"name":{}}],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unlink on rusdoo.test.save.line"),
        "a delete command must be checked on the comodel: {resp}"
    );

    // the plain write path is gated exactly the same way
    let (_, resp, _) = rpc_full(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":7,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"write",
            "args":[[order], {"line_ids": [[0, 0, {"name":"nova"}]]}],"kwargs":{}}}),
        Some(&ana),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("create on rusdoo.test.save.line"),
        "write() must not be a back door into the comodel: {resp}"
    );

    // nothing of the refused calls reached the database
    let lines: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_wsa_line""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lines, 1, "no line was created or deleted by a denied call");

    // the superuser still creates through web_save
    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":8,"method":"call","params":{"login":"admin","password":"admin"}}),
        None,
    )
    .await;
    let admin = cookie.unwrap().split(';').next().unwrap().to_string();
    let (_, resp, _) = rpc_full(
        router(service),
        "/web/dataset/call_kw",
        json!({"jsonrpc":"2.0","id":9,"method":"call","params":{
            "model":"rusdoo.test.save.order","method":"web_save",
            "args":[[], {"name":"admin cria", "line_ids": [[0, 0, {"name":"l"}]]},
                    {"name":{}, "line_ids": {"fields": {"name": {}}}}],
            "kwargs":{}}}),
        Some(&admin),
    )
    .await;
    let record = &resp["result"][0];
    assert_eq!(record["name"], json!("admin cria"));
    assert_eq!(record["line_ids"].as_array().unwrap().len(), 1);
}

fn group_registry(prefix: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.country".into(),
            table: format!("{prefix}_country"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.rg.sale".into(),
            table: format!("{prefix}_sale"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("amount", FieldType::Integer),
            Field::new("day", FieldType::Date),
            Field::new("secret", FieldType::Integer).private(),
            Field::new(
                "country_id",
                FieldType::Many2one {
                    comodel: "res.country".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

#[tokio::test]
async fn read_group_shapes_groups_for_the_web_client_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let reg = group_registry("rusdoo_test_rg");
    for t in ["rusdoo_test_rg_sale", "rusdoo_test_rg_country"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.country", "rusdoo.test.rg.sale"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let br = reg
        .create(&pool, "res.country", vec![("name", json!("Brasil"))])
        .await
        .unwrap();
    let pt = reg
        .create(&pool, "res.country", vec![("name", json!("Portugal"))])
        .await
        .unwrap();
    for (name, amount, country, day) in [
        ("a", 10, Some(br), "2026-01-05"),
        ("b", 20, Some(br), "2026-01-20"),
        ("c", 5, Some(pt), "2026-02-03"),
        ("d", 7, None, "2026-02-11"),
    ] {
        reg.create(
            &pool,
            "rusdoo.test.rg.sale",
            vec![
                ("name", json!(name)),
                ("amount", json!(amount)),
                ("country_id", country.map_or(json!(null), |c| json!(c))),
                ("day", json!(day)),
            ],
        )
        .await
        .unwrap();
    }
    let service = OrmService::insecure(Arc::new(reg), pool.clone());

    // formatted_read_group: a many2one group carries [id, display_name]
    // and the domain that reopens it
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "rusdoo.test.rg.sale", "method": "formatted_read_group",
                       "args": [[], ["country_id"], ["amount:sum"]],
                       "kwargs": {"order": "amount:sum desc"}}
        }),
    )
    .await;
    let groups = resp["result"]
        .as_array()
        .unwrap_or_else(|| panic!("formatted_read_group must return a list, got: {resp}"));
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0]["country_id"], json!([br, "Brasil"]));
    assert_eq!(groups[0]["amount:sum"], json!(30));
    assert_eq!(
        groups[0]["__extra_domain"],
        json!([["country_id", "=", br]])
    );
    // the group with no country is false, and its domain is the unset one
    let empty = groups
        .iter()
        .find(|g| g["country_id"] == json!(false))
        .expect("the empty group is present");
    assert_eq!(empty["__extra_domain"], json!([["country_id", "=", false]]));

    // web_read_group adds __count and the total number of groups
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "rusdoo.test.rg.sale", "method": "web_read_group",
                       "args": [[], ["country_id"], []], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(3));
    let groups = resp["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    assert!(groups.iter().all(|g| g["__count"].is_number()));

    // a page of groups still reports how many there are in total
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {"model": "rusdoo.test.rg.sale", "method": "web_read_group",
                       "args": [[], ["country_id"], [], 1], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["groups"].as_array().unwrap().len(), 1);
    assert_eq!(
        resp["result"]["length"],
        json!(3),
        "length counts every group, not the page"
    );

    // a date bucket's domain is the half-open interval it covers — and
    // feeding it back as a search returns exactly the group's records
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "call",
            "params": {"model": "rusdoo.test.rg.sale", "method": "web_read_group",
                       "args": [[], ["day:month"], []], "kwargs": {}}
        }),
    )
    .await;
    let groups = resp["result"]["groups"].as_array().unwrap().clone();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["day:month"], json!("2026-01-01"));
    assert_eq!(
        groups[0]["__extra_domain"],
        json!([["day", ">=", "2026-01-01"], ["day", "<", "2026-02-01"]])
    );
    for group in &groups {
        let (_, resp) = rpc(
            router(service.clone()),
            "/web/dataset/call_kw",
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "call",
                "params": {"model": "rusdoo.test.rg.sale", "method": "search_count",
                           "args": [group["__extra_domain"]], "kwargs": {}}
            }),
        )
        .await;
        assert_eq!(
            resp["result"], group["__count"],
            "the group domain must select exactly the group"
        );
    }

    // the domain filters before grouping
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 6, "method": "call",
            "params": {"model": "rusdoo.test.rg.sale", "method": "web_read_group",
                       "args": [[["amount", ">=", 10]], ["country_id"], []], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(1));
    assert_eq!(resp["result"]["groups"][0]["__count"], json!(2));
}

#[tokio::test]
async fn read_group_refuses_what_it_cannot_answer_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    // own tables: the sibling grouping test drops and recreates its own
    let reg = group_registry("rusdoo_test_rgx");
    for t in ["rusdoo_test_rgx_sale", "rusdoo_test_rgx_country"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.country", "rusdoo.test.rg.sale"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let service = OrmService::insecure(Arc::new(reg), pool);

    let refused = [
        // an aggregate over a private field would disclose it
        json!({"model": "rusdoo.test.rg.sale", "method": "formatted_read_group",
               "args": [[], ["country_id"], ["secret:sum"]], "kwargs": {}}),
        // unknown field, unknown function, unknown granularity
        json!({"model": "rusdoo.test.rg.sale", "method": "formatted_read_group",
               "args": [[], ["nope"], []], "kwargs": {}}),
        json!({"model": "rusdoo.test.rg.sale", "method": "formatted_read_group",
               "args": [[], ["country_id"], ["amount:median"]], "kwargs": {}}),
        json!({"model": "rusdoo.test.rg.sale", "method": "web_read_group",
               "args": [[], ["day:fortnight"], []], "kwargs": {}}),
        // having filters on the aggregates; ignoring it would answer with
        // groups the caller excluded
        json!({"model": "rusdoo.test.rg.sale", "method": "formatted_read_group",
               "args": [[], ["country_id"], [], [["amount:sum", ">", 5]]], "kwargs": {}}),
        // unfolding is not implemented — better refused than silently folded
        json!({"model": "rusdoo.test.rg.sale", "method": "web_read_group",
               "args": [[], ["country_id"], []], "kwargs": {"auto_unfold": true}}),
        json!({"model": "rusdoo.test.rg.sale", "method": "web_read_group",
               "args": [[], ["country_id"], []],
               "kwargs": {"opening_info": [{"value": 1, "folded": false}]}}),
        // grouping by nothing has no meaning for the client
        json!({"model": "rusdoo.test.rg.sale", "method": "web_read_group",
               "args": [[], [], []], "kwargs": {}}),
        // a second level with a typo must not pass because only the first
        // level is read
        json!({"model": "rusdoo.test.rg.sale", "method": "web_read_group",
               "args": [[], ["country_id", "nope"], []], "kwargs": {}}),
    ];
    for (index, params) in refused.iter().enumerate() {
        let (_, resp) = rpc(
            router(service.clone()),
            "/web/dataset/call_kw",
            json!({"jsonrpc": "2.0", "id": index, "method": "call", "params": params}),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "call {index} must be refused: {params} -> {resp}"
        );
    }

    // auto_unfold: false is the list view's normal call, not a refusal
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({"jsonrpc": "2.0", "id": 99, "method": "call",
               "params": {"model": "rusdoo.test.rg.sale", "method": "web_read_group",
                          "args": [[], ["country_id"], []],
                          "kwargs": {"auto_unfold": false, "opening_info": []}}}),
    )
    .await;
    assert!(resp.get("result").is_some(), "{resp}");
}

#[tokio::test]
async fn default_get_serves_declared_and_context_defaults() {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_defaults_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new("color", FieldType::Integer).default_value(json!(7)),
            Field::new("token", FieldType::Char { size: None })
                .private()
                .default_value(json!("s3cret")),
        ],
    ))
    .unwrap();
    let service = OrmService::insecure(
        Arc::new(reg),
        rusdoo_orm::db::lazy_pool("postgres://localhost/does-not-matter").unwrap(),
    );

    // declared defaults, only for the fields asked for
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "default_get",
                       "args": [["name", "active", "color"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"], json!({"active": true, "color": 7}));

    // the client's context overrides them — how an action opens a form
    // with values already filled in
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "res.partner", "method": "default_get",
                       "args": [["name", "color"]],
                       "kwargs": {"context": {"default_name": "Ana", "default_color": 3,
                                              "default_nope": 1, "lang": "pt_BR"}}}
        }),
    )
    .await;
    assert_eq!(
        resp["result"],
        json!({"name": "Ana", "color": 3}),
        "a context default for a field not asked for is not invented"
    );

    // a private field is refused, not defaulted
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {"model": "res.partner", "method": "default_get",
                       "args": [["token"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");

    // an unknown field is an error, not a silent omission
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "call",
            "params": {"model": "res.partner", "method": "default_get",
                       "args": [["nope"]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
}

#[tokio::test]
async fn active_test_context_controls_archived_records_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_active_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_active_partner""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("res.partner")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let live = reg
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let archived = reg
        .create(
            &pool,
            "res.partner",
            vec![("name", json!("Antiga")), ("active", json!(false))],
        )
        .await
        .unwrap();
    let service = OrmService::insecure(Arc::new(reg), pool);

    let call = |method: &str, args: Value, kwargs: Value| {
        let service = service.clone();
        let method = method.to_string();
        async move {
            let (_, resp) = rpc(
                router(service),
                "/web/dataset/call_kw",
                json!({"jsonrpc": "2.0", "id": 1, "method": "call",
                       "params": {"model": "res.partner", "method": method,
                                  "args": args, "kwargs": kwargs}}),
            )
            .await;
            resp
        }
    };

    // every read path hides archived records by default...
    let resp = call("search", json!([[]]), json!({})).await;
    assert_eq!(resp["result"], json!([live]));
    let resp = call("search_count", json!([[]]), json!({})).await;
    assert_eq!(resp["result"], json!(1));
    let resp = call("web_search_read", json!([[], {"name": {}}]), json!({})).await;
    assert_eq!(resp["result"]["length"], json!(1));
    let resp = call("name_search", json!([""]), json!({})).await;
    assert_eq!(resp["result"], json!([[live, "Ana"]]));

    // ...and the context flag brings them back
    let ctx = json!({"context": {"active_test": false}});
    let resp = call("search", json!([[]]), ctx.clone()).await;
    assert_eq!(resp["result"], json!([live, archived]));
    let resp = call("search_count", json!([[]]), ctx.clone()).await;
    assert_eq!(resp["result"], json!(2));
    let resp = call("web_search_read", json!([[], {"name": {}}]), ctx.clone()).await;
    assert_eq!(resp["result"]["length"], json!(2));
    let resp = call("name_search", json!([""]), ctx).await;
    assert_eq!(resp["result"].as_array().unwrap().len(), 2);

    // a domain naming the field decides for itself, context or not
    let resp = call("search", json!([[["active", "=", false]]]), json!({})).await;
    assert_eq!(resp["result"], json!([archived]));

    // reading an archived record by id still works — active_test filters
    // searches, it does not make records unreadable
    let resp = call("read", json!([[archived], ["name"]]), json!({})).await;
    assert_eq!(resp["result"][0]["name"], json!("Antiga"));
}

/// What the Owl client fetches before drawing anything: its session and
/// the navigation tree.
#[tokio::test]
async fn webclient_boot_endpoints_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // own schema: dispatch queries ir_ui_menu/ir_model_data by their fixed
    // names, so this must not share `public` with the other system tests
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(&mut *conn, "CREATE SCHEMA IF NOT EXISTS rusdoo_boot_test")
                    .await?;
                sqlx::Executor::execute(&mut *conn, "SET search_path TO rusdoo_boot_test").await?;
                Ok(())
            })
        })
        .connect_lazy(&url)
        .unwrap();

    let mut reg = Registry::new();
    for (name, table, fields) in [
        (
            "ir.actions.act_window",
            "ir_act_window",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new("res_model", FieldType::Char { size: None }).required(),
            ],
        ),
        (
            "ir.ui.menu",
            "ir_ui_menu",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new(
                    "parent_id",
                    FieldType::Many2one {
                        comodel: "ir.ui.menu".into(),
                    },
                ),
                Field::new("sequence", FieldType::Integer),
                Field::new("action", FieldType::Char { size: None }),
            ],
        ),
        (
            "res.partner",
            "res_partner",
            vec![Field::new("name", FieldType::Char { size: None }).required()],
        ),
        (
            "res.users",
            "res_users",
            vec![
                Field::new("login", FieldType::Char { size: None }).required(),
                Field::new("password", FieldType::Char { size: None }).private(),
                Field::new("lang", FieldType::Char { size: None }),
                Field::new("tz", FieldType::Char { size: None }),
            ],
        ),
    ] {
        reg.register(Model::new(
            ModelMeta {
                name: name.into(),
                table: table.into(),
                inherit: vec![],
                inherits: vec![],
            },
            fields,
        ))
        .unwrap();
    }
    for t in ["ir_act_window", "ir_ui_menu", "res_partner", "res_users"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in [
        "ir.actions.act_window",
        "ir.ui.menu",
        "res.partner",
        "res.users",
    ] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    sqlx::query(r#"DROP TABLE IF EXISTS "ir_model_data""#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "ir_model_data" ("module" varchar, "name" varchar,
           "model" varchar, "res_id" int4)"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let action = reg
        .create(
            &pool,
            "ir.actions.act_window",
            vec![
                ("name", json!("Parceiros")),
                ("res_model", json!("res.partner")),
            ],
        )
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id")
           VALUES ('test', 'act_partners', 'ir.actions.act_window', $1)"#,
    )
    .bind(action as i32)
    .execute(&pool)
    .await
    .unwrap();
    // an app menu with no action of its own, and a child that has one
    let app = reg
        .create(
            &pool,
            "ir.ui.menu",
            vec![("name", json!("Vendas")), ("sequence", json!(1))],
        )
        .await
        .unwrap();
    let child = reg
        .create(
            &pool,
            "ir.ui.menu",
            vec![
                ("name", json!("Parceiros")),
                ("parent_id", json!(app)),
                ("action", json!("test.act_partners")),
                ("sequence", json!(1)),
            ],
        )
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id")
           VALUES ('test', 'menu_vendas', 'ir.ui.menu', $1)"#,
    )
    .bind(app as i32)
    .execute(&pool)
    .await
    .unwrap();

    let service = OrmService::insecure(Arc::new(reg), pool.clone());
    let (status, body) = get_html(router(service.clone()), "/web/webclient/load_menus").await;
    assert_eq!(status, StatusCode::OK);
    let menus: Value = serde_json::from_str(&body).unwrap();

    // the synthetic root holds the apps
    assert_eq!(menus["root"]["children"], json!([app]));
    assert_eq!(menus["root"]["id"], json!("root"));

    // an app opens the action of its first descendant that has one
    let app_menu = &menus[app.to_string()];
    assert_eq!(app_menu["name"], json!("Vendas"));
    assert_eq!(app_menu["children"], json!([child]));
    assert_eq!(app_menu["appID"], json!(app));
    assert_eq!(app_menu["xmlid"], json!("test.menu_vendas"));
    assert_eq!(app_menu["actionID"], json!(action));
    assert_eq!(app_menu["actionModel"], json!("ir.actions.act_window"));

    // the child carries its own action and points back at its app
    let child_menu = &menus[child.to_string()];
    assert_eq!(child_menu["appID"], json!(app));
    assert_eq!(child_menu["actionID"], json!(action));
    assert_eq!(child_menu["children"], json!([]));
    // no external id was recorded for the child
    assert_eq!(child_menu["xmlid"], json!(""));

    // an anonymous session gets the public answer, and the client routes
    // itself to the login page
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/session/get_session_info",
        json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {}}),
    )
    .await;
    assert_eq!(resp["result"]["uid"], json!(null));
    assert_eq!(resp["result"]["is_public"], json!(true));
    assert_eq!(resp["result"]["server_version"], json!("19.0"));

    // with a session it answers as that user
    let hash = rusdoo_http::session::hash_password("admin").unwrap();
    reg_users_insert(&pool, "admin", &hash).await;
    let secure = OrmService::new(Arc::new(boot_registry()), pool.clone());
    let (_, _, cookie) = rpc_full(
        router(secure.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":2,"method":"call",
               "params":{"login":"admin","password":"admin"}}),
        None,
    )
    .await;
    let cookie = cookie.unwrap().split(';').next().unwrap().to_string();
    let (_, resp, _) = rpc_full(
        router(secure.clone()),
        "/web/session/get_session_info",
        json!({"jsonrpc": "2.0", "id": 3, "method": "call", "params": {}}),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp["result"]["uid"], json!(1));
    assert_eq!(resp["result"]["username"], json!("admin"));
    assert_eq!(
        resp["result"]["is_system"],
        json!(true),
        "uid 1 is the superuser"
    );
    assert_eq!(resp["result"]["user_context"]["uid"], json!(1));
    // no language was set on the user: the server default, and no
    // timezone at all rather than one nobody chose
    assert_eq!(resp["result"]["user_context"]["lang"], json!("en_US"));
    assert_eq!(resp["result"]["user_context"]["tz"], json!(false));

    // ...and once the user has them, the context carries what they set
    sqlx::query(r#"UPDATE "res_users" SET "lang" = $1, "tz" = $2 WHERE "id" = 1"#)
        .bind("pt_BR")
        .bind("America/Sao_Paulo")
        .execute(&pool)
        .await
        .unwrap();
    let (_, resp, _) = rpc_full(
        router(secure.clone()),
        "/web/session/get_session_info",
        json!({"jsonrpc": "2.0", "id": 4, "method": "call", "params": {}}),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp["result"]["user_context"]["lang"], json!("pt_BR"));
    assert_eq!(
        resp["result"]["user_context"]["tz"],
        json!("America/Sao_Paulo")
    );

    // ...and the menus need that session
    let response = router(secure)
        .oneshot(
            Request::get("/web/webclient/load_menus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The same registry as the boot test, for the authenticated half.
fn boot_registry() -> Registry {
    let mut reg = Registry::new();
    for (name, table, fields) in [
        (
            "ir.ui.menu",
            "ir_ui_menu",
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new(
                    "parent_id",
                    FieldType::Many2one {
                        comodel: "ir.ui.menu".into(),
                    },
                ),
                Field::new("sequence", FieldType::Integer),
                Field::new("action", FieldType::Char { size: None }),
            ],
        ),
        (
            "res.users",
            "res_users",
            vec![
                Field::new("login", FieldType::Char { size: None }).required(),
                Field::new("password", FieldType::Char { size: None }).private(),
                Field::new("lang", FieldType::Char { size: None }),
                Field::new("tz", FieldType::Char { size: None }),
            ],
        ),
    ] {
        reg.register(Model::new(
            ModelMeta {
                name: name.into(),
                table: table.into(),
                inherit: vec![],
                inherits: vec![],
            },
            fields,
        ))
        .unwrap();
    }
    reg
}

async fn reg_users_insert(pool: &sqlx::PgPool, login: &str, hash: &str) {
    sqlx::query(r#"INSERT INTO "res_users" ("login", "password") VALUES ($1, $2)"#)
        .bind(login)
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
}

/// What an action load calls before its first search: the arch of the
/// views it opens plus the fields to render them.
#[tokio::test]
async fn get_views_returns_arch_and_fields_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // own schema: ir_ui_view is queried by its fixed table name
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    "CREATE SCHEMA IF NOT EXISTS rusdoo_views_test",
                )
                .await?;
                sqlx::Executor::execute(&mut *conn, "SET search_path TO rusdoo_views_test").await?;
                Ok(())
            })
        })
        .connect_lazy(&url)
        .unwrap();

    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "ir.ui.view".into(),
            table: "ir_ui_view".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("model", FieldType::Char { size: None }),
            Field::new("type", FieldType::Char { size: None }).default_value(json!("form")),
            Field::new("priority", FieldType::Integer).default_value(json!(16)),
            Field::new(
                "inherit_id",
                FieldType::Many2one {
                    comodel: "ir.ui.view".into(),
                },
            ),
            Field::new("arch", FieldType::Text),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "rusdoo_test_views_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("secret", FieldType::Char { size: None }).private(),
        ],
    ))
    .unwrap();
    for t in ["ir_ui_view", "rusdoo_test_views_partner"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["ir.ui.view", "res.partner"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    // two list views for the same model: the lower priority one wins
    let fallback = reg
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("Parceiros (alternativa)")),
                ("model", json!("res.partner")),
                ("type", json!("list")),
                ("priority", json!(99)),
                ("arch", json!("<list><field name=\"name\"/></list>")),
            ],
        )
        .await
        .unwrap();
    let list = reg
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("Parceiros")),
                ("model", json!("res.partner")),
                ("type", json!("list")),
                ("priority", json!(1)),
                ("arch", json!("<list><field name=\"name\"/></list>")),
            ],
        )
        .await
        .unwrap();
    let form = reg
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("Parceiro")),
                ("model", json!("res.partner")),
                ("type", json!("form")),
                ("arch", json!("<form><field name=\"name\"/></form>")),
            ],
        )
        .await
        .unwrap();
    let service = OrmService::insecure(Arc::new(reg), pool);

    // false means "the default view of this type"
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "get_views",
                       "args": [[[false, "list"], [false, "form"]]], "kwargs": {}}
        }),
    )
    .await;
    let result = &resp["result"];
    assert_eq!(
        result["views"]["list"]["id"],
        json!(list),
        "the lowest priority view wins, not the first created: {resp}"
    );
    assert_ne!(result["views"]["list"]["id"], json!(fallback));
    assert_eq!(result["views"]["form"]["id"], json!(form));
    assert!(result["views"]["form"]["arch"]
        .as_str()
        .unwrap()
        .contains("<form>"));
    assert_eq!(result["views"]["form"]["model"], json!("res.partner"));

    // the fields the client renders with — private ones stay out
    let fields = &result["models"]["res.partner"]["fields"];
    assert!(fields["name"].is_object());
    assert_eq!(fields["name"]["required"], json!(true));
    assert!(
        fields.get("secret").is_none(),
        "a private field must not be described"
    );

    // an explicit id is honoured
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "res.partner", "method": "get_views",
                       "args": [[[fallback, "list"]]], "kwargs": {}}
        }),
    )
    .await;
    assert_eq!(resp["result"]["views"]["list"]["id"], json!(fallback));

    // ...but only for the model and type it really is
    for (args, why) in [
        (json!([[[form, "list"]]]), "a form view asked for as a list"),
        (json!([[[999999, "list"]]]), "an id that does not exist"),
        (json!([[]]), "no views requested"),
        (json!([[["nope"]]]), "a malformed pair"),
    ] {
        let (_, resp) = rpc(
            router(service.clone()),
            "/web/dataset/call_kw",
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "call",
                "params": {"model": "res.partner", "method": "get_views",
                           "args": args, "kwargs": {}}
            }),
        )
        .await;
        assert!(resp.get("error").is_some(), "{why} must be refused: {resp}");
    }

    // a *default* view of a type the model has none of is omitted rather
    // than refused: the client asks for every kind it can draw, and the
    // two that exist should still reach it
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "call",
            "params": {"model": "res.partner", "method": "get_views",
                       "args": [[[false, "kanban"], [form, "form"]]], "kwargs": {}}
        }),
    )
    .await;
    let views = &resp["result"]["views"];
    assert!(views.get("form").is_some(), "the form still comes back: {resp}");
    assert!(views.get("kanban").is_none(), "no kanban to answer with: {resp}");

    // a view of another model is never served for this one
    let other = reg_view_for_other_model(&service).await;
    let (_, resp) = rpc(
        router(service),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "call",
            "params": {"model": "res.partner", "method": "get_views",
                       "args": [[[other, "list"]]], "kwargs": {}}
        }),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("belongs to model"),
        "{resp}"
    );
}

/// A list view declared on another model, to prove get_views refuses it.
async fn reg_view_for_other_model(service: &OrmService) -> i64 {
    let (_, resp) = rpc(
        router(service.clone()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 9, "method": "call",
            "params": {"model": "ir.ui.view", "method": "create",
                       "args": [{"name": "Outro", "model": "res.company",
                                 "type": "list", "arch": "<list/>"}],
                       "kwargs": {}}
        }),
    )
    .await;
    resp["result"].as_i64().unwrap()
}

/// Record rules over RPC: a user sees, writes and deletes only the rows
/// a rule leaves them.
#[tokio::test]
async fn record_rules_scope_every_path_live() {
    use rusdoo_orm::access::{AccessControl, Operation};
    use rusdoo_orm::rules::{RecordRules, Rule};

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_rule_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_rule_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }).private(),
            Field::new(
                "groups_id",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "rusdoo_test_rule_ug_rel".into(),
                    column1: "user_id".into(),
                    column2: "group_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.lead".into(),
            table: "rusdoo_test_rule_lead".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("amount", FieldType::Integer),
            Field::new(
                "user_id",
                FieldType::Many2one {
                    comodel: "res.users".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_rule_ug_rel",
        "rusdoo_test_rule_lead",
        "rusdoo_test_rule_users",
        "rusdoo_test_rule_groups",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.groups", "res.users", "rusdoo.test.lead"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let group = reg
        .create(&pool, "res.groups", vec![("name", json!("vendas"))])
        .await
        .unwrap();
    let admin_hash = rusdoo_http::session::hash_password("admin").unwrap();
    let ana_hash = rusdoo_http::session::hash_password("segredo").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("admin")), ("password", json!(admin_hash))],
    )
    .await
    .unwrap();
    let ana = reg
        .create(
            &pool,
            "res.users",
            vec![
                ("login", json!("ana")),
                ("password", json!(ana_hash)),
                ("groups_id", json!([[4, group, 0]])),
            ],
        )
        .await
        .unwrap();
    let mine = reg
        .create(
            &pool,
            "rusdoo.test.lead",
            vec![
                ("name", json!("meu")),
                ("amount", json!(10)),
                ("user_id", json!(ana)),
            ],
        )
        .await
        .unwrap();
    let theirs = reg
        .create(
            &pool,
            "rusdoo.test.lead",
            vec![
                ("name", json!("de outro")),
                ("amount", json!(90)),
                ("user_id", json!(1)),
            ],
        )
        .await
        .unwrap();

    // model-level access is wide open for everyone; the rule is what scopes
    let mut acl = AccessControl::new();
    acl.grant(
        "rusdoo.test.lead",
        group,
        &[
            Operation::Read,
            Operation::Write,
            Operation::Create,
            Operation::Unlink,
        ],
    );
    let mut rules = RecordRules::new();
    rules.add(Rule {
        model: "rusdoo.test.lead".into(),
        domain: json!([["user_id", "=", "user.id"]]),
        // global rule: it applies to every non-superuser
        groups: vec![],
        operations: vec![Operation::Read, Operation::Write, Operation::Unlink],
    });
    let service = OrmService::new(Arc::new(reg), pool.clone())
        .with_access(acl)
        .with_rules(rules);

    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":1,"method":"call",
               "params":{"login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let ana_cookie = cookie.unwrap().split(';').next().unwrap().to_string();
    let call = |method: &str, args: Value, cookie: String| {
        let service = service.clone();
        let method = method.to_string();
        async move {
            let (_, resp, _) = rpc_full(
                router(service),
                "/web/dataset/call_kw",
                json!({"jsonrpc":"2.0","id":2,"method":"call",
                       "params":{"model":"rusdoo.test.lead","method":method,
                                 "args":args,"kwargs":{}}}),
                Some(&cookie),
            )
            .await;
            resp
        }
    };

    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":3,"method":"call",
               "params":{"login":"admin","password":"admin"}}),
        None,
    )
    .await;
    let admin_cookie = cookie.unwrap().split(';').next().unwrap().to_string();

    // the superuser sees both records: rules never apply to them
    let resp = call("search", json!([[]]), admin_cookie.clone()).await;
    assert_eq!(resp["result"].as_array().unwrap().len(), 2, "{resp}");

    // every search path is scoped for ana
    let resp = call("search", json!([[]]), ana_cookie.clone()).await;
    assert_eq!(resp["result"], json!([mine]), "search is scoped: {resp}");
    let resp = call("search_count", json!([[]]), ana_cookie.clone()).await;
    assert_eq!(resp["result"], json!(1));
    let resp = call("search_read", json!([[], ["name"]]), ana_cookie.clone()).await;
    assert_eq!(resp["result"].as_array().unwrap().len(), 1);
    let resp = call(
        "web_search_read",
        json!([[], {"name": {}}]),
        ana_cookie.clone(),
    )
    .await;
    assert_eq!(resp["result"]["length"], json!(1));
    let resp = call(
        "web_read_group",
        json!([[], ["user_id"], []]),
        ana_cookie.clone(),
    )
    .await;
    assert_eq!(
        resp["result"]["length"],
        json!(1),
        "grouping is scoped: {resp}"
    );
    assert_eq!(resp["result"]["groups"][0]["__count"], json!(1));

    // reading someone else's record by id is refused, not silently empty
    let resp = call("read", json!([[theirs], ["name"]]), ana_cookie.clone()).await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not allowed to read"),
        "{resp}"
    );
    let resp = call(
        "web_read",
        json!([[theirs], {"name": {}}]),
        ana_cookie.clone(),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");
    // ...and her own record still reads
    let resp = call("read", json!([[mine], ["name"]]), ana_cookie.clone()).await;
    assert_eq!(resp["result"][0]["name"], json!("meu"));

    // writing and deleting someone else's record is refused
    let resp = call(
        "write",
        json!([[theirs], {"name": "sequestrado"}]),
        ana_cookie.clone(),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");
    let resp = call("unlink", json!([[theirs]]), ana_cookie.clone()).await;
    assert!(resp.get("error").is_some(), "{resp}");
    // a mixed batch is refused whole, not partially applied
    let resp = call(
        "write",
        json!([[mine, theirs], {"name": "os dois"}]),
        ana_cookie.clone(),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");
    let rows = sqlx::query_scalar::<_, String>(
        r#"SELECT "name" FROM "rusdoo_test_rule_lead" WHERE "id" = $1"#,
    )
    .bind(theirs as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, "de outro", "nothing of the refused writes landed");

    // her own record writes fine
    let resp = call("write", json!([[mine], {"amount": 11}]), ana_cookie.clone()).await;
    assert_eq!(resp["result"], json!(true), "{resp}");
}

/// A create rule constrains a record that does not exist yet, so the
/// check runs inside the insert's transaction: a refused record is rolled
/// back, never merely reported.
#[tokio::test]
async fn create_rules_are_enforced_in_the_insert_transaction_live() {
    use rusdoo_orm::access::{AccessControl, Operation};
    use rusdoo_orm::rules::{RecordRules, Rule};

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_crule_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "rusdoo_test_crule_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new("password", FieldType::Char { size: None }).private(),
            Field::new(
                "groups_id",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "rusdoo_test_crule_ug_rel".into(),
                    column1: "user_id".into(),
                    column2: "group_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.note".into(),
            table: "rusdoo_test_crule_note".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("user_id", FieldType::Integer),
        ],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_crule_ug_rel",
        "rusdoo_test_crule_note",
        "rusdoo_test_crule_users",
        "rusdoo_test_crule_groups",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.groups", "res.users", "rusdoo.test.note"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let group = reg
        .create(&pool, "res.groups", vec![("name", json!("notas"))])
        .await
        .unwrap();
    let admin_hash = rusdoo_http::session::hash_password("admin").unwrap();
    reg.create(
        &pool,
        "res.users",
        vec![("login", json!("admin")), ("password", json!(admin_hash))],
    )
    .await
    .unwrap();
    let ana_hash = rusdoo_http::session::hash_password("segredo").unwrap();
    let ana_uid = reg
        .create(
            &pool,
            "res.users",
            vec![
                ("login", json!("ana")),
                ("password", json!(ana_hash)),
                ("groups_id", json!([[4, group, 0]])),
            ],
        )
        .await
        .unwrap();
    let mut acl = AccessControl::new();
    acl.grant(
        "rusdoo.test.note",
        group,
        &[Operation::Create, Operation::Read, Operation::Write],
    );
    let mut rules = RecordRules::new();
    rules.add(Rule {
        model: "rusdoo.test.note".into(),
        domain: json!([["user_id", "=", "user.id"]]),
        groups: vec![],
        operations: vec![Operation::Create],
    });
    let service = OrmService::new(Arc::new(reg), pool.clone())
        .with_access(acl)
        .with_rules(rules);

    let (_, _, cookie) = rpc_full(
        router(service.clone()),
        "/web/session/authenticate",
        json!({"jsonrpc":"2.0","id":1,"method":"call",
               "params":{"login":"ana","password":"segredo"}}),
        None,
    )
    .await;
    let ana = cookie.unwrap().split(';').next().unwrap().to_string();
    // ana may only create notes that are hers
    let create = |method: &str, args: Value| {
        let service = service.clone();
        let method = method.to_string();
        let cookie = ana.clone();
        async move {
            let (_, resp, _) = rpc_full(
                router(service),
                "/web/dataset/call_kw",
                json!({"jsonrpc":"2.0","id":2,"method":"call",
                       "params":{"model":"rusdoo.test.note","method":method,
                                 "args":args,"kwargs":{}}}),
                Some(&cookie),
            )
            .await;
            resp
        }
    };

    let resp = create("create", json!([{"name": "minha", "user_id": ana_uid}])).await;
    assert!(
        resp["result"].is_number(),
        "a record she owns is created: {resp}"
    );
    let resp = create(
        "web_save",
        json!([[], {"name": "minha 2", "user_id": ana_uid},
                                         {"name": {}}]),
    )
    .await;
    assert_eq!(resp["result"][0]["name"], json!("minha 2"), "{resp}");

    // one that would belong to someone else is refused...
    for (method, args) in [
        ("create", json!([{"name": "alheia", "user_id": 1}])),
        (
            "web_save",
            json!([[], {"name": "alheia", "user_id": 1}, {"name": {}}]),
        ),
        // ...including one with no owner at all
        ("create", json!([{"name": "sem dono"}])),
    ] {
        let resp = create(method, args).await;
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not allowed to create"),
            "{method} must be refused: {resp}"
        );
    }

    // and the refused records were rolled back, not merely reported
    let left: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "rusdoo_test_crule_note" WHERE "name" LIKE 'alheia%'
              OR "name" = 'sem dono'"#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0, "a refused create must leave no row behind");
    let mine: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_crule_note""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mine, 2, "only her own two notes exist");
}

#[tokio::test]
async fn onchange_answers_no_change_for_a_model_without_logic() {
    let app = router(test_service());
    let (_, resp) = rpc(
        app,
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": {"model": "res.partner", "method": "onchange",
                       "args": [[], {"name": "Ana"}, ["name"], {"name": {}}],
                       "kwargs": {}}
        }),
    )
    .await;
    // the form view needs the call to succeed; nothing computes yet
    assert_eq!(resp["result"], json!({"value": {}}), "{resp}");

    // an unknown model is still an error, not a shrug
    let (_, resp) = rpc(
        router(test_service()),
        "/web/dataset/call_kw",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "call",
            "params": {"model": "nope.model", "method": "onchange",
                       "args": [[], {}, [], {}], "kwargs": {}}
        }),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");
}
