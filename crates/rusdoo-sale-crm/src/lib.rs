//! rusdoo-sale-crm — port of `odoo/addons/sale_crm/`: the opportunity
//! and the quotation, tied together.
//!
//! Two apps that were built apart and are used together. Somebody works
//! an opportunity until the customer asks for a price; from then on the
//! deal is a quotation, and the two records have to stay one thing:
//! the order knows which opportunity it came from, the opportunity knows
//! what it is worth, and the pipeline stops being a list of guesses the
//! moment there are real orders behind it.
//!
//! That is the whole module: `sale.order.opportunity_id`, the orders
//! hanging off the lead, the numbers they add up to, and the button that
//! makes the first quotation.
//!
//! ## Where this differs from Odoo
//!
//! * **No customer wizard.** Odoo opens `crm.quotation.partner` first,
//!   which asks whether to use an existing contact, create one from the
//!   lead's typed-in name, or leave the order without one. Here the
//!   quotation is made from the opportunity's `partner_id` and refused
//!   when there is none — the dialog is a way of *choosing*, and the
//!   choice it offers needs `_find_matching_partner`, a fuzzy search over
//!   email and name that belongs to `crm` and is not ported.
//! * **`sale_amount_total` counts confirmed orders only**, like Odoo,
//!   but it is not stored: it moves when an *order* is written, and a
//!   recompute only follows a write to the record it is on.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Money, at the precision an amount is written in.
const AMOUNT: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

/// The states an order is in once somebody has agreed to it. Odoo's
/// `sale_amount_total` is "untaxed total of confirmed orders", and a
/// quotation nobody signed is not one.
const CONFIRMED: [&str; 2] = ["sale", "done"];

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(order())?;
    reg.register(lead())?;
    Ok(())
}

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "crm.lead",
        "action_new_quotation",
        Operation::Write,
        action_new_quotation,
    )?;
    Ok(())
}

