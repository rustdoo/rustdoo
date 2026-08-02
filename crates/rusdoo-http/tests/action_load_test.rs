//! `/web/action/load`: what the client gets when a menu is clicked.

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

/// The models an action load touches, in a schema of their own: the
/// queries name `ir_act_window` and `ir_model_data` literally, so they
/// must not share `public` — with the other live tests, nor with each
/// other, since the suite runs them in parallel.
async fn fixture(url: &str, schema: &str) -> OrmService {
    let schema = schema.to_string();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let schema = schema.clone();
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}") as &str,
                )
                .await?;
                sqlx::Executor::execute(&mut *conn, &format!("SET search_path TO {schema}") as &str)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap();

    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "ir.actions.act_window".into(),
            table: "ir_act_window".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("res_model", FieldType::Char { size: None }).required(),
            Field::new("view_mode", FieldType::Char { size: None }),
            Field::new("domain", FieldType::Text),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None }).required()],
    ))
    .unwrap();

    for table in ["ir_act_window", "res_partner", "ir_model_data"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in ["ir.actions.act_window", "res.partner"] {
        reg.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "ir_model_data" ("module" varchar, "name" varchar,
           "model" varchar, "res_id" int4)"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    // an action scoped by a domain, reachable by external id
    let scoped = reg
        .create(
            &pool,
            "ir.actions.act_window",
            vec![
                ("name", json!("Parceiros")),
                ("res_model", json!("res.partner")),
                ("view_mode", json!("list,form")),
                ("domain", json!(r#"[["name", "!=", "Bob"]]"#)),
            ],
        )
        .await
        .unwrap();
    // one without a view_mode, and one whose domain is not readable
    reg.create(
        &pool,
        "ir.actions.act_window",
        vec![
            ("name", json!("Sem modo")),
            ("res_model", json!("res.partner")),
        ],
    )
    .await
    .unwrap();
    let broken = reg
        .create(
            &pool,
            "ir.actions.act_window",
            vec![
                ("name", json!("Domínio python")),
                ("res_model", json!("res.partner")),
                ("domain", json!("[('name', '=', user.name)]")),
            ],
        )
        .await
        .unwrap();
    for (name, id) in [("act_partners", scoped), ("act_broken", broken)] {
        sqlx::query(
            r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id")
               VALUES ('test', $1, 'ir.actions.act_window', $2)"#,
        )
        .bind(name)
        .bind(id as i32)
        .execute(&pool)
        .await
        .unwrap();
    }

    OrmService::insecure(Arc::new(reg), pool)
}

async fn load(app: axum::Router, action: Value) -> (StatusCode, Value) {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "call",
                      "params": {"action_id": action}});
    let response = app
        .oneshot(
            Request::post("/web/action/load")
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
async fn action_loads_by_external_id_with_its_views_and_domain_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, "rusdoo_action_by_id").await;
    let (status, body) = load(router(service.clone()), json!("test.act_partners")).await;
    assert_eq!(status, StatusCode::OK);
    let result = &body["result"];
    assert_eq!(result["res_model"], "res.partner");
    assert_eq!(result["name"], "Parceiros");
    assert_eq!(result["type"], "ir.actions.act_window");
    // view_mode becomes the [view_id, type] pairs get_views is called with
    assert_eq!(result["views"], json!([[false, "list"], [false, "form"]]));
    // the domain arrives as a domain, not as the text the record holds
    assert_eq!(result["domain"], json!([["name", "!=", "Bob"]]));

    // the same action by database id answers the same thing
    let id = result["id"].clone();
    let (_, by_id) = load(router(service), id).await;
    assert_eq!(by_id["result"]["res_model"], "res.partner");
}

#[tokio::test]
async fn an_action_without_a_view_mode_opens_list_then_form_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, "rusdoo_action_default_mode").await;
    let (_, body) = load(router(service), json!(2)).await;
    assert_eq!(body["result"]["view_mode"], "list,form");
    assert_eq!(body["result"]["domain"], json!([]));
}

#[tokio::test]
async fn an_unreadable_domain_refuses_the_action_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, "rusdoo_action_bad_domain").await;
    // a python-expression domain would silently open the model unscoped
    let (_, body) = load(router(service), json!("test.act_broken")).await;
    assert!(body.get("result").is_none(), "must not answer: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unscoped"),
        "unexpected error: {body}"
    );
}

#[tokio::test]
async fn unknown_actions_are_errors_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, "rusdoo_action_unknown").await;
    for reference in [json!("test.nope"), json!(9999), json!("semponto"), json!(true)] {
        let (_, body) = load(router(service.clone()), reference.clone()).await;
        assert!(
            body.get("result").is_none(),
            "{reference} must not resolve: {body}"
        );
    }
}
