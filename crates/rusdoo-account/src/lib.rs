//! rusdoo-account — port of `odoo/addons/account/models/`: the invoice.
//!
//! Odoo keeps invoices and journal entries in one model (`account.move`)
//! with a `move_type` telling them apart, and the port keeps that: an
//! invoice is a document with lines, a total, and a life of its own
//! (draft → posted → cancelled).
//!
//! What "posted" means here is narrow and deliberate: the document
//! becomes read-only and its number is fixed. Ledgers, taxes and
//! reconciliation are not ported, and pretending otherwise by writing
//! numbers into an accounting nobody implemented would be worse than
//! saying so.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
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

fn round(value: f64) -> Value {
    json!((value * 100.0).round() / 100.0)
}

/// `price_subtotal` — one line of the invoice.
fn price_subtotal(record: &Map<String, Value>) -> Value {
    round(number(record, "quantity") * number(record, "price_unit"))
}

/// `amount_total` — the document is worth what its lines are worth.
fn amount_total(record: &Map<String, Value>) -> Value {
    let lines = record
        .get("line_ids.price_subtotal")
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
    reg.register(mv())?;
    reg.register(move_line())?;
    Ok(())
}

/// The buttons of an invoice.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register("account.move", "action_post", Operation::Write, action_post)?;
    methods.register(
        "account.move",
        "action_cancel",
        Operation::Write,
        action_cancel,
    )?;
    methods.register(
        "account.move",
        "action_draft",
        Operation::Write,
        action_draft,
    )?;
    Ok(())
}

/// A due date before the invoice date would make a document born
/// overdue — the dates travel as `YYYY-MM-DD`, which compares as text.
fn due_after_issue(record: &Map<String, Value>) -> Result<(), String> {
    let issued = record.get("invoice_date").and_then(Value::as_str);
    let due = record.get("invoice_date_due").and_then(Value::as_str);
    if let (Some(issued), Some(due)) = (issued, due) {
        if due < issued {
            return Err(format!(
                "o vencimento ({due}) é anterior à data da fatura ({issued})"
            ));
        }
    }
    Ok(())
}

/// `account.move` — an invoice (or a bill, or a plain entry).
fn mv() -> Model {
    Model::new(
        meta("account.move", "account_move"),
        vec![
            char("name").required().from_sequence("account.move"),
            char("ref"),
            m2o("partner_id", "res.partner").required(),
            m2o("company_id", "res.company"),
            Field::new("invoice_date", FieldType::Date),
            Field::new("invoice_date_due", FieldType::Date),
            Field::new(
                "move_type",
                FieldType::Selection(vec![
                    ("out_invoice".into(), "Fatura de cliente".into()),
                    ("in_invoice".into(), "Fatura de fornecedor".into()),
                    ("entry".into(), "Lançamento".into()),
                ]),
            )
            .default_value(json!("out_invoice")),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Rascunho".into()),
                    ("posted".into(), "Lançada".into()),
                    ("cancel".into(), "Cancelada".into()),
                ]),
            )
            .default_value(json!("draft")),
            Field::new(
                "line_ids",
                FieldType::One2many {
                    comodel: "account.move.line".into(),
                    inverse: "move_id".into(),
                },
            ),
            // stored: a list of invoices shows the total, and a total is
            // read far more often than the lines change
            Field::new("amount_total", PRICE)
                .computed(&["line_ids.price_subtotal"], amount_total)
                .store(),
            // where the document came from, when it came from a sale
            char("invoice_origin"),
            Field::new("narration", FieldType::Text),
        ],
    )
    .constrained(
        "vencimento após a emissão",
        &["invoice_date", "invoice_date_due"],
        due_after_issue,
    )
}

/// `account.move.line` — one billed thing, at one price.
fn move_line() -> Model {
    Model::new(
        meta("account.move.line", "account_move_line"),
        vec![
            m2o("move_id", "account.move").required(),
            m2o("product_id", "product.product"),
            char("name"),
            Field::new("quantity", PRICE).default_value(json!(1.0)),
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new("price_subtotal", PRICE)
                .computed(&["quantity", "price_unit"], price_subtotal)
                .store(),
        ],
    )
}

/// Move `ids` between states, refusing what makes no sense — a posted
/// invoice is not posted again, and a cancelled one is reopened as a
/// draft, not as a posted document.
async fn set_state(ctx: &MethodCtx<'_>, from: &[&str], to: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "a ação precisa de pelo menos uma fatura".into(),
        ));
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "account.move", &ctx.ids, &["name", "state"])
        .await?;
    for row in &rows {
        let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
        if !from.contains(&state) {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            return Err(RusdooError::Validation(format!(
                "a fatura {name} está em {state:?} e não pode ir para {to:?}"
            )));
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "account.move",
            &ctx.ids,
            vec![("state", json!(to))],
        )
        .await?;
    Ok(json!(true))
}

/// Posting an invoice with no lines would post a document worth nothing
/// — in Odoo that is an error, and it is one here.
fn action_post<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rows = ctx
            .registry
            .read(ctx.pool, "account.move", &ctx.ids, &["name", "line_ids"])
            .await?;
        for row in &rows {
            let empty = row
                .get("line_ids")
                .and_then(Value::as_array)
                .is_none_or(|lines| lines.is_empty());
            if empty {
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                return Err(RusdooError::Validation(format!(
                    "a fatura {name} não tem linhas: não há o que lançar"
                )));
            }
        }
        set_state(&ctx, &["draft"], "posted").await
    })
}

fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["draft", "posted"], "cancel").await })
}

fn action_draft<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { set_state(&ctx, &["cancel"], "draft").await })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_worth_quantity_times_price() {
        let mut record = Map::new();
        record.insert("quantity".into(), json!(3));
        record.insert("price_unit".into(), json!(19.9));
        assert_eq!(price_subtotal(&record), json!(59.7));
    }

    #[test]
    fn an_invoice_is_worth_the_sum_of_its_lines() {
        let mut record = Map::new();
        record.insert("line_ids.price_subtotal".into(), json!([59.7, 10.3]));
        assert_eq!(amount_total(&record), json!(70.0));
        assert_eq!(amount_total(&Map::new()), json!(0.0));
    }

    #[test]
    fn the_models_register_on_top_of_base_and_product() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        for name in ["account.move", "account.move.line"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        let total = reg
            .get("account.move")
            .unwrap()
            .field("amount_total")
            .unwrap();
        assert!(total.stored, "a list of invoices reads a column");
    }
}
