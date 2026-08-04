//! The agreement's own buttons: its numbering, its state machine, and
//! the request for quotation it produces.
//!
//! Port of `PurchaseRequisition` in `models/purchase_requisition.py`.

use crate::models::{company_of, requisition_type_of, sequence_for};
use crate::{filled, first_id, ids_of, number, text, OPEN_RFQ_STATES};
use rusdoo_core::RusdooError;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const MODEL: &str = "purchase.requisition";

/// Odoo's wire format for a datetime, which is what `date_order` holds.
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The one agreement a button was pressed on.
fn only_one(ctx: &MethodCtx<'_>, what: &str) -> Result<i64, RusdooError> {
    match ctx.ids[..] {
        [id] => Ok(id),
        _ => Err(RusdooError::Validation(format!(
            "{what} works on one agreement at a time"
        ))),
    }
}

/// `create` — the agreement draws its number from the series its type
/// belongs to.
///
/// Odoo picks the sequence in `create` for the same reason: a blanket
/// order and a purchase template are one model but two documents, and a
/// buyer reading `BO00042` knows which one they have in front of them
/// without opening it. A name the caller passed is discarded, like Odoo
/// discards it — the series is what names these records.
pub(crate) fn create<'a>(
    ctx: MethodCtx<'a>,
    args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let values = args
            .first()
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                RusdooError::Validation("create needs the values of the agreement".into())
            })?;
        let requisition_type = requisition_type_of(&values, "blanket_order");
        let code = sequence_for(&requisition_type);
        let number = ctx
            .registry
            .next_sequence(ctx.pool, code)
            .await?
            .ok_or_else(|| {
                RusdooError::Validation(format!(
                    "sequence {code:?} does not exist: load the addon's data file"
                ))
            })?;
        let mut pairs: Vec<(&str, Value)> = values
            .iter()
            .filter(|(name, _)| name.as_str() != "name")
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        pairs.push(("name", json!(number)));
        let id = ctx.registry.create_as(ctx.pool, ctx.uid, MODEL, pairs).await?;
        Ok(json!(id))
    })
}

/// `write` — changing the kind of agreement renumbers it, and is only
/// allowed while it is a draft.
///
/// Odoo writes first and raises afterwards, inside one transaction. Here
/// the two are separate statements, so the refusal has to come first: a
/// write that landed and then had to be undone by hand is a worse
/// failure than the one it was guarding against.
pub(crate) fn write<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "write needs at least one agreement".into(),
            ));
        }
        let values = ctx
            .rest
            .first()
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| RusdooError::Validation("write needs a values object".into()))?;

        let before = ctx
            .registry
            .read(
                ctx.pool,
                MODEL,
                &ctx.ids,
                &["name", "state", "requisition_type", "company_id"],
            )
            .await?;
        // which records this write actually moves to another type or
        // another company — Odoo's `requisitions_to_rename`
        let mut to_rename: Vec<(i64, String)> = Vec::new();
        for row in &before {
            let Some(id) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let was = text(row, "requisition_type").to_string();
            let now = requisition_type_of(&values, &was);
            let company_changes = values.contains_key("company_id")
                && company_of(&values) != row.get("company_id").and_then(first_id);
            if now == was && !company_changes {
                continue;
            }
            let state = text(row, "state");
            if state != "draft" {
                let name = text(row, "name");
                return Err(RusdooError::Validation(format!(
                    "agreement {name} is {state:?}: the type and the company of an agreement \
                     may only be changed while it is a draft"
                )));
            }
            to_rename.push((id, now));
        }

        let pairs: Vec<(&str, Value)> = values
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        ctx.registry
            .write_as(ctx.pool, ctx.uid, MODEL, &ctx.ids, pairs)
            .await?;

        for (id, requisition_type) in &to_rename {
            let code = sequence_for(requisition_type);
            let mut renamed: Vec<(&str, Value)> = Vec::new();
            if let Some(number) = ctx.registry.next_sequence(ctx.pool, code).await? {
                renamed.push(("name", json!(number)));
            }
            // a purchase template is a shape, not a deal: it has no
            // period of validity, so the dates go with the type
            if requisition_type == "purchase_template" {
                renamed.push(("date_start", Value::Null));
                renamed.push(("date_end", Value::Null));
            }
            if !renamed.is_empty() {
                ctx.registry
                    .write_as(ctx.pool, ctx.uid, MODEL, &[*id], renamed)
                    .await?;
            }
        }
        Ok(json!(true))
    })
}

