//! The opportunity and the quotation against the database: the button
//! that makes the first one, and the numbers that stop being guesses
//! once there are real orders behind them.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 10] = [
    "base",
    "mail",
    "utm",
    "sales_team",
    "crm",
    "product",
    "account",
    "stock",
    "sale",
    "sale_crm",
];

async fn ask(
    case: &TransactionCase,
    model: &str,
    ids: &[i64],
    method: &str,
) -> Result<Value, String> {
    let methods = case.methods();
    let entry = methods
        .get(model, method)
        .unwrap_or_else(|| panic!("{model}.{method} is not registered"));
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, model, ids.to_vec());
    entry
        .call(ctx, &[], &Map::new())
        .await
        .map_err(|error| error.to_string())
}

async fn create(case: &TransactionCase, model: &str, values: Vec<(&str, Value)>) -> i64 {
    case.models()
        .create(&case.pool(), model, values)
        .await
        .unwrap_or_else(|error| panic!("{model} saves: {error}"))
}

#[tokio::test]
async fn an_opportunity_makes_its_first_quotation_live() {
    let Some(case) = TransactionCase::open("sale_crm_quote", &MODULES).await else {
        return;
    };
    let partner = create(&case, "res.partner", vec![("name", json!("Loja do Bairro"))]).await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Reforma da loja")),
            ("type", json!("opportunity")),
            ("partner_id", json!(partner)),
        ],
    )
    .await;

    let action = ask(&case, "crm.lead", &[lead], "action_new_quotation")
        .await
        .expect("quoting works");
    // the client is told where the quotation went, instead of having to
    // guess
    assert_eq!(action["res_model"], json!("sale.order"), "{action}");
    let order = action["res_id"].as_i64().expect("an order id");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "sale.order",
            &[order],
            &["partner_id", "opportunity_id"],
        )
        .await
        .expect("the order reads");
    let row = &rows[0];
    assert_eq!(row["partner_id"][0], json!(partner), "{row:?}");
    assert_eq!(row["opportunity_id"][0], json!(lead), "{row:?}");

    // and the opportunity knows about it, from the other side
    let rows = case
        .models()
        .read(&case.pool(), "crm.lead", &[lead], &["order_ids"])
        .await
        .expect("the lead reads");
    assert_eq!(
        rows[0]["order_ids"].as_array().map(Vec::len),
        Some(1),
        "{:?}",
        rows[0]
    );

    case.close().await;
}

#[tokio::test]
async fn what_the_pipeline_is_worth_counts_only_signed_orders_live() {
    let Some(case) = TransactionCase::open("sale_crm_amounts", &MODULES).await else {
        return;
    };
    let partner = create(&case, "res.partner", vec![("name", json!("Cliente"))]).await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Contrato")),
            ("type", json!("opportunity")),
            ("partner_id", json!(partner)),
        ],
    )
    .await;

    // one quotation nobody signed, one order that was confirmed
    for (state, price) in [("draft", 100.0), ("sale", 250.0)] {
        create(
            &case,
            "sale.order",
            vec![
                ("partner_id", json!(partner)),
                ("opportunity_id", json!(lead)),
                ("state", json!(state)),
                (
                    "order_line",
                    json!([[0, 0, {"name": "Serviço", "product_uom_qty": 1.0,
                                   "price_unit": price}]]),
                ),
            ],
        )
        .await;
    }

    let rows = case
        .models()
        .read(
            &case.pool(),
            "crm.lead",
            &[lead],
            &["sale_amount_total", "quotation_count", "sale_order_count"],
        )
        .await
        .expect("the lead reads");
    let row = &rows[0];
    assert_eq!(
        row["sale_amount_total"],
        json!(250.0),
        "só o pedido assinado conta: {row:?}"
    );
    assert_eq!(row["quotation_count"], json!(1), "{row:?}");
    assert_eq!(row["sale_order_count"], json!(1), "{row:?}");

    case.close().await;
}

#[tokio::test]
async fn a_lead_without_a_customer_is_not_quoted_live() {
    let Some(case) = TransactionCase::open("sale_crm_refusals", &MODULES).await else {
        return;
    };

    // still a lead: it is quoted after somebody decides it is worth
    // working, not before
    let scrap = create(
        &case,
        "crm.lead",
        vec![("name", json!("Cartão de visita"))],
    )
    .await;
    let refused = ask(&case, "crm.lead", &[scrap], "action_new_quotation").await;
    assert!(refused.is_err(), "cotou um lead: {refused:?}");

    // an opportunity with nobody to bill: Odoo opens a dialog to pick or
    // create the contact, and refusing says the same thing without
    // inventing one
    let nameless = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Sem cliente")),
            ("type", json!("opportunity")),
        ],
    )
    .await;
    let refused = ask(&case, "crm.lead", &[nameless], "action_new_quotation").await;
    let message = refused.expect_err("cotou sem cliente");
    assert!(message.contains("partner_id"), "{message}");

    case.close().await;
}

#[tokio::test]
async fn deleting_an_opportunity_leaves_the_orders_it_produced_live() {
    let Some(case) = TransactionCase::open("sale_crm_unlink", &MODULES).await else {
        return;
    };
    let partner = create(&case, "res.partner", vec![("name", json!("Cliente"))]).await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Some depois")),
            ("type", json!("opportunity")),
            ("partner_id", json!(partner)),
        ],
    )
    .await;
    let action = ask(&case, "crm.lead", &[lead], "action_new_quotation")
        .await
        .expect("quoting works");
    let order = action["res_id"].as_i64().expect("an order id");

    case.models()
        .unlink_as(&case.pool(), 1, "crm.lead", &[lead])
        .await
        .expect("the opportunity is deleted");

    // the order is what the company invoiced: it does not go with the
    // opportunity, it only loses the link
    let rows = case
        .models()
        .read(&case.pool(), "sale.order", &[order], &["opportunity_id"])
        .await
        .expect("the order still reads");
    assert!(rows[0]["opportunity_id"].is_null(), "{:?}", rows[0]);

    case.close().await;
}
