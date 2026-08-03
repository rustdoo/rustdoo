//! rusdoo-purchase — port of `odoo/addons/purchase/models/`: the other
//! side of a sale.
//!
//! A purchase order is the mirror image of `sale.order`: the same
//! document, pointing at a supplier, producing an incoming picking and a
//! vendor bill instead of a delivery and a customer invoice. The port
//! keeps them as two models, like Odoo, because what they do next
//! differs even where their fields look the same.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_product::PRICE;
use serde_json::{json, Map, Value};

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn round(value: f64) -> Value {
    json!((value * 100.0).round() / 100.0)
}

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn price_subtotal(record: &Map<String, Value>) -> Value {
    round(number(record, "product_qty") * number(record, "price_unit"))
}

fn amount_total(record: &Map<String, Value>) -> Value {
    let lines = record
        .get("order_line.price_subtotal")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    round(
        lines
            .iter()
            .filter_map(|value| match value {
                Value::Number(n) => n.as_f64(),
                Value::String(text) => text.parse().ok(),
                _ => None,
            })
            .sum(),
    )
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(order())?;
    reg.register(order_line())?;
    Ok(())
}

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "purchase.order",
        "action_confirm",
        Operation::Write,
        action_confirm,
    )?;
    methods.register(
        "purchase.order",
        "action_cancel",
        Operation::Write,
        action_cancel,
    )?;
    methods.register(
        "purchase.order",
        "action_draft",
        Operation::Write,
        action_draft,
    )?;
    methods.register(
        "purchase.order",
        "action_create_bill",
        Operation::Write,
        action_create_bill,
    )?;
    methods.register(
        "purchase.order",
        "action_create_receipt",
        Operation::Write,
        action_create_receipt,
    )?;
    Ok(())
}

/// `purchase.order` — what we asked a supplier for.
fn order() -> Model {
    Model::new(
        meta("purchase.order", "purchase_order"),
        vec![
            char("name").required().from_sequence("purchase.order"),
            m2o("partner_id", "res.partner").required(),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new("date_order", FieldType::Datetime).default_from(defaults::NOW),
            Field::new("date_planned", FieldType::Datetime),
            char("partner_ref"),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Cotação".into()),
                    ("purchase".into(), "Pedido confirmado".into()),
                    ("cancel".into(), "Cancelado".into()),
                ]),
            )
            .default_value(json!("draft")),
            Field::new(
                "order_line",
                FieldType::One2many {
                    comodel: "purchase.order.line".into(),
                    inverse: "order_id".into(),
                },
            ),
            Field::new("amount_total", PRICE)
                .computed(&["order_line.price_subtotal"], amount_total)
                .store(),
            Field::new("notes", FieldType::Text),
        ],
    )
    // o Odoo ordena por `priority desc, id desc`; sem o campo de
    // prioridade, o que sobra da intenção é o pedido mais novo primeiro
    .ordered("id desc")
}

/// Buying zero of something is not an order, and a negative price is a
/// credit note written in the wrong place.
fn line_is_buyable(record: &Map<String, Value>) -> Result<(), String> {
    if number(record, "product_qty") <= 0.0 {
        return Err("a quantidade de uma linha precisa ser maior que zero".into());
    }
    if number(record, "price_unit") < 0.0 {
        return Err("o preço unitário não pode ser negativo".into());
    }
    Ok(())
}

/// `purchase.order.line` — one thing bought, at one price.
fn order_line() -> Model {
    Model::new(
        meta("purchase.order.line", "purchase_order_line"),
        vec![
            m2o("order_id", "purchase.order")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("product_id", "product.product"),
            char("name"),
            Field::new("product_qty", PRICE).default_value(json!(1.0)),
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new("price_subtotal", PRICE)
                .computed(&["product_qty", "price_unit"], price_subtotal)
                .store(),
        ],
    )
    .constrained(
        "linha comprável",
        &["product_qty", "price_unit"],
        line_is_buyable,
    )
}

async fn set_state(ctx: &MethodCtx<'_>, from: &[&str], to: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "a ação precisa de pelo menos um pedido".into(),
        ));
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "purchase.order", &ctx.ids, &["name", "state"])
        .await?;
    for row in &rows {
        let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
        if !from.contains(&state) {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            return Err(RusdooError::Validation(format!(
                "o pedido {name} está em {state:?} e não pode ir para {to:?}"
            )));
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "purchase.order",
            &ctx.ids,
            vec![("state", json!(to))],
        )
        .await?;
    Ok(json!(true))
}

fn action_confirm<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["draft"], "purchase").await })
}

fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["draft", "purchase"], "cancel").await })
}

fn action_draft<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["cancel"], "draft").await })
}