/// `action_confirm` — the agreement starts applying.
///
/// A blanket order is checked line by line first: it is a promise about
/// a price and a quantity, and a line missing either is a promise nobody
/// can keep.
///
/// What Odoo does here and this cannot: publish each line onto the
/// vendor's price list (`product.supplierinfo`), so that a later
/// quotation to that vendor picks the agreed price up by itself. The
/// port has no `product.supplierinfo` — the agreed price is applied by
/// `action_create_rfq`, which copies it onto the order line.
pub(crate) fn action_confirm<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx, "confirming an agreement")?;
        let agreement = read_one(&ctx, id, &["name", "state", "requisition_type", "line_ids"]).await?;
        let name = text(&agreement, "name").to_string();
        let state = text(&agreement, "state");
        if state != "draft" {
            return Err(RusdooError::Validation(format!(
                "agreement {name} is {state:?} and is not confirmed again"
            )));
        }
        let line_ids = ids_of(&agreement, "line_ids");
        if line_ids.is_empty() {
            return Err(RusdooError::Validation(format!(
                "agreement {name} cannot be confirmed: it contains no product lines"
            )));
        }
        if text(&agreement, "requisition_type") == "blanket_order" {
            let lines = ctx
                .registry
                .read(
                    ctx.pool,
                    "purchase.requisition.line",
                    &line_ids,
                    &["price_unit", "product_qty"],
                )
                .await?;
            for line in &lines {
                if number(line, "price_unit") <= 0.0 {
                    return Err(RusdooError::Validation(
                        "a blanket order cannot be confirmed with a line missing a price".into(),
                    ));
                }
                if number(line, "product_qty") <= 0.0 {
                    return Err(RusdooError::Validation(
                        "a blanket order cannot be confirmed with a line missing a quantity".into(),
                    ));
                }
            }
        }
        set_state(&ctx, &[id], "confirmed").await
    })
}

/// `action_draft` — reopen a cancelled agreement.
///
/// Odoo's method has no guard at all: what keeps it from reopening a
/// closed agreement is the form, where the button only appears while the
/// state is `cancel`. A method reachable over RPC needs the rule itself,
/// so the guard is here — the intent is the button's.
pub(crate) fn action_draft<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx, "resetting an agreement to draft")?;
        let agreement = read_one(&ctx, id, &["name", "state"]).await?;
        let state = text(&agreement, "state");
        if state != "cancel" {
            let name = text(&agreement, "name");
            return Err(RusdooError::Validation(format!(
                "agreement {name} is {state:?}: only a cancelled agreement is reset to draft"
            )));
        }
        set_state(&ctx, &[id], "draft").await
    })
}

/// `action_cancel` — drop the agreement and the quotations it produced.
///
/// The open requests for quotation go with it: they exist because of the
/// agreement, and leaving them behind would let somebody confirm an
/// order under a deal that no longer holds.
///
/// Odoo also writes a line into each cancelled order's chatter saying
/// why. That is not ported: a method here has the model registry but not
/// the method registry, so it cannot call `message_post`.
pub(crate) fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "cancelling needs at least one agreement".into(),
            ));
        }
        let agreements = ctx
            .registry
            .read(ctx.pool, MODEL, &ctx.ids, &["purchase_ids"])
            .await?;
        let mut orders: Vec<i64> = Vec::new();
        for agreement in &agreements {
            orders.extend(ids_of(agreement, "purchase_ids"));
        }
        let cancellable = open_orders(&ctx, &orders).await?;
        if !cancellable.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "purchase.order",
                    &cancellable,
                    vec![("state", json!("cancel"))],
                )
                .await?;
        }
        set_state(&ctx, &ctx.ids.clone(), "cancel").await
    })
}

