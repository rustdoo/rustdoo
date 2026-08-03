//! A module grafts a field onto another's form without republishing it:
//! `get_views` answers the base arch with the patches applied.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The form the module that owns the model publishes.
const BASE: &str = r#"<form><button name="action_post" type="object" string="Lançar"/><group><field name="name"/><field name="partner_id"/></group></form>"#;

async fn call(service: OrmService, body: Value) -> Value {
    let response = router(service)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/web/dataset/call_kw")
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

fn get_views(model: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "method": "call", "id": 1,
        "params": {
            "model": model, "method": "get_views", "args": [], "kwargs": {
                "views": [[null, "form"]]
            }
        }
    })
}

#[tokio::test]
async fn a_patch_is_applied_to_the_view_it_extends_live() {
    let Some(case) = TransactionCase::open("view_inheritance", &["base"]).await else {
        return;
    };
    let pool = case.pool();
    let base = case
        .models()
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("res.partner.form")),
                ("model", json!("res.partner")),
                ("type", json!("form")),
                ("arch", json!(BASE)),
            ],
        )
        .await
        .unwrap();
    // the patch: a button after the one already there, a field after
    // `name`
    case.models()
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("res.partner.form.extra")),
                ("model", json!("res.partner")),
                ("type", json!("form")),
                ("inherit_id", json!(base)),
                (
                    "arch",
                    json!(
                        r#"<data><xpath expr="//button[@name='action_post']" position="after"><button name="action_extra" type="object" string="Extra"/></xpath><field name="name" position="after"><field name="ref"/></field></data>"#
                    ),
                ),
            ],
        )
        .await
        .unwrap();

    let service = OrmService::insecure(case.registry(), pool.clone());
    let response = call(service, get_views("res.partner")).await;
    let arch = response["result"]["views"]["form"]["arch"]
        .as_str()
        .unwrap_or_else(|| panic!("no arch: {response}"))
        .to_string();

    assert!(
        arch.contains(r#"<button name="action_extra""#),
        "the patch's button went in: {arch}"
    );
    assert!(
        arch.contains(r#"<field name="ref"/>"#),
        "the patch's field went in: {arch}"
    );
    // and in the position asked for, not at the end
    let extra = arch.find("action_extra").unwrap();
    let group = arch.find("<group>").unwrap();
    assert!(extra < group, "the button landed before the group: {arch}");
    assert!(
        arch.find(r#"name="ref""#).unwrap() > arch.find(r#"name="name""#).unwrap(),
        "the field landed after `name`: {arch}"
    );

    case.close().await;
}

#[tokio::test]
async fn a_patch_is_never_served_as_a_view_of_its_own_live() {
    let Some(case) = TransactionCase::open("view_inheritance_pick", &["base"]).await else {
        return;
    };
    let pool = case.pool();
    // the patch goes in first and with a lower priority: if the default
    // form lookup did not exclude patches, it is what the client would
    // get
    let patch = case
        .models()
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("res.users.form.extra")),
                ("model", json!("res.users")),
                ("type", json!("form")),
                ("priority", json!(1)),
                ("arch", json!(r#"<data><field name="login" position="after"><field name="active"/></field></data>"#)),
            ],
        )
        .await
        .unwrap();
    let base = case
        .models()
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("res.users.form")),
                ("model", json!("res.users")),
                ("type", json!("form")),
                ("priority", json!(16)),
                ("arch", json!(r#"<form><field name="login"/></form>"#)),
            ],
        )
        .await
        .unwrap();
    case.models()
        .write(&pool, "ir.ui.view", &[patch], vec![("inherit_id", json!(base))])
        .await
        .unwrap();

    let service = OrmService::insecure(case.registry(), pool.clone());
    let response = call(service, get_views("res.users")).await;
    let view = &response["result"]["views"]["form"];
    assert_eq!(
        view["id"],
        json!(base),
        "the client gets the view, not the patch: {response}"
    );
    let arch = view["arch"].as_str().unwrap();
    assert!(arch.starts_with("<form>"), "the arch is the base's: {arch}");
    assert!(arch.contains(r#"<field name="active"/>"#), "with the patch: {arch}");

    case.close().await;
}
