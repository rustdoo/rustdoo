//! rusdoo-sale — port of `odoo/addons/sale/models/` (and the slice of
//! `product` it needs): the first business module of the port.
//!
//! It is here to be a real one, not a demo: an order with lines, line
//! subtotals and an order total that the database keeps current — the
//! shape every other business module repeats.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Money and quantities: two decimals, like Odoo's default precision.
const PRICE: FieldType = rusdoo_product::PRICE;

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

/// A numeric field's value, whatever shape the driver decoded it in.
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

/// `price_subtotal` — what one line is worth.
fn price_subtotal(record: &Map<String, Value>) -> Value {
    let total = number(record, "product_uom_qty") * number(record, "price_unit");
    // rounded to the currency's precision, not left with the binary tail
    // of the multiplication
    json!((total * 100.0).round() / 100.0)
}

/// `amount_total` — the order is worth what its lines are worth.
fn amount_total(record: &Map<String, Value>) -> Value {
    let lines = record
        .get("order_line.price_subtotal")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total: f64 = lines
        .iter()
        .filter_map(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .sum();
    json!((total * 100.0).round() / 100.0)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    for model in models() {
        reg.register(model)?;
    }
    Ok(())
}

/// The buttons of a sales order, as methods a client may call.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register("sale.order", "action_confirm", Operation::Write, action_confirm)?;
    methods.register("sale.order", "action_cancel", Operation::Write, action_cancel)?;
    methods.register("sale.order", "action_draft", Operation::Write, action_draft)?;
    methods.register(
        "sale.order",
        "action_create_invoice",
        Operation::Write,
        action_create_invoice,
    )?;
    methods.register(
        "sale.order",
        "action_create_delivery",
        Operation::Write,
        action_create_delivery,
    )?;
    Ok(())
}

/// `action_create_delivery` — ship what was sold.
///
/// Odoo splits this into a bridge module (`sale_stock`), because a sale
/// does not have to know about a warehouse. The port keeps the two
/// models apart and the bridge here, where the order already is: only
/// storable lines travel, since a service is not delivered by anyone.
fn action_create_delivery<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [order_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "entregue um pedido de cada vez".into(),
            ));
        };
        let orders = ctx
            .registry
            .read(
                ctx.pool,
                "sale.order",
                &[order_id],
                &["name", "state", "partner_id", "company_id", "order_line"],
            )
            .await?;
        let order = orders
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("pedido {order_id} não existe")))?;
        let name = order.get("name").and_then(Value::as_str).unwrap_or("");
        let state = order.get("state").and_then(Value::as_str).unwrap_or("draft");
        if state != "sale" {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} não está confirmado: confirme antes de entregar"
            )));
        }
        let existing = ctx
            .registry
            .search(
                ctx.pool,
                "stock.picking",
                &parse_domain(&json!([["origin", "=", name]]))?,
                &SearchOptions::default(),
            )
            .await?;
        if let Some(picking) = existing.first() {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} já tem entrega (documento {picking})"
            )));
        }

        let line_ids: Vec<i64> = order
            .get("order_line")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let sold = if line_ids.is_empty() {
            Vec::new()
        } else {
            ctx.registry
                .read(
                    ctx.pool,
                    "sale.order.line",
                    &line_ids,
                    &["product_id", "name", "product_uom_qty"],
                )
                .await?
        };
        // a service has nothing to put in a box: only storable products
        // travel, which means asking the catalogue what each line sells
        let products: Vec<i64> = sold
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
        let moves: Vec<Value> = sold
            .iter()
            .filter_map(|line| {
                let product = line.get("product_id").and_then(first_id)?;
                if !storable.contains(&product) {
                    return None;
                }
                Some(json!([0, 0, {
                    "product_id": product,
                    "name": line.get("name").cloned().unwrap_or(Value::Null),
                    "product_uom_qty": line
                        .get("product_uom_qty")
                        .cloned()
                        .unwrap_or(json!(0)),
                }]))
            })
            .collect();
        if moves.is_empty() {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} só tem serviços: não há o que entregar"
            )));
        }

        let picking = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "stock.picking",
                vec![
                    ("picking_type", json!("outgoing")),
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
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Entrega",
            "res_model": "stock.picking",
            "res_id": picking,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

/// `action_create_invoice` — bill a confirmed order.
///
/// The invoice is a copy of what was agreed, not a link to it: an order
/// edited afterwards must not silently rewrite a document somebody
/// already sent. Only confirmed orders are billed, and only once — the
/// second click says so instead of quietly issuing a duplicate.
fn action_create_invoice<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [order_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "fature um pedido de cada vez".into(),
            ));
        };
        let orders = ctx
            .registry
            .read(
                ctx.pool,
                "sale.order",
                &[order_id],
                &["name", "state", "partner_id", "company_id"],
            )
            .await?;
        let order = orders
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("pedido {order_id} não existe")))?;
        let name = order.get("name").and_then(Value::as_str).unwrap_or("");
        let state = order.get("state").and_then(Value::as_str).unwrap_or("draft");
        if state != "sale" {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} não está confirmado: confirme antes de faturar"
            )));
        }
        // already billed? the origin field is what ties them together
        let existing = ctx
            .registry
            .search(
                ctx.pool,
                "account.move",
                &parse_domain(&json!([["invoice_origin", "=", name]]))?,
                &SearchOptions::default(),
            )
            .await?;
        if let Some(invoice) = existing.first() {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} já foi faturado (fatura {invoice})"
            )));
        }

        let lines = ctx
            .registry
            .read(ctx.pool, "sale.order", &[order_id], &["order_line"])
            .await?;
        let line_ids: Vec<i64> = lines
            .first()
            .and_then(|row| row.get("order_line"))
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if line_ids.is_empty() {
            return Err(RusdooError::Validation(format!(
                "o pedido {name} não tem linhas: não há o que faturar"
            )));
        }
        let sold = ctx
            .registry
            .read(
                ctx.pool,
                "sale.order.line",
                &line_ids,
                &["product_id", "name", "product_uom_qty", "price_unit"],
            )
            .await?;
        let invoice_lines: Vec<Value> = sold
            .iter()
            .map(|line| {
                json!([0, 0, {
                    "product_id": line.get("product_id").and_then(first_id),
                    "name": line.get("name").cloned().unwrap_or(Value::Null),
                    "quantity": line.get("product_uom_qty").cloned().unwrap_or(json!(0)),
                    "price_unit": line.get("price_unit").cloned().unwrap_or(json!(0)),
                }])
            })
            .collect();

        let invoice = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "account.move",
                vec![
                    // no name: the sequence numbers it, like every other
                    // invoice, so a billed order does not produce a
                    // document numbered differently from the rest
                    ("move_type", json!("out_invoice")),
                    (
                        "partner_id",
                        json!(order.get("partner_id").and_then(first_id)),
                    ),
                    (
                        "company_id",
                        json!(order.get("company_id").and_then(first_id)),
                    ),
                    ("invoice_origin", json!(name)),
                    ("line_ids", Value::Array(invoice_lines)),
                ],
            )
            .await?;

        // what the client should do next, in the shape Odoo answers with
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Fatura",
            "res_model": "account.move",
            "res_id": invoice,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Move `ids` from one state to another, refusing the moves that make no