/// `action_done` — the agreement is over.
///
/// Odoo refuses while any request for quotation is still open, and says
/// why in the plainest terms it has: confirming those duplicates would
/// double the order. The check is the point, not the joke.
pub(crate) fn action_done<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "closing needs at least one agreement".into(),
            ));
        }
        let agreements = ctx
            .registry
            .read(ctx.pool, MODEL, &ctx.ids, &["name", "state", "purchase_ids"])
            .await?;
        let mut orders: Vec<i64> = Vec::new();
        for agreement in &agreements {
            let state = text(agreement, "state");
            if state != "confirmed" {
                let name = text(agreement, "name");
                return Err(RusdooError::Validation(format!(
                    "agreement {name} is {state:?}: only a confirmed agreement is closed"
                )));
            }
            orders.extend(ids_of(agreement, "purchase_ids"));
        }
        if !open_orders(&ctx, &orders).await?.is_empty() {
            return Err(RusdooError::Validation(
                "there are still open requests for quotation under this agreement: \
                 cancel or confirm them before closing it, or the same goods get ordered twice"
                    .into(),
            ));
        }
        set_state(&ctx, &ctx.ids.clone(), "done").await
    })
}

/// `action_create_rfq` — the "New Quotation" button of the agreement.
///
/// Port of what Odoo splits between an action (`New Quotation`, which
/// opens a purchase order with `default_requisition_id`) and
/// `_onchange_requisition_id`, which then fills the order in. This
/// registry has no onchange, so the two halves are one method: the
/// button creates the quotation already filled and opens it.
///
/// Odoo gives a blanket order's lines a quantity of zero, so the buyer
/// types in what they actually want this time. The port's
/// `purchase.order.line` refuses a zero quantity, so the line is born
/// with the agreed quantity instead — the buyer edits it down rather
/// than up.
pub(crate) fn action_create_rfq<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx, "creating a quotation")?;
        let agreement = read_one(
            &ctx,
            id,
            &[
                "name",
                "state",
                "vendor_id",
                "company_id",
                "description",
                "date_start",
                "line_ids",
            ],
        )
        .await?;
        let name = text(&agreement, "name").to_string();
        let state = text(&agreement, "state");
        if state != "confirmed" {
            return Err(RusdooError::Validation(format!(
                "agreement {name} is {state:?}: confirm it before asking for quotations under it"
            )));
        }
        let vendor = agreement.get("vendor_id").and_then(first_id).ok_or_else(|| {
            RusdooError::Validation(format!(
                "agreement {name} names no vendor: a request for quotation needs one"
            ))
        })?;
        let line_ids = ids_of(&agreement, "line_ids");
        if line_ids.is_empty() {
            return Err(RusdooError::Validation(format!(
                "agreement {name} has no product lines to quote"
            )));
        }
        let lines = ctx
            .registry
            .read(
                ctx.pool,
                "purchase.requisition.line",
                &line_ids,
                &[
                    "product_id",
                    "product_qty",
                    "price_unit",
                    "product_description_variants",
                ],
            )
            .await?;

        let order_lines: Vec<Value> = lines
            .iter()
            .map(|line| {
                let described = line
                    .get("product_id")
                    .and_then(|value| value.as_array())
                    .and_then(|pair| pair.get(1))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // the vendor's own wording for the product, when the
                // agreement carries one
                let description = match filled(line, "product_description_variants") {
                    Some(variant) if described.is_empty() => variant.to_string(),
                    Some(variant) => format!("{described}\n{variant}"),
                    None => described,
                };
                json!([0, 0, {
                    "product_id": line.get("product_id").and_then(first_id),
                    "name": description,
                    "product_qty": number(line, "product_qty"),
                    "price_unit": number(line, "price_unit"),
                }])
            })
            .collect();

        // an agreement that has not started yet dates its quotations on
        // its first day, never in its past
        let now = chrono::Utc::now().format(DATETIME_FORMAT).to_string();
        let date_order = match agreement.get("date_start").and_then(Value::as_str) {
            Some(start) if format!("{start} 00:00:00") > now => format!("{start} 00:00:00"),
            _ => now,
        };

        let mut values: Vec<(&str, Value)> = vec![
            ("partner_id", json!(vendor)),
            ("requisition_id", json!(id)),
            ("date_order", json!(date_order)),
            ("order_line", Value::Array(order_lines)),
        ];
        if let Some(company) = agreement.get("company_id").and_then(first_id) {
            values.push(("company_id", json!(company)));
        }
        // the agreement's terms and conditions become the order's notes:
        // they are what the vendor has to read
        if let Some(description) = filled(&agreement, "description") {
            values.push(("notes", json!(description.to_string())));
        }
        let order = ctx
            .registry
            .create_as(ctx.pool, ctx.uid, "purchase.order", values)
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Request for Quotation",
            "res_model": "purchase.order",
            "res_id": order,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