/// The order and its lines, once it is confirmed and not yet turned into
/// `document` — the check both bridges share.
async fn confirmed_order<'a>(
    ctx: &MethodCtx<'a>,
    document: &str,
    origin_field: &str,
) -> Result<(Map<String, Value>, Vec<Map<String, Value>>), RusdooError> {
    let [order_id] = ctx.ids[..] else {
        return Err(RusdooError::Validation(
            "trate um pedido de cada vez".into(),
        ));
    };
    let orders = ctx
        .registry
        .read(
            ctx.pool,
            "purchase.order",
            &[order_id],
            &["name", "state", "partner_id", "company_id", "order_line"],
        )
        .await?;
    let order = orders
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation(format!("pedido {order_id} não existe")))?;
    let name = order
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let state = order.get("state").and_then(Value::as_str).unwrap_or("draft");
    if state != "purchase" {
        return Err(RusdooError::Validation(format!(
            "o pedido {name} não está confirmado: confirme antes"
        )));
    }
    let existing = ctx
        .registry
        .search(
            ctx.pool,
            document,
            &parse_domain(&json!([[origin_field, "=", name]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if let Some(id) = existing.first() {
        return Err(RusdooError::Validation(format!(
            "o pedido {name} já gerou esse documento ({id})"
        )));
    }
    let line_ids: Vec<i64> = order
        .get("order_line")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if line_ids.is_empty() {
        return Err(RusdooError::Validation(format!(
            "o pedido {name} não tem linhas"
        )));
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "purchase.order.line",
            &line_ids,
            &["product_id", "name", "product_qty", "price_unit"],
        )
        .await?;
    Ok((order, lines))
}

/// `action_create_bill` — the supplier's invoice, as we expect it.
fn action_create_bill<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (order, lines) = confirmed_order(&ctx, "account.move", "invoice_origin").await?;
        let name = order.get("name").and_then(Value::as_str).unwrap_or("");
        let bill_lines: Vec<Value> = lines
            .iter()
            .map(|line| {
                json!([0, 0, {
                    "product_id": line.get("product_id").and_then(first_id),
                    "name": line.get("name").cloned().unwrap_or(Value::Null),
                    "quantity": line.get("product_qty").cloned().unwrap_or(json!(0)),
                    "price_unit": line.get("price_unit").cloned().unwrap_or(json!(0)),
                }])
            })
            .collect();
        let bill = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "account.move",
                vec![
                    // an incoming bill, not a customer invoice: the same
                    // model, told apart by its type, like in Odoo
                    ("move_type", json!("in_invoice")),
                    (
                        "partner_id",
                        json!(order.get("partner_id").and_then(first_id)),
                    ),
                    (
                        "company_id",
                        json!(order.get("company_id").and_then(first_id)),
                    ),
                    ("invoice_origin", json!(name)),
                    ("line_ids", Value::Array(bill_lines)),
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Fatura de fornecedor",
            "res_model": "account.move",
            "res_id": bill,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

/// `action_create_receipt` — what the supplier is going to hand over.
fn action_create_receipt<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (order, lines) = confirmed_order(&ctx, "stock.picking", "origin").await?;
        let name = order.get("name").and_then(Value::as_str).unwrap_or("");
        // a service is not received into a warehouse either
        let products: Vec<i64> = lines
            .iter()
            .filter_map(|line| line.get("product_id").and_then(first_id))
            .collect();
        let storable: std::collections::HashSet<i64> = if products.is_empty() {
            std::collections::HashSet::new()
        } else {
            ctx.registry
                .read(ctx.pool, "product.product", &products, &["type"])
                .await?
                .iter()
                .filter(|row| row.get("type").and_then(Value::as_str) != Some("service"))
                .filter_map(|row| row.get("id").and_then(Value::as_i64))
                .collect()
        };
        let moves: Vec<Value> = lines
            .iter()
            .filter_map(|line| {
                let product = line.get("product_id").and_then(first_id)?;
                if !storable.contains(&product) {
                    return None;
                }
                Some(json!([0, 0, {
                    "product_id": product,
                    "name": line.get("name").cloned().unwrap_or(Value::Null),
                    "product_uom_qty": line.get("product_qty").cloned().unwrap_or(json!(0)),
                }]))
            })
            .collect();
        if moves.is_empty() {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} só tem serviços: não há o que receber"
            )));
        }
        // a receipt is not numbered like a delivery: the field's own
        // sequence is the outgoing one, so this draws the incoming
        let number = ctx
            .registry
            .next_sequence(ctx.pool, "stock.picking.in")
            .await?;
        let mut values = vec![];
        if let Some(number) = number {
            values.push(("name", json!(number)));
        }
        values.extend([
                    ("picking_type", json!("incoming")),
                    (
                        "partner_id",
                        json!(order.get("partner_id").and_then(first_id)),
                    ),
                    (
                        "company_id",
                        json!(order.get("company_id").and_then(first_id)),
                    ),
                    ("origin", json!(name)),
                    ("move_ids", Value::Array(moves)),
        ]);
        let picking = ctx
            .registry
            .create_as(ctx.pool, ctx.uid, "stock.picking", values)
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Recebimento",
            "res_model": "stock.picking",
            "res_id": picking,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_worth_quantity_times_price() {
        let mut record = Map::new();
        record.insert("product_qty".into(), json!(4));
        record.insert("price_unit".into(), json!(12.5));
        assert_eq!(price_subtotal(&record), json!(50.0));
    }

    #[test]
    fn the_models_register_on_top_of_what_they_need() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        rusdoo_stock::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        for name in ["purchase.order", "purchase.order.line"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
    }

    #[test]
    fn an_order_carries_its_five_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("purchase.order"),
            vec![
                "action_cancel",
                "action_confirm",
                "action_create_bill",
                "action_create_receipt",
                "action_draft"
            ]
        );
    }
}
