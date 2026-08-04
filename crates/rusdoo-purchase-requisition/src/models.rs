//! The models the addon adds, and the ones it extends.
//!
//! Port of `models/purchase_requisition.py` and `models/purchase.py`.

use crate::{char, first_id, ids_of, m2o, meta, o2m, DELETABLE_STATES};
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_product::PRICE;
use serde_json::{json, Map, Value};

/// `order_count` — how many requests for quotation came out of this
/// agreement.
///
/// Not materialised: the number changes when a *purchase order* is
/// written, and the recompute only follows a write to the agreement
/// itself. A stored column here would go quietly stale.
fn order_count(record: &Map<String, Value>) -> Value {
    json!(ids_of(record, "purchase_ids").len())
}

/// `_check_dates` — an agreement that ends before it starts.
///
/// Dates arrive as `YYYY-MM-DD`, which orders correctly as text; there
/// is nothing to parse and nothing that can fail to parse.
fn dates_are_in_order(record: &Map<String, Value>) -> Result<(), String> {
    let start = record.get("date_start").and_then(Value::as_str);
    let end = record.get("date_end").and_then(Value::as_str);
    let (Some(start), Some(end)) = (start, end) else {
        // an open-ended agreement is normal: Odoo checks nothing here
        return Ok(());
    };
    if end < start {
        return Err(format!(
            "the agreement starts on {start} and ends on {end}: \
             an end date cannot come before the start date"
        ));
    }
    Ok(())
}

/// `_unlink_if_draft_or_cancel` — a confirmed or closed agreement is not
/// deleted.
///
/// Requests for quotation point at it, and so does everything a buyer
/// negotiated. Closing it is the way out, not deleting it.
fn refuse_unless_draft_or_cancel(
    mut ctx: rusdoo_orm::unlink::UnlinkCtx<'_>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RusdooError>> + Send + '_>> {
    Box::pin(async move {
        for record in ctx.read(&["name", "state"]).await? {
            let state = record
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("draft");
            if DELETABLE_STATES.contains(&state) {
                continue;
            }
            let name = record.get("name").and_then(Value::as_str).unwrap_or("?");
            return Err(RusdooError::Validation(format!(
                "agreement {name} is {state:?}: only a draft or cancelled agreement is deleted"
            )));
        }
        Ok(())
    })
}

/// A line nobody can order: the agreement is about a quantity of
/// something, and zero of it is not an agreement.
///
/// Odoo checks the quantity and the price only at confirmation, and only
/// for a blanket order. Here the quantity is refused outright because
/// `purchase.order.line` refuses it too — a line that cannot become an
/// order line is worth nothing on an agreement.
fn line_is_orderable(record: &Map<String, Value>) -> Result<(), String> {
    if crate::number(record, "product_qty") <= 0.0 {
        return Err("a line's quantity must be greater than zero".into());
    }
    if crate::number(record, "price_unit") < 0.0 {
        return Err("the unit price cannot be negative".into());
    }
    Ok(())
}

/// `purchase.requisition` — a blanket order or a purchase template.
pub(crate) fn requisition() -> Model {
    Model::new(
        meta("purchase.requisition", "purchase_requisition"),
        vec![
            // no `from_sequence`: which series numbers this record
            // depends on `requisition_type`, and that is decided in the
            // create override
            char("name").required(),
            // an agreement that is over is archived, never deleted: the
            // orders signed under it still point at it
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // the vendor's own number for the deal, so a buyer can quote
            // it on the phone
            char("reference"),
            m2o("vendor_id", "res.partner"),
            Field::new(
                "requisition_type",
                FieldType::Selection(vec![
                    ("blanket_order".into(), "Blanket Order".into()),
                    ("purchase_template".into(), "Purchase Template".into()),
                ]),
            )
            .required()
            .default_value(json!("blanket_order")),
            Field::new("date_start", FieldType::Date),
            Field::new("date_end", FieldType::Date),
            m2o("user_id", "res.users").default_from(rusdoo_orm::defaults::CURRENT_USER),
            // the terms and conditions, which land in the quotation's
            // notes
            Field::new("description", FieldType::Html),
            // Odoo marks this required; here it is not, because the
            // default reads the acting user's company and a database
            // that never filled that in would refuse every agreement
            m2o("company_id", "res.company").default_from(rusdoo_orm::defaults::USER_COMPANY),
            o2m("purchase_ids", "purchase.order", "requisition_id"),
            o2m(
                "line_ids",
                "purchase.requisition.line",
                "requisition_id",
            ),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Draft".into()),
                    ("confirmed".into(), "Confirmed".into()),
                    ("done".into(), "Closed".into()),
                    ("cancel".into(), "Cancelled".into()),
                ]),
            )
            .required()
            .default_value(json!("draft")),
            Field::new("order_count", FieldType::Integer)
                .computed(&["purchase_ids"], order_count),
        ],
    )
    .ordered("id desc")
    .constrained(
        "agreement validity",
        &["date_start", "date_end"],
        dates_are_in_order,
    )
    .on_unlink("only a draft or cancelled agreement", refuse_unless_draft_or_cancel)
}

