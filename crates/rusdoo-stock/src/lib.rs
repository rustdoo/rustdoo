//! rusdoo-stock — port of `odoo/addons/stock/models/`: what physically
//! moves.
//!
//! A picking is a document (a delivery, a receipt) holding moves; a move
//! is one product going from one place to another. Confirming reserves
//! nothing yet and validating posts no quants: the port models the
//! documents and their lifecycle, not the stock valuation behind them.
//! Saying so is better than a number that looks like inventory and is
//! not.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
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

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(location())?;
    reg.register(picking())?;
    reg.register(mv())?;
    Ok(())
}

/// The buttons of a picking.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "stock.picking",
        "action_confirm",
        Operation::Write,
        action_confirm,
    )?;
    methods.register(
        "stock.picking",
        "action_done",
        Operation::Write,
        action_done,
    )?;
    methods.register(
        "stock.picking",
        "action_cancel",
        Operation::Write,
        action_cancel,
    )?;
    Ok(())
}

/// `stock.location` — a place things can be.
fn location() -> Model {
    Model::new(
        meta("stock.location", "stock_location"),
        vec![
            char("name").required(),
            char("complete_name"),
            m2o("location_id", "stock.location"),
            Field::new(
                "usage",
                FieldType::Selection(vec![
                    ("internal".into(), "Internal".into()),
                    ("customer".into(), "Customer".into()),
                    ("supplier".into(), "Vendor".into()),
                    ("inventory".into(), "Inventory adjustment".into()),
                ]),
            )
            .default_value(json!("internal")),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `stock.picking` — a delivery, a receipt, a transfer.
fn picking() -> Model {
    Model::new(
        meta("stock.picking", "stock_picking"),
        vec![
            char("name").required().from_sequence("stock.picking.out"),
            m2o("partner_id", "res.partner"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("location_id", "stock.location"),
            m2o("location_dest_id", "stock.location"),
            Field::new("scheduled_date", FieldType::Datetime).default_from(defaults::NOW),
            Field::new(
                "picking_type",
                FieldType::Selection(vec![
                    ("outgoing".into(), "Delivery".into()),
                    ("incoming".into(), "Receipt".into()),
                    ("internal".into(), "Internal transfer".into()),
                ]),
            )
            .default_value(json!("outgoing")),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Draft".into()),
                    ("confirmed".into(), "Confirmed".into()),
                    ("done".into(), "Done".into()),
                    ("cancel".into(), "Cancelled".into()),
                ]),
            )
            .default_value(json!("draft")),
            Field::new(
                "move_ids",
                FieldType::One2many {
                    comodel: "stock.move".into(),
                    inverse: "picking_id".into(),
                },
            ),
            // where the document came from, when it came from a sale
            char("origin"),
            Field::new("note", FieldType::Text),
        ],
    )
    // Odoo puts priority first (`priority desc, scheduled_date asc, id
    // desc`); without it, the promised date is what
    // que organiza a fila de quem separa
    .ordered("scheduled_date asc, id desc")
}

/// Nothing moves backwards: a negative quantity is a move in the other
/// direction, and that is a different document.
fn quantities_are_positive(record: &Map<String, Value>) -> Result<(), String> {
    if number(record, "product_uom_qty") <= 0.0 {
        return Err("a move's quantity must be greater than zero".into());
    }
    if number(record, "quantity_done") < 0.0 {
        return Err("the done quantity cannot be negative".into());
    }
    Ok(())
}

/// `stock.move` — one product, one quantity, one direction.
fn mv() -> Model {
    Model::new(
        meta("stock.move", "stock_move"),
        vec![
            m2o("picking_id", "stock.picking")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("product_id", "product.product").required(),
            char("name"),
            Field::new("product_uom_qty", PRICE).default_value(json!(1.0)),
            // what was actually shipped, which is what validating records
            Field::new("quantity_done", PRICE).default_value(json!(0.0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
        ],
    )
    .constrained(
        "quantidades positivas",
        &["product_uom_qty", "quantity_done"],
        quantities_are_positive,
    )
}

/// Move `ids` between states, refusing what makes no sense.
async fn set_state(ctx: &MethodCtx<'_>, from: &[&str], to: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "the action needs at least one document".into(),
        ));
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "stock.picking", &ctx.ids, &["name", "state"])
        .await?;
    for row in &rows {
        let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
        if !from.contains(&state) {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            return Err(RusdooError::Validation(format!(
                "document {name} is {state:?} and cannot go to {to:?}"
            )));
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "stock.picking",
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
    Box::pin(async move {
        let rows = ctx
            .registry
            .read(ctx.pool, "stock.picking", &ctx.ids, &["name", "move_ids"])
            .await?;
        for row in &rows {
            let empty = row
                .get("move_ids")
                .and_then(Value::as_array)
                .is_none_or(|moves| moves.is_empty());
            if empty {
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                return Err(RusdooError::Validation(format!(
                    "document {name} has no lines: there is nothing to move"
                )));
            }
        }
        set_state(&ctx, &["draft"], "confirmed").await
    })
}

/// Validating a picking records what actually left: a move whose done
/// quantity is still zero takes the planned one, which is what a
/// warehouse clicking "validate" without touching anything means.
fn action_done<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let pickings = ctx
            .registry
            .read(ctx.pool, "stock.picking", &ctx.ids, &["move_ids"])
            .await?;
        let move_ids: Vec<i64> = pickings
            .iter()
            .filter_map(|row| row.get("move_ids").and_then(Value::as_array))
            .flat_map(|moves| moves.iter().filter_map(Value::as_i64))
            .collect();
        if !move_ids.is_empty() {
            let rows = ctx
                .registry
                .read(
                    ctx.pool,
                    "stock.move",
                    &move_ids,
                    &["product_uom_qty", "quantity_done"],
                )
                .await?;
            for row in rows {
                let done = number(&row, "quantity_done");
                if done > 0.0 {
                    continue;
                }
                let planned = number(&row, "product_uom_qty");
                let id = row.get("id").and_then(Value::as_i64).unwrap_or_default();
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "stock.move",
                        &[id],
                        vec![("quantity_done", json!(planned))],
                    )
                    .await?;
            }
        }
        set_state(&ctx, &["draft", "confirmed"], "done").await
    })
}

fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["draft", "confirmed"], "cancel").await })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_register_on_top_of_base_and_product() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        for name in ["stock.location", "stock.picking", "stock.move"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // a move without a product is not a move
        let product = reg.get("stock.move").unwrap().field("product_id").unwrap();
        assert!(product.required);
    }

    #[test]
    fn a_picking_has_the_three_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("stock.picking"),
            vec!["action_cancel", "action_confirm", "action_done"]
        );
    }
}
