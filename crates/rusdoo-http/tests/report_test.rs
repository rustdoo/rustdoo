//! Printing a record: the QWeb document a report renders, and what it
//! is allowed to read on the way.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use serde_json::{json, Value};
use std::collections::HashMap;
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
        "product_template",
        "product_category",
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
        // the company default reads the creating user
        "res.groups",
        "res.users",
        "product.category",
        "product.template",
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

/// A converter that is not one, so the route can be proved without a
/// binary on the machine — and so the test says what the *route* does
/// rather than what somebody's chromium does.
struct FakePdf;

impl rusdoo_http::pdf::PdfRenderer for FakePdf {
    fn name(&self) -> &str {
        "fake"
    }

    fn render(&self, html: &str) -> Result<Vec<u8>, rusdoo_core::RusdooError> {
        // enough of a PDF to be one, carrying the length of what it was
        // given: the test can then tell "the report reached the
        // converter" from "something reached the converter"
        Ok(format!("%PDF-1.4 fake of {} bytes", html.len()).into_bytes())
    }
}

async fn fetch(app: axum::Router, uri: &str) -> (StatusCode, HashMap<String, String>, Vec<u8>) {
    let response = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// The same document as `/report/html/`, converted and served as a file.
#[tokio::test]
async fn a_report_is_served_as_a_pdf_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, order) = fixture(&url, rusdoo_testing::schema_for("rusdoo_report_pdf")).await;
    let printing = service.clone().with_pdf(Arc::new(FakePdf));

    let (status, headers, body) = fetch(
        router(printing.clone()),
        &format!("/report/pdf/test.doc_report/{order}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(
        headers["content-disposition"],
        format!("inline; filename=\"test.doc_report-{order}.pdf\""),
        "opened rather than downloaded, and named after what it is"
    );
    assert_eq!(
        headers["cache-control"], "no-store",
        "a printed document is what the database says now"
    );
    assert!(
        body.starts_with(b"%PDF"),
        "the bytes are the converter's, not the HTML: {:?}",
        String::from_utf8_lossy(&body[..20.min(body.len())])
    );
    // and what reached the converter was the rendered report, not an
    // empty page
    let rendered = printing
        .render_report("test.doc_report", order, None)
        .await
        .expect("the document renders");
    assert!(
        String::from_utf8_lossy(&body).contains(&format!("{} bytes", rendered.len())),
        "the converter was handed the report: {}",
        String::from_utf8_lossy(&body)
    );

    // an unknown report is refused here the same way it is in html, and
    // still without saying which of the two was wrong
    let (status, _, _) = fetch(
        router(printing),
        &format!("/report/pdf/test.nope/{order}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Without a converter the route refuses and says so, and the HTML the
/// same report renders is still served.
#[tokio::test]
async fn without_a_converter_pdf_refuses_and_html_still_serves_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (service, order) = fixture(&url, rusdoo_testing::schema_for("rusdoo_report_nopdf")).await;

    let (status, headers, body) = fetch(
        router(service.clone()),
        &format!("/report/pdf/test.doc_report/{order}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a server with no converter says so"
    );
    assert!(
        !headers
            .get("content-type")
            .is_some_and(|kind| kind.contains("application/pdf")),
        "and never labels its refusal a PDF: {headers:?}"
    );
    let message = String::from_utf8_lossy(&body);
    assert!(
        message.contains("RUSDOO_PDF_BIN"),
        "the message names the way out: {message}"
    );

    // nothing was lost but the file extension
    let (status, page) = get(
        router(service),
        &format!("/report/html/test.doc_report/{order}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Pedido SO00001"), "{page}");
}