/// `purchase.requisition.line` — one product the agreement covers.
pub(crate) fn requisition_line() -> Model {
    Model::new(
        meta(
            "purchase.requisition.line",
            "purchase_requisition_line",
        ),
        vec![
            m2o("requisition_id", "purchase.requisition")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("product_id", "product.product").required(),
            // Odoo computes this from the product and lets the user
            // override it; the port has no writable compute, so it is a
            // plain field the form fills in
            m2o("product_uom_id", "uom.uom"),
            Field::new("product_qty", PRICE).default_value(json!(1.0)),
            // what the vendor calls it, when that differs from the
            // catalogue — it is appended to the order line's description
            char("product_description_variants"),
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            // Odoo stores this as a related field; a related field here
            // is never stored, and the multi-company rule that would
            // need the column lives on the agreement instead
            m2o("company_id", "res.company").related("requisition_id.company_id"),
        ],
    )
    .constrained(
        "orderable line",
        &["product_qty", "price_unit"],
        line_is_orderable,
    )
}

/// `purchase.order.group` — what holds a call for tender together.
///
/// Odoo calls it a "technical model to group PO for call to tenders",
/// and that is all it is: no name, no screen, one relation. It exists so
/// that "the alternatives of this order" is a fact stored once instead
/// of a link every order has to keep to every other.
pub(crate) fn order_group() -> Model {
    Model::new(
        meta("purchase.order.group", "purchase_order_group"),
        vec![o2m("order_ids", "purchase.order", "purchase_group_id")],
    )
}

/// `purchase.order` extended (`_inherit`): where it came from, and what
/// it is being compared against.
pub(crate) fn purchase_order() -> Model {
    Model::new(
        ModelMeta {
            name: "purchase.order".into(),
            table: "purchase_order".into(),
            inherit: vec!["purchase.order".into()],
            inherits: vec![],
        },
        vec![
            m2o("requisition_id", "purchase.requisition"),
            // a related selection, so a list can be grouped by the kind
            // of agreement without a join the client has to know about
            Field::new(
                "requisition_type",
                FieldType::Selection(vec![
                    ("blanket_order".into(), "Blanket Order".into()),
                    ("purchase_template".into(), "Purchase Template".into()),
                ]),
            )
            .related("requisition_id.requisition_type"),
            // an order leaves its group when the group is dissolved, so
            // the reference is emptied rather than refusing the delete
            m2o("purchase_group_id", "purchase.order.group").ondelete(OnDelete::SetNull),
            // the whole group, this order included — Odoo's field is the
            // same and the form's domain is what hides the order from
            // its own list of alternatives
            o2m(
                "alternative_po_ids",
                "purchase.order",
                "purchase_group_id",
            )
            .related("purchase_group_id.order_ids"),
        ],
    )
}

