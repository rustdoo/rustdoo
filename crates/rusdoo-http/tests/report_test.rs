//! Printing a record: the QWeb document a report renders, and what it
//! is allowed to read on the way.

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

/// The document template, as an addon would ship it.
const TEMPLATE: &str = r#"<div><h1>Pedido <t t-out="doc.name"/></h1>
<p>Cliente: <t t-out="doc.partner_id"/> — <t t-out="doc.state"/></p>
<table><tr t-foreach="doc.order_line" t-as="line"><td t-out="line.name"/><td t-out="line.price_subtotal"/></tr></table>
<p>Total: <t t-out="doc.amount_total"/></p></div>"#;

async fn fixture(url: &str, schema: &'static str) -> (OrmService, i64) {
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
        "res_users",
        "res_groups",
        "res_country",
        "ir_sequence",
        "ir_ui_view",
        "ir_act_report",
        "ir_model_data",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "ir.sequence",
        "ir.ui.view",
        "ir.actions.report",
        "res.country",
        "res.company",
        "res.partner",
        // o default de empresa lê o usuário que está criando
        "res.groups",
        "res.users",
        "product.product",
        "sale.order",
        "sale.order.line",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "ir_model_data" ("module" varchar, "name" varchar,
           "model" varchar, "res_id" int4)"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("Pedido")),
                ("code", json!("sale.order")),
                ("prefix", json!("SO")),
                ("padding", json!(5)),
            ],
        )
        .await
        .unwrap();

    // the template and the report that names it
    let view = registry
        .create(
            &pool,
            "ir.ui.view",
            vec![
                ("name", json!("sale.order.report")),
                ("model", json!("sale.order")),
                ("type", json!("qweb")),
                ("arch", json!(TEMPLATE)),
            ],
        )
        .await
        .unwrap();
    let report = registry
        .create(
            &pool,
            "ir.actions.report",
            vec![
                ("name", json!("Pedido de venda")),
                ("model", json!("sale.order")),
                ("report_name", json!("test.doc_template")),
            ],
        )
        .await
        .unwrap();
    for (name, model, res_id) in [
        ("doc_template", "ir.ui.view", view),
        ("doc_report", "ir.actions.report", report),
    ] {
        sqlx::query(
            r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id")
               VALUES ('test', $1, $2, $3)"#,
        )
        .bind(name)
        .bind(model)
        .bind(res_id as i32)
        .execute(&pool)
        .await
        .unwrap();
    }

    // an order to print
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let product = registry
        .create(
            &pool,
            "product.product",
            vec![("name", json!("Mesa")), ("list_price", json!(1250))],
        )
        .await
        .unwrap();
    let order = registry
        .create(
            &pool,
            "sale.order",
            vec![
                ("partner_id", json!(partner)),
                (
                    "order_line",
                    json!([
                        [0, 0, {"product_id": product, "name": "Mesa", "product_uom_qty": 2, "price_unit": 1250}],
                        [0, 0, {"name": "Montagem", "product_uom_qty": 1, "price_unit": 300}],
                    ]),
                ),
            ],
        )
        .await
        .unwrap();
    (
        OrmService::insecure(Arc::new(registry), pool),
        order,
    )
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_report_prints_the_record_and_its_lines_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, order) = fixture(&url, rusdoo_testing::schema_for("rusdoo_report_test")).await;
    // render directly first, so a failure says why instead of arriving
    // as the route's generic error page
    let rendered = service
        .render_report("test.doc_report", order, None)
        .await
        .expect("o documento renderiza");
    assert!(rendered.contains("Pedido SO00001"), "{rendered}");

    let (status, html) = get(
        router(service),
        &format!("/report/html/test.doc_report/{order}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Pedido SO00001"), "{html}");
    // a many2one prints the record's name, not its id
    assert!(html.contains("Ana"), "{html}");
    // a selection prints its label: a document that says `draft` is not
    // a document anyone sends to a customer
    assert!(html.contains("Quotation"), "{html}");
    assert!(!html.contains(">draft<"), "{html}");
    // both lines, with money at its precision
    assert!(html.contains("Mesa"), "{html}");
    assert!(html.contains("Montagem"), "{html}");
    assert!(html.contains("2500.00"), "{html}");
    assert!(html.contains("2800.00"), "the total: {html}");
    assert!(!html.contains("2500.0<"), "no bare float: {html}");
}

#[tokio::test]
async fn an_unknown_report_or_record_does_not_leak_why_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, order) = fixture(&url, rusdoo_testing::schema_for("rusdoo_report_missing_test")).await;
    for uri in [
        format!("/report/html/test.nope/{order}"),
        format!("/report/html/test.doc_report/{}", order + 5000),
        "/report/html/semponto/1".to_string(),
    ] {
        let (status, html) = get(router(service.clone()), &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        // the page says it failed without echoing the request back
        assert!(!html.contains("nope"), "{uri}: {html}");
        assert!(html.contains("could not"), "{uri}: {html}");
    }
}

#[tokio::test]
async fn a_report_is_never_cached_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, order) = fixture(&url, rusdoo_testing::schema_for("rusdoo_report_cache_test")).await;
    let response = router(service)
        .oneshot(
            Request::get(format!("/report/html/test.doc_report/{order}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        cache, "no-store",
        "a printed document is what the database says now"
    );
    let _: Value = json!(null);
}
