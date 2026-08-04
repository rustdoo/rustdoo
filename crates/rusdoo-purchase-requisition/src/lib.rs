//! rusdoo-purchase-requisition — port of `odoo/addons/purchase_requisition/`:
//! the two agreements a buyer signs before there is any order.
//!
//! A **blanket order** is a deal with one vendor for a period: agreed
//! products at agreed prices, drawn down by request for quotation after
//! request for quotation. A **purchase template** is the same list of
//! products without the deal — a shape to start a quotation from. Both
//! are one model, `purchase.requisition`, told apart by
//! `requisition_type`, exactly as Odoo 19 does it.
//!
//! The other half of the addon is the **call for tender**: the same need
//! sent to several vendors as alternative requests for quotation, so
//! their offers can be read side by side. The alternatives are held
//! together by `purchase.order.group`, a technical model with no screen
//! of its own — Odoo's own description of it, and the reason it dies as
//! soon as fewer than two orders are left in it.
//!
//! What is deliberately not here is listed in the crate's report, but
//! the two that shape the code are worth saying next to it: this port
//! has no `product.supplierinfo`, so confirming a blanket order cannot
//! publish its prices onto the vendor's price list; and
//! `purchase.order.line` here refuses a zero quantity, so a request for
//! quotation born of an agreement carries the agreed quantity instead of
//! Odoo's zero.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::model::ModelMeta;
use rusdoo_orm::registry::Registry;
use serde_json::{Map, Value};

mod agreement;
mod alternatives;
mod models;

/// The series a blanket order is numbered from (`BO00001`).
pub const BLANKET_ORDER_SEQUENCE: &str = "purchase.requisition.blanket.order";

/// The series a purchase template is numbered from (`PT00001`).
pub const PURCHASE_TEMPLATE_SEQUENCE: &str = "purchase.requisition.purchase.template";

/// The states a request for quotation may still be cancelled or
/// confirmed from.
///
/// Odoo lists `draft`, `sent` and `to approve`; the port's
/// `purchase.order` only ever reaches `draft`, `purchase` and `cancel`,
/// so what is left of that list is the one state that means "not decided
/// yet".
pub(crate) const OPEN_RFQ_STATES: [&str; 1] = ["draft"];

/// The states an agreement may be deleted in (`_unlink_if_draft_or_cancel`).
pub(crate) const DELETABLE_STATES: [&str; 2] = ["draft", "cancel"];

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    // the group first: `purchase.order`'s new many2one names it
    reg.register(models::order_group())?;
    reg.register(models::requisition())?;
    reg.register(models::requisition_line())?;
    reg.register(models::purchase_order())?;
    reg.register(models::create_alternative_wizard())?;
    reg.register(models::alternative_warning_wizard())?;
    Ok(())
}

/// Every button this addon puts on a screen.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // `create` and `write` are overrides, not new buttons: the agreement
    // draws its number from one of two series depending on its type, and
    // changing that type after the fact has to renumber it. Both are
    // `create`/`write` in Odoo too.
    methods.register(
        "purchase.requisition",
        "create",
        Operation::Create,
        agreement::create,
    )?;
    methods.register(
        "purchase.requisition",
        "write",
        Operation::Write,
        agreement::write,
    )?;
    methods.register(
        "purchase.requisition",
        "action_confirm",
        Operation::Write,
        agreement::action_confirm,
    )?;
    methods.register(
        "purchase.requisition",
        "action_draft",
        Operation::Write,
        agreement::action_draft,
    )?;
    methods.register(
        "purchase.requisition",
        "action_cancel",
        Operation::Write,
        agreement::action_cancel,
    )?;
    methods.register(
        "purchase.requisition",
        "action_done",
        Operation::Write,
        agreement::action_done,
    )?;
    // the "New Quotation" button of the agreement's header
    methods.register(
        "purchase.requisition",
        "action_create_rfq",
        Operation::Write,
        agreement::action_create_rfq,
    )?;
    // `qty_ordered` cannot be a computed field here (see the report), so
    // the number a line shows is asked for
    methods.register(
        "purchase.requisition",
        "get_ordered_quantities",
        Operation::Read,
        agreement::get_ordered_quantities,
    )?;

    // Odoo overrides `purchase.order.button_confirm`; the port's
    // `purchase` module named its own confirm `action_confirm`, and this
    // registry has no way to wrap another module's method — so the
    // alternatives check lands under Odoo's own name, and the form's
    // confirm button points here.
    methods.register(
        "purchase.order",
        "button_confirm",
        Operation::Write,
        alternatives::button_confirm,
    )?;
    methods.register(
        "purchase.order",
        "action_create_alternative",
        Operation::Write,
        alternatives::action_create_alternative,
    )?;
    methods.register(
        "purchase.order",
        "action_compare_alternative_lines",
        Operation::Read,
        alternatives::action_compare_alternative_lines,
    )?;
    methods.register(
        "purchase.order",
        "get_tender_best_lines",
        Operation::Read,
        alternatives::get_tender_best_lines,
    )?;
    methods.register(
        "purchase.order",
        "action_remove_from_group",
        Operation::Write,
        alternatives::action_remove_from_group,
    )?;
    methods.register(
        "purchase.requisition.create.alternative",
        "action_create_alternative",
        Operation::Write,
        alternatives::wizard_create_alternative,
    )?;
    methods.register(
        "purchase.requisition.alternative.warning",
        "action_keep_alternatives",
        Operation::Write,
        alternatives::action_keep_alternatives,
    )?;
    methods.register(
        "purchase.requisition.alternative.warning",
        "action_cancel_alternatives",
        Operation::Write,
        alternatives::action_cancel_alternatives,
    )?;
    Ok(())
}

// ── the small shapes every model file repeats ───────────────────────

pub(crate) fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

pub(crate) fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

pub(crate) fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    Field::new(
        name,
        FieldType::One2many {
            comodel: comodel.to_string(),
            inverse: inverse.to_string(),
        },
    )
}

pub(crate) fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The id inside a many2one, which reads as `[id, name]` — or as a plain
/// number when nobody resolved the name.
pub(crate) fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The ids behind an x2many field, which reads as a flat list.
pub(crate) fn ids_of(record: &Map<String, Value>, field: &str) -> Vec<i64> {
    record
        .get(field)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// A numeric field's value, whatever shape the driver handed it back in.
pub(crate) fn number(record: &Map<String, Value>, field: &str) -> f64 {
    record
        .get(field)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// A text field's value, or `""` — a missing name is never a panic.
pub(crate) fn text<'a>(record: &'a Map<String, Value>, field: &str) -> &'a str {
    record.get(field).and_then(Value::as_str).unwrap_or("")
}

/// A field the caller may or may not have filled in, trimmed.
pub(crate) fn filled<'a>(record: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    record
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_many2one_reads_the_same_whichever_shape_it_arrives_in() {
        assert_eq!(first_id(&serde_json::json!([7, "BO00001"])), Some(7));
        assert_eq!(first_id(&serde_json::json!(7)), Some(7));
        assert_eq!(first_id(&Value::Null), None);
        assert_eq!(first_id(&Value::Bool(false)), None);
    }

    #[test]
    fn an_x2many_reads_as_the_ids_it_holds() {
        let mut record = Map::new();
        record.insert("line_ids".into(), serde_json::json!([3, 4, 5]));
        assert_eq!(ids_of(&record, "line_ids"), vec![3, 4, 5]);
        // and a record that has none answers an empty list, not null
        assert_eq!(ids_of(&record, "purchase_ids"), Vec::<i64>::new());
    }
}
