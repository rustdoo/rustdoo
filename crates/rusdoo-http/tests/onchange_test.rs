//! `@api.onchange` over the wire: what the form view calls on every
//! edit, answered by the Python an addon wrote.
//!
//! Through the RPC endpoint and not against the model directly, because
//! the shape of the call is half the feature: the client sends what the
//! *form* holds, not what the database holds, and a record that was
//! never saved has to get an answer all the same.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const ADDON: &str = r#"
from odoo import models, fields, api


class Quote(models.Model):
    _name = "onchange.quote"

    name = fields.Char()
    quantity = fields.Integer(default=1)
    unit_price = fields.Float()
    total = fields.Float()
    note = fields.Text()

    @api.onchange("quantity", "unit_price")
    def _onchange_total(self):
        self.total = self.quantity * self.unit_price

    @api.onchange("total")
    def _onchange_note(self):
        # runs on the total the one above just set, not on the stale one
        # the form sent
        if self.total > 100:
            self.note = "needs approval"

    @api.onchange("name")
    def _onchange_name(self):
        self.name = (self.name or "").strip().upper()
"#;

async fn onchange(service: OrmService, values: Value, edited: Value) -> Value {
    let response = router(service)
        .oneshot(
            Request::post("/web/dataset/call_kw")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "call",
                        "params": {
                            "model": "onchange.quote",
                            "method": "onchange",
                            "args": [[], values, edited, {}],
                            "kwargs": {}
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_python_onchange_answers_the_form() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut registry = Registry::new();
    rusdoo_python::load_python_models(&mut registry, "onchange_addon", ADDON)
        .expect("the addon loads");
    let service = OrmService::insecure(
        Arc::new(registry),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    );

    // editing a quantity recomputes the total, and the second onchange
    // sees the total the first one set rather than the stale one the
    // form sent
    let answer = onchange(
        service.clone(),
        json!({"quantity": 12, "unit_price": 10.0, "total": 0.0}),
        json!(["quantity"]),
    )
    .await;
    assert_eq!(
        answer["result"]["value"],
        json!({"total": 120.0, "note": "needs approval"}),
        "{answer}"
    );

    // an edit no onchange watches changes nothing — the endpoint still
    // answers, because a form whose onchange errors is a form stuck
    let answer = onchange(
        service.clone(),
        json!({"quantity": 12, "unit_price": 10.0, "note": "hand-written"}),
        json!(["note"]),
    )
    .await;
    assert_eq!(answer["result"]["value"], json!({}), "{answer}");

    // and a field the form never filled in reads as empty rather than
    // failing: an unsaved record is partial by nature
    let answer = onchange(
        service.clone(),
        json!({"name": "  acme  "}),
        json!(["name"]),
    )
    .await;
    assert_eq!(answer["result"]["value"], json!({"name": "ACME"}), "{answer}");
    let answer = onchange(service, json!({"unit_price": 3.5}), json!(["unit_price"])).await;
    assert_eq!(
        answer["result"]["value"],
        json!({"total": 0.0}),
        "an empty quantity counted as nothing: {answer}"
    );
}