fn extension(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

/// The parallel arrays a dotted dependency over a one2many arrives as.
fn gathered(record: &Map<String, Value>, key: &str) -> Vec<Value> {
    record
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `sale_amount_total` — what the opportunity is actually worth, which
/// is the confirmed orders and not the hope.
fn sale_amount_total(record: &Map<String, Value>) -> Value {
    let states = gathered(record, "order_ids.state");
    let totals = gathered(record, "order_ids.amount_total");
    let sum: f64 = states
        .iter()
        .zip(totals.iter())
        .filter(|(state, _)| {
            state
                .as_str()
                .is_some_and(|state| CONFIRMED.contains(&state))
        })
        .filter_map(|(_, total)| total.as_f64())
        .sum();
    json!((sum * 100.0).round() / 100.0)
}

/// How many quotations are still quotations.
fn quotation_count(record: &Map<String, Value>) -> Value {
    let states = gathered(record, "order_ids.state");
    json!(states
        .iter()
        .filter(|state| {
            state
                .as_str()
                .is_some_and(|state| !CONFIRMED.contains(&state) && state != "cancel")
        })
        .count() as i64)
}

/// And how many became orders.
fn sale_order_count(record: &Map<String, Value>) -> Value {
    let states = gathered(record, "order_ids.state");
    json!(states
        .iter()
        .filter(|state| state.as_str().is_some_and(|s| CONFIRMED.contains(&s)))
        .count() as i64)
}

/// `sale.order` extended: which opportunity this came from.
fn order() -> Model {
    Model::new(
        extension("sale.order", "sale_order"),
        vec![
            // `set null`, not cascade: deleting an opportunity must not
            // delete the orders that came out of it — they are what the
            // company invoiced
            Field::new(
                "opportunity_id",
                FieldType::Many2one {
                    comodel: "crm.lead".into(),
                },
            )
            .ondelete(OnDelete::SetNull),
        ],
    )
}

/// `crm.lead` extended: the orders, and what they add up to.
fn lead() -> Model {
    Model::new(
        extension("crm.lead", "crm_lead"),
        vec![
            Field::new(
                "order_ids",
                FieldType::One2many {
                    comodel: "sale.order".into(),
                    inverse: "opportunity_id".into(),
                },
            ),
            // none of the three is materialised: they move when an
            // *order* is written, and a recompute only follows a write to
            // the record it is on — a column here would go stale the
            // first time somebody confirmed a quotation
            Field::new("sale_amount_total", AMOUNT).computed(
                &["order_ids.state", "order_ids.amount_total"],
                sale_amount_total,
            ),
            Field::new("quotation_count", FieldType::Integer)
                .computed(&["order_ids.state"], quotation_count),
            Field::new("sale_order_count", FieldType::Integer)
                .computed(&["order_ids.state"], sale_order_count),
        ],
    )
}

/// `action_new_quotation` — the button on an opportunity.
///
/// Answers the act_window Odoo answers with, so a client opens the new
/// quotation instead of guessing where it went.
fn action_new_quotation<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [lead] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "quote one opportunity at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "crm.lead",
                &[lead],
                &["type", "partner_id", "name", "team_id", "user_id"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("no opportunity {lead}")))?;
        if row.get("type").and_then(Value::as_str) != Some("opportunity") {
            return Err(RusdooError::Validation(
                "a lead is quoted after it becomes an opportunity, not before".into(),
            ));
        }
        let partner = row
            .get("partner_id")
            .and_then(|value| match value {
                Value::Array(pair) => pair.first().and_then(Value::as_i64),
                other => other.as_i64(),
            })
            .ok_or_else(|| {
                // Odoo asks in a dialog which contact to use or create;
                // refusing says the same thing without inventing one
                RusdooError::Validation(
                    "the opportunity has no customer: set partner_id before quoting".into(),
                )
            })?;
        let order = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "sale.order",
                // no `origin`: this port's `sale.order` has no such
                // column yet, and the link back to the opportunity says
                // where the quotation came from more exactly than a
                // string copy of its name would
                vec![
                    ("partner_id", json!(partner)),
                    ("opportunity_id", json!(lead)),
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "res_model": "sale.order",
            "res_id": order,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(orders: Value) -> Map<String, Value> {
        orders.as_object().expect("an object").clone()
    }

    #[test]
    fn only_the_orders_somebody_signed_count_as_revenue() {
        let row = record(json!({
            "order_ids.state": ["draft", "sale", "done", "cancel"],
            "order_ids.amount_total": [100.0, 200.0, 50.0, 999.0],
        }));
        assert_eq!(sale_amount_total(&row), json!(250.0));
        assert_eq!(sale_order_count(&row), json!(2));
        // a cancelled quotation is not a quotation anybody is waiting on
        assert_eq!(quotation_count(&row), json!(1));
    }

    #[test]
    fn an_opportunity_with_no_orders_is_worth_nothing_yet() {
        let row = record(json!({}));
        assert_eq!(sale_amount_total(&row), json!(0.0));
        assert_eq!(quotation_count(&row), json!(0));
        assert_eq!(sale_order_count(&row), json!(0));
    }

    #[test]
    fn the_two_documents_point_at_each_other() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_utm::extend(&mut reg).unwrap();
        rusdoo_sales_team::extend(&mut reg).unwrap();
        rusdoo_crm::extend(&mut reg).unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        rusdoo_stock::extend(&mut reg).unwrap();
        rusdoo_sale::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();

        let order = reg.get("sale.order").expect("registered");
        assert!(order.field("opportunity_id").is_some());
        // what `sale` brought is still there
        assert!(order.field("amount_total").is_some());
        let lead = reg.get("crm.lead").expect("registered");
        assert!(lead.field("order_ids").is_some());
        assert!(lead.field("stage_id").is_some(), "crm's own field survives");
    }
}
