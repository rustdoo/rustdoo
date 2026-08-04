//! The unit a document line is written in, and how far it travels.
//!
//! Odoo's `product_uom_id` on a line is a stored computed field with
//! `readonly=False`: it follows the product until somebody sells by the
//! dozen, and then the dozen has to survive the save, the delivery and
//! the invoice. This is the test of that whole sentence.

use rusdoo_http::dispatch::OrmService;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 7] = ["base", "mail", "uom", "product", "account", "stock", "sale"];

fn service(case: &TransactionCase) -> OrmService {
    OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods())
}

async fn call(service: &OrmService, model: &str, method: &str, args: Value) -> Value {
    service
        .call_kw(1, model, method, args.as_array().unwrap(), &Default::default())
        .await
        .unwrap_or_else(|error| panic!("{model}.{method}: {}", error.message))
}

async fn create(service: &OrmService, model: &str, values: Value) -> i64 {
    call(service, model, "create", json!([values]))
        .await
        .as_i64()
        .expect("an id")
}

/// The `[id, display_name]` a many2one reads as.
fn link(row: &Value, field: &str) -> i64 {
    row[field][0]
        .as_i64()
        .unwrap_or_else(|| panic!("{field} holds no link: {row}"))
}

#[tokio::test]
async fn a_line_is_written_in_the_products_unit_unless_it_says_otherwise_live() {
    let Some(case) = TransactionCase::open("line_uom", &MODULES).await else {
        return;
    };
    let service = service(&case);

    let unit = create(&service, "uom.uom", json!({"name": "Unidades"})).await;
    let dozen = create(
        &service,
        "uom.uom",
        json!({"name": "Dúzias", "relative_factor": 12.0, "relative_uom_id": unit}),
    )
    .await;
    let product = create(
        &service,
        "product.product",
        json!({"name": "Caneta", "list_price": 3.0, "uom_id": unit}),
    )
    .await;
    let partner = create(&service, "res.partner", json!({"name": "Ana"})).await;

    // one line says nothing about units and gets the product's; the
    // other sells by the dozen and keeps it
    let order = create(
        &service,
        "sale.order",
        json!({
            "partner_id": partner,
            "order_line": [
                [0, 0, {"product_id": product, "name": "Caneta", "product_uom_qty": 10.0,
                        "price_unit": 3.0}],
                [0, 0, {"product_id": product, "name": "Caneta (caixa)", "product_uom_qty": 2.0,
                        "price_unit": 33.0, "product_uom_id": dozen}],
            ],
        }),
    )
    .await;

    let lines = call(
        &service,
        "sale.order",
        "read",
        json!([[order], ["order_line"]]),
    )
    .await[0]["order_line"]
        .as_array()
        .expect("the lines")
        .iter()
        .filter_map(Value::as_i64)
        .collect::<Vec<_>>();
    let rows = call(
        &service,
        "sale.order.line",
        "read",
        json!([lines, ["name", "product_uom_id"]]),
    )
    .await;
    let plain = &rows[0];
    let by_the_dozen = &rows[1];
    assert_eq!(link(plain, "product_uom_id"), unit, "{plain}");
    assert_eq!(
        by_the_dozen["product_uom_id"][1], "Dúzias",
        "the unit written on the line survived the save: {by_the_dozen}"
    );

    // the delivery ships what was ordered, in the unit it was ordered in
    call(&service, "sale.order", "action_confirm", json!([[order]])).await;
    let action = call(
        &service,
        "sale.order",
        "action_create_delivery",
        json!([[order]]),
    )
    .await;
    let picking = action["res_id"].as_i64().expect("a picking");
    let moves = call(
        &service,
        "stock.picking",
        "read",
        json!([[picking], ["move_ids"]]),
    )
    .await[0]["move_ids"]
        .as_array()
        .expect("moves")
        .iter()
        .filter_map(Value::as_i64)
        .collect::<Vec<_>>();
    let rows = call(
        &service,
        "stock.move",
        "read",
        json!([moves, ["name", "product_uom_qty", "product_uom_id"]]),
    )
    .await;
    let shipped: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["product_uom_id"][1].as_str().unwrap_or("—"))
        .collect();
    assert!(shipped.contains(&"Dúzias"), "{rows}");
    assert!(shipped.contains(&"Unidades"), "{rows}");

    // and so does the invoice
    let action = call(
        &service,
        "sale.order",
        "action_create_invoice",
        json!([[order]]),
    )
    .await;
    let invoice = action["res_id"].as_i64().expect("an invoice");
    let invoice_lines = call(
        &service,
        "account.move",
        "read",
        json!([[invoice], ["line_ids"]]),
    )
    .await[0]["line_ids"]
        .as_array()
        .expect("lines")
        .iter()
        .filter_map(Value::as_i64)
        .collect::<Vec<_>>();
    let rows = call(
        &service,
        "account.move.line",
        "read",
        json!([invoice_lines, ["product_uom_id"]]),
    )
    .await;
    let billed: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["product_uom_id"][1].as_str().unwrap_or("—"))
        .collect();
    assert!(billed.contains(&"Dúzias"), "{rows}");

    case.close().await;
}

/// The other half of `readonly=False`: the override stands until a
/// dependency moves, and then the compute answers again. Odoo behaves
/// exactly this way, and a line that kept a stale unit after its product
/// changed would be worse than one that never had an override at all.
#[tokio::test]
async fn changing_the_product_computes_the_unit_again_live() {
    let Some(case) = TransactionCase::open("line_uom_recompute", &MODULES).await else {
        return;
    };
    let service = service(&case);

    let unit = create(&service, "uom.uom", json!({"name": "Unidades"})).await;
    let hour = create(&service, "uom.uom", json!({"name": "Horas"})).await;
    let dozen = create(
        &service,
        "uom.uom",
        json!({"name": "Dúzias", "relative_factor": 12.0, "relative_uom_id": unit}),
    )
    .await;
    let pen = create(
        &service,
        "product.product",
        json!({"name": "Caneta", "uom_id": unit}),
    )
    .await;
    let service_product = create(
        &service,
        "product.product",
        json!({"name": "Consultoria", "type": "service", "uom_id": hour}),
    )
    .await;
    let partner = create(&service, "res.partner", json!({"name": "Ana"})).await;
    let order = create(
        &service,
        "sale.order",
        json!({"partner_id": partner, "order_line": [
            [0, 0, {"product_id": pen, "name": "Caneta", "product_uom_qty": 1.0,
                    "price_unit": 3.0, "product_uom_id": dozen}],
        ]}),
    )
    .await;
    let line = call(
        &service,
        "sale.order",
        "read",
        json!([[order], ["order_line"]]),
    )
    .await[0]["order_line"][0]
        .as_i64()
        .expect("the line");

    let row = &call(
        &service,
        "sale.order.line",
        "read",
        json!([[line], ["product_uom_id"]]),
    )
    .await[0];
    assert_eq!(link(row, "product_uom_id"), dozen, "{row}");

    // the line now sells something else, and dozens of it mean nothing
    call(
        &service,
        "sale.order.line",
        "write",
        json!([[line], {"product_id": service_product}]),
    )
    .await;
    let row = &call(
        &service,
        "sale.order.line",
        "read",
        json!([[line], ["product_uom_id"]]),
    )
    .await[0];
    assert_eq!(
        link(row, "product_uom_id"),
        hour,
        "the product moved, so the compute answers again: {row}"
    );

    case.close().await;
}
