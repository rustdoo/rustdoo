//! A product is two records, as in Odoo: the template the catalogue
//! describes and the variant a warehouse counts. Reference:
//! `odoo/addons/product/models/product_template.py` +
//! `product_product.py` (`_inherits = {'product.template': 'product_tmpl_id'}`).

use rusdoo_http::dispatch::OrmService;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 2] = ["base", "product"];

async fn call(service: &OrmService, model: &str, method: &str, args: Value) -> Value {
    service
        .call_kw(1, model, method, args.as_array().unwrap(), &Default::default())
        .await
        .unwrap_or_else(|e| panic!("{model}.{method}: {}", e.message))
}

/// The whole point of the delegation: a caller that only knows about
/// variants still reads and writes one value, not two that drift apart.
#[tokio::test]
async fn a_product_written_through_the_variant_is_one_value_live() {
    let Some(case) = TransactionCase::open("product_tmpl", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // created through the variant, as every caller does it
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Cadeira", "list_price": 250.0, "standard_price": 90.0}]),
    )
    .await;
    let id = id.as_i64().expect("an id");

    // the name and the price come back through the variant...
    let read = call(
        &service,
        "product.product",
        "read",
        json!([[id], ["name", "list_price", "standard_price", "product_tmpl_id"]]),
    )
    .await;
    let row = &read[0];
    assert_eq!(row["name"], json!("Cadeira"));
    assert_eq!(row["list_price"], json!(250.0));
    assert_eq!(row["standard_price"], json!(90.0));

    // ...and the template is a record of its own, holding the name
    let tmpl = row["product_tmpl_id"][0].as_i64().expect("a template link");
    let read = call(
        &service,
        "product.template",
        "read",
        json!([[tmpl], ["name", "list_price"]]),
    )
    .await;
    assert_eq!(read[0]["name"], json!("Cadeira"));
    assert_eq!(read[0]["list_price"], json!(250.0));

    // writing the name through the variant writes the template's row:
    // one value, not two that drift apart
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Cadeira de escritório"}]),
    )
    .await;
    let read = call(
        &service,
        "product.template",
        "read",
        json!([[tmpl], ["name"]]),
    )
    .await;
    assert_eq!(read[0]["name"], json!("Cadeira de escritório"));
}

#[tokio::test]
async fn the_cost_is_the_variants_own_live() {
    let Some(case) = TransactionCase::open("product_cost", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // one template, two variants: what they share is priced once, what
    // they do not is priced apart
    let first = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Camiseta", "list_price": 60.0, "standard_price": 20.0}]),
    )
    .await;
    let first = first.as_i64().unwrap();
    let tmpl = call(
        &service,
        "product.product",
        "read",
        json!([[first], ["product_tmpl_id"]]),
    )
    .await[0]["product_tmpl_id"][0]
        .as_i64()
        .unwrap();

    let second = call(
        &service,
        "product.product",
        "create",
        json!([{"product_tmpl_id": tmpl, "standard_price": 35.0, "default_code": "CAM-G"}]),
    )
    .await;
    let second = second.as_i64().unwrap();

    let read = call(
        &service,
        "product.product",
        "read",
        json!([[first, second], ["name", "list_price", "standard_price"]]),
    )
    .await;
    let rows = read.as_array().unwrap();
    // the name and the sales price are the template's, so both variants
    // answer the same
    for row in rows {
        assert_eq!(row["name"], json!("Camiseta"), "{row}");
        assert_eq!(row["list_price"], json!(60.0), "{row}");
    }
    // the cost is not
    let costs: Vec<f64> = rows
        .iter()
        .map(|r| r["standard_price"].as_f64().unwrap())
        .collect();
    assert!(costs.contains(&20.0) && costs.contains(&35.0), "{costs:?}");
}

#[tokio::test]
async fn a_negative_price_is_refused_on_either_record_live() {
    let Some(case) = TransactionCase::open("product_neg", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // the sales price belongs to the template and is checked there
    let refused = service
        .call_kw(
            1,
            "product.template",
            "create",
            &[json!({"name": "Errado", "list_price": -1.0})],
            &Default::default(),
        )
        .await;
    assert!(refused.is_err(), "a negative sales price was accepted");

    // the cost belongs to the variant and is checked there
    let refused = service
        .call_kw(
            1,
            "product.product",
            "create",
            &[json!({"name": "Errado", "standard_price": -1.0})],
            &Default::default(),
        )
        .await;
    assert!(refused.is_err(), "a negative cost was accepted");
}