/// `purchase.requisition.create.alternative` — "ask these vendors too".
pub(crate) fn create_alternative_wizard() -> Model {
    Model::new(
        meta(
            "purchase.requisition.create.alternative",
            "purchase_requisition_create_alternative",
        ),
        vec![
            m2o("origin_po_id", "purchase.order").required(),
            Field::new(
                "partner_ids",
                FieldType::Many2many {
                    comodel: "res.partner".into(),
                    relation: "purchase_requisition_alternative_partner_rel".into(),
                    column1: "wizard_id".into(),
                    column2: "partner_id".into(),
                },
            ),
            // ticked by default: a call for tender is the same need sent
            // to several vendors, so the products are the point
            Field::new("copy_products", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .transient()
}

/// `purchase.requisition.alternative.warning` — "there are still open
/// alternatives; keep them or cancel them?".
pub(crate) fn alternative_warning_wizard() -> Model {
    Model::new(
        meta(
            "purchase.requisition.alternative.warning",
            "purchase_requisition_alternative_warning",
        ),
        vec![
            Field::new(
                "po_ids",
                FieldType::Many2many {
                    comodel: "purchase.order".into(),
                    relation: "warning_purchase_order_rel".into(),
                    column1: "warning_id".into(),
                    column2: "order_id".into(),
                },
            ),
            Field::new(
                "alternative_po_ids",
                FieldType::Many2many {
                    comodel: "purchase.order".into(),
                    relation: "warning_purchase_order_alternative_rel".into(),
                    column1: "warning_id".into(),
                    column2: "order_id".into(),
                },
            ),
        ],
    )
    .transient()
}

/// The type an agreement is being given, out of the values written.
///
/// Shared by the create and the write override: both have to know which
/// series numbers the record, and both read it the same way.
pub(crate) fn requisition_type_of(values: &Map<String, Value>, fallback: &str) -> String {
    values
        .get("requisition_type")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// The `ir.sequence` code an agreement of this type is numbered from.
pub(crate) fn sequence_for(requisition_type: &str) -> &'static str {
    match requisition_type {
        "purchase_template" => crate::PURCHASE_TEMPLATE_SEQUENCE,
        _ => crate::BLANKET_ORDER_SEQUENCE,
    }
}

/// The company being written, as an id — `null` when the values do not
/// name one.
pub(crate) fn company_of(values: &Map<String, Value>) -> Option<i64> {
    values.get("company_id").and_then(first_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agreement_counts_the_quotations_that_came_out_of_it() {
        let mut record = Map::new();
        record.insert("purchase_ids".into(), json!([4, 9, 11]));
        assert_eq!(order_count(&record), json!(3));
        // and one nobody quoted yet counts zero, not null
        assert_eq!(order_count(&Map::new()), json!(0));
    }

    #[test]
    fn an_agreement_may_not_end_before_it_starts() {
        let mut record = Map::new();
        record.insert("date_start".into(), json!("2026-01-01"));
        record.insert("date_end".into(), json!("2026-12-31"));
        assert!(dates_are_in_order(&record).is_ok());

        record.insert("date_end".into(), json!("2025-12-31"));
        let error = dates_are_in_order(&record).expect_err("the dates are backwards");
        assert!(error.contains("cannot come before"), "{error}");
    }

    #[test]
    fn an_open_ended_agreement_has_nothing_to_check() {
        let mut record = Map::new();
        record.insert("date_start".into(), json!("2026-01-01"));
        assert!(dates_are_in_order(&record).is_ok());
        assert!(dates_are_in_order(&Map::new()).is_ok());
    }

    #[test]
    fn a_line_of_nothing_is_not_an_agreement() {
        let mut record = Map::new();
        record.insert("product_qty".into(), json!(0));
        let error = line_is_orderable(&record).expect_err("zero is not a quantity");
        assert!(error.contains("greater than zero"), "{error}");

        record.insert("product_qty".into(), json!(10));
        record.insert("price_unit".into(), json!(-1));
        let error = line_is_orderable(&record).expect_err("a negative price is a typo");
        assert!(error.contains("cannot be negative"), "{error}");

        record.insert("price_unit".into(), json!(0));
        assert!(
            line_is_orderable(&record).is_ok(),
            "a price of zero is a quotation waiting for one"
        );
    }

    #[test]
    fn each_kind_of_agreement_draws_from_its_own_series() {
        assert_eq!(sequence_for("blanket_order"), crate::BLANKET_ORDER_SEQUENCE);
        assert_eq!(
            sequence_for("purchase_template"),
            crate::PURCHASE_TEMPLATE_SEQUENCE
        );
        // an unknown type is numbered as what the field defaults to
        assert_eq!(sequence_for(""), crate::BLANKET_ORDER_SEQUENCE);
    }

    #[test]
    fn the_type_written_wins_over_the_one_already_there() {
        let mut values = Map::new();
        assert_eq!(requisition_type_of(&values, "blanket_order"), "blanket_order");
        values.insert("requisition_type".into(), json!("purchase_template"));
        assert_eq!(
            requisition_type_of(&values, "blanket_order"),
            "purchase_template"
        );
    }
}