/// `get_ordered_quantities` — how much of each line has actually been
/// ordered, port of `_compute_ordered_qty`.
///
/// A method and not a computed field: the number is a sum over the lines
/// of the orders of the agreement, two relational hops away, and a
/// compute here reads values rather than running queries.
///
/// Odoo's own oddity is kept: when two lines of the agreement name the
/// same product, the total lands on the first of them and the rest read
/// zero — the quantity was ordered once, and showing it twice would
/// suggest it was ordered twice.
pub(crate) fn get_ordered_quantities<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx, "reading the ordered quantities")?;
        let agreement = read_one(&ctx, id, &["line_ids", "purchase_ids"]).await?;
        let line_ids = ids_of(&agreement, "line_ids");
        let order_ids = ids_of(&agreement, "purchase_ids");

        // only what was actually bought counts: a quotation is not an
        // order, and a cancelled one is not either
        let mut order_line_ids: Vec<i64> = Vec::new();
        if !order_ids.is_empty() {
            let orders = ctx
                .registry
                .read(ctx.pool, "purchase.order", &order_ids, &["state", "order_line"])
                .await?;
            for order in &orders {
                if text(order, "state") != "purchase" {
                    continue;
                }
                order_line_ids.extend(ids_of(order, "order_line"));
            }
        }
        let mut ordered: HashMap<i64, f64> = HashMap::new();
        if !order_line_ids.is_empty() {
            let order_lines = ctx
                .registry
                .read(
                    ctx.pool,
                    "purchase.order.line",
                    &order_line_ids,
                    &["product_id", "product_qty"],
                )
                .await?;
            for line in &order_lines {
                let Some(product) = line.get("product_id").and_then(first_id) else {
                    continue;
                };
                *ordered.entry(product).or_insert(0.0) += number(line, "product_qty");
            }
        }

        let mut answer = Map::new();
        if !line_ids.is_empty() {
            let lines = ctx
                .registry
                .read(
                    ctx.pool,
                    "purchase.requisition.line",
                    &line_ids,
                    &["product_id"],
                )
                .await?;
            let mut counted: HashSet<i64> = HashSet::new();
            for line in &lines {
                let Some(line_id) = line.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let product = line.get("product_id").and_then(first_id);
                let quantity = match product {
                    Some(product) if counted.insert(product) => {
                        ordered.get(&product).copied().unwrap_or(0.0)
                    }
                    _ => 0.0,
                };
                answer.insert(line_id.to_string(), json!(quantity));
            }
        }
        Ok(Value::Object(answer))
    })
}

// ── the pieces the buttons share ────────────────────────────────────

async fn read_one(
    ctx: &MethodCtx<'_>,
    id: i64,
    fields: &[&str],
) -> Result<Map<String, Value>, RusdooError> {
    ctx.registry
        .read(ctx.pool, MODEL, &[id], fields)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation(format!("agreement {id} does not exist")))
}

async fn set_state(ctx: &MethodCtx<'_>, ids: &[i64], state: &str) -> Result<Value, RusdooError> {
    ctx.registry
        .write_as(ctx.pool, ctx.uid, MODEL, ids, vec![("state", json!(state))])
        .await?;
    Ok(json!(true))
}

/// The requests for quotation among `orders` that nobody has decided on
/// yet.
async fn open_orders(ctx: &MethodCtx<'_>, orders: &[i64]) -> Result<Vec<i64>, RusdooError> {
    if orders.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "purchase.order", orders, &["state"])
        .await?;
    Ok(rows
        .iter()
        .filter(|row| OPEN_RFQ_STATES.contains(&text(row, "state")))
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .collect())
}