/// sense — a confirmed order is not confirmed twice, and a cancelled one
/// is reopened as a quotation, not as a sale.
async fn set_state(ctx: &MethodCtx<'_>, from: &[&str], to: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "a ação precisa de pelo menos um pedido".into(),
        ));
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "sale.order", &ctx.ids, &["name", "state"])
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
            "sale.order",
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
    Box::pin(async move { set_state(&ctx, &["draft"], "sale").await })
}

fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["draft", "sale"], "cancel").await })
}

fn action_draft<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["cancel"], "draft").await })
}

fn models() -> Vec<Model> {
    vec![order(), order_line()]
}

/// `sale.order` — the order itself.
fn order() -> Model {
    Model::new(
        meta("sale.order", "sale_order"),
        vec![
            char("name").required().from_sequence("sale.order"),
            m2o("partner_id", "res.partner").required(),
            m2o("company_id", "res.company"),
            Field::new("date_order", FieldType::Datetime),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Orçamento".into()),
                    ("sale".into(), "Pedido confirmado".into()),
                    ("cancel".into(), "Cancelado".into()),
                ]),
            )
            .default_value(json!("draft")),
            Field::new(
                "order_line",
                FieldType::One2many {
                    comodel: "sale.order.line".into(),
                    inverse: "order_id".into(),
                },
            ),
            // stored: an order list showing totals must not compute one
            // per row, and a total is asked for far more often than the
            // lines change
            Field::new("amount_total", PRICE)
                .computed(&["order_line.price_subtotal"], amount_total)
                .store(),
            Field::new("note", FieldType::Text),
        ],
    )
}

/// A line nobody can fulfil: zero of something is not a sale, and a
/// negative price is a refund written in the wrong place.
fn line_is_sellable(record: &Map<String, Value>) -> Result<(), String> {
    if number(record, "product_uom_qty") <= 0.0 {
        return Err("a quantidade de uma linha precisa ser maior que zero".into());
    }
    if number(record, "price_unit") < 0.0 {
        return Err("o preço unitário não pode ser negativo".into());
    }
    Ok(())
}

/// `sale.order.line` — one thing sold, at one price.
fn order_line() -> Model {
    Model::new(
        meta("sale.order.line", "sale_order_line"),
        vec![
            m2o("order_id", "sale.order").required(),
            m2o("product_id", "product.product"),
            char("name"),
            Field::new("product_uom_qty", PRICE).default_value(json!(1.0)),
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new("price_subtotal", PRICE)
                .computed(&["product_uom_qty", "price_unit"], price_subtotal)
                .store(),
        ],
    )
    .constrained(
        "linha vendável",
        &["product_uom_qty", "price_unit"],
        line_is_sellable,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_worth_quantity_times_price() {
        let mut record = Map::new();
        record.insert("product_uom_qty".into(), json!(3));
        record.insert("price_unit".into(), json!(19.9));
        assert_eq!(price_subtotal(&record), json!(59.7));
    }

    #[test]
    fn an_order_is_worth_the_sum_of_its_lines() {
        let mut record = Map::new();
        record.insert("order_line.price_subtotal".into(), json!([59.7, 10.3]));
        assert_eq!(amount_total(&record), json!(70.0));
        // and an order with no lines is worth zero, not null
        assert_eq!(amount_total(&Map::new()), json!(0.0));
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        for name in ["sale.order", "sale.order.line"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the total is materialized: a list of orders reads a column
        let total = reg.get("sale.order").unwrap().field("amount_total").unwrap();
        assert!(total.stored);
    }
}
