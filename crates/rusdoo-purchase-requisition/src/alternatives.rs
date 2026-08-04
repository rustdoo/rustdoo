//! The call for tender: the same need sent to several vendors, and the
//! comparison of what came back.
//!
//! Port of `PurchaseOrderGroup`, of the `purchase.order` half of
//! `models/purchase.py`, and of both wizards.

use crate::{first_id, ids_of, number, text, OPEN_RFQ_STATES};
use rusdoo_core::RusdooError;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};

const ORDER: &str = "purchase.order";
const GROUP: &str = "purchase.order.group";
const CREATE_WIZARD: &str = "purchase.requisition.create.alternative";
const WARNING_WIZARD: &str = "purchase.requisition.alternative.warning";

/// The context flag the warning wizard sets so that confirming from it
/// does not open the same warning again. Odoo's own name.
const SKIP_ALTERNATIVE_CHECK: &str = "skip_alternative_check";

/// `action_create_alternative` — "ask another vendor for the same
/// thing".
pub(crate) fn action_create_alternative<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [order] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "an alternative is created for one request for quotation at a time".into(),
            ));
        };
        // the dialog is born pointing at the order it came from, the way
        // Odoo passes `default_origin_po_id` through the context: a
        // dialog that opens pointing at nothing has nothing to copy
        let wizard = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                CREATE_WIZARD,
                vec![("origin_po_id", json!(order))],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Create alternative",
            "res_model": CREATE_WIZARD,
            "res_id": wizard,
            "views": [[false, "form"]],
            "target": "new",
        }))
    })
}

/// The dialog's button: one alternative request for quotation per vendor
/// chosen, all of them in one group with the original.
///
/// Odoo does not copy the price onto the alternative, and neither does
/// this: the whole point of asking another vendor is to find out what
/// *they* charge.
pub(crate) fn wizard_create_alternative<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [wizard_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation("the wizard is gone".into()));
        };
        let wizard = ctx
            .registry
            .read(
                ctx.pool,
                CREATE_WIZARD,
                &[wizard_id],
                &["origin_po_id", "partner_ids", "copy_products"],
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| RusdooError::Validation("the wizard is gone".into()))?;
        let origin = wizard
            .get("origin_po_id")
            .and_then(first_id)
            .ok_or_else(|| {
                RusdooError::Validation("the wizard points at no request for quotation".into())
            })?;
        let partners = ids_of(&wizard, "partner_ids");
        if partners.is_empty() {
            return Err(RusdooError::Validation(
                "choose at least one vendor to ask".into(),
            ));
        }
        let copy_products = wizard
            .get("copy_products")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let source = ctx
            .registry
            .read(
                ctx.pool,
                ORDER,
                &[origin],
                &["date_order", "company_id", "order_line"],
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                RusdooError::Validation("the request for quotation this came from is gone".into())
            })?;

        let mut order_lines: Vec<Value> = Vec::new();
        if copy_products {
            let line_ids = ids_of(&source, "order_line");
            if !line_ids.is_empty() {
                let lines = ctx
                    .registry
                    .read(
                        ctx.pool,
                        "purchase.order.line",
                        &line_ids,
                        &["product_id", "name", "product_qty", "sequence"],
                    )
                    .await?;
                order_lines = lines
                    .iter()
                    .map(|line| {
                        json!([0, 0, {
                            "product_id": line.get("product_id").and_then(first_id),
                            "name": line.get("name").cloned().unwrap_or(Value::Null),
                            "product_qty": number(line, "product_qty"),
                            "sequence": line.get("sequence").cloned().unwrap_or_else(|| json!(10)),
                        }])
                    })
                    .collect();
            }
        }

        let mut created: Vec<i64> = Vec::with_capacity(partners.len());
        for partner in &partners {
            let mut values: Vec<(&str, Value)> = vec![
                ("partner_id", json!(partner)),
                ("order_line", Value::Array(order_lines.clone())),
            ];
            if let Some(date_order) = source.get("date_order").and_then(Value::as_str) {
                values.push(("date_order", json!(date_order)));
            }
            if let Some(company) = source.get("company_id").and_then(first_id) {
                values.push(("company_id", json!(company)));
            }
            created.push(
                ctx.registry
                    .create_as(ctx.pool, ctx.uid, ORDER, values)
                    .await?,
            );
        }
        link_into_group(&ctx, origin, &created).await?;

        // one alternative opens straight away; several open as a list,
        // which is what the buyer is going to compare anyway
        if let [only] = created[..] {
            return Ok(json!({
                "type": "ir.actions.act_window",
                "res_model": ORDER,
                "res_id": only,
                "views": [[false, "form"]],
                "target": "current",
            }));
        }
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Alternative Purchase Orders",
            "res_model": ORDER,
            "view_mode": "list,form",
            "domain": [["id", "in", created]],
            "target": "current",
        }))
    })
}

/// `button_confirm` — confirm, but not while a rival offer is still
/// open.
///
/// Confirming one quotation of a tender and forgetting the others is how
/// the same goods get bought twice, so Odoo stops and asks. The dialog
/// answers back with `skip_alternative_check`, and the second pass goes
/// through.
///
/// The state change itself repeats what `purchase`'s own `action_confirm`
/// does, because this registry has no way to call another module's
/// method — see the report.
pub(crate) fn button_confirm<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "confirming needs at least one request for quotation".into(),
            ));
        }
        let asked = ctx
            .context
            .get(SKIP_ALTERNATIVE_CHECK)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !asked {
            let alternatives = open_alternatives(&ctx, &ctx.ids).await?;
            if !alternatives.is_empty() {
                let wizard = ctx
                    .registry
                    .create_as(
                        ctx.pool,
                        ctx.uid,
                        WARNING_WIZARD,
                        vec![
                            ("po_ids", json!([[6, 0, ctx.ids]])),
                            ("alternative_po_ids", json!([[6, 0, alternatives]])),
                        ],
                    )
                    .await?;
                return Ok(json!({
                    "type": "ir.actions.act_window",
                    "name": "What about the alternative Requests for Quotations?",
                    "res_model": WARNING_WIZARD,
                    "res_id": wizard,
                    "views": [[false, "form"]],
                    "target": "new",
                }));
            }
        }
        confirm_orders(&ctx, &ctx.ids).await
    })
}

/// The dialog's first answer: leave the other offers alone.
pub(crate) fn action_keep_alternatives<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (orders, _) = warning_wizard(&ctx).await?;
        confirm_orders(&ctx, &orders).await
    })
}

/// The dialog's second answer: the tender is decided, drop the rest.
///
/// An order that is being confirmed is never cancelled here even if it
/// somehow ended up on both lists — Odoo guards the same way, and for
/// the same reason: the two lists come from a form somebody could have
/// edited.
pub(crate) fn action_cancel_alternatives<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let (orders, alternatives) = warning_wizard(&ctx).await?;
        let losing: Vec<i64> = alternatives
            .into_iter()
            .filter(|id| !orders.contains(id))
            .collect();
        let cancellable = still_open(&ctx, &losing).await?;
        if !cancellable.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    ORDER,
                    &cancellable,
                    vec![("state", json!("cancel"))],
                )
                .await?;
        }
        confirm_orders(&ctx, &orders).await
    })
}

/// `action_compare_alternative_lines` — the side-by-side list.
pub(crate) fn action_compare_alternative_lines<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [order] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "comparing starts from one request for quotation".into(),
            ));
        };
        let orders = group_members(&ctx, &[order]).await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Compare Order Lines",
            "res_model": "purchase.order.line",
            "view_mode": "list",
            "domain": [["order_id", "in", orders]],
            // grouped by product, which is the only way the offers line
            // up next to each other
            "context": {"search_default_groupby_product": true, "purchase_order_id": order},
            "target": "current",
        }))
    })
}

/// `get_tender_best_lines` — which offer wins, per product.
///
/// Two answers, not Odoo's three: the cheapest total for the quantity
/// asked, and the cheapest per unit — which differ whenever two vendors
/// quoted different quantities. Odoo's third answer, the earliest
/// delivery, needs a date on the order line; the port's
/// `purchase.order.line` has none, and guessing from the order's date
/// would rank vendors by when the quotation was written.
///
/// Ties are kept rather than broken: two vendors at the same price are
/// both best, and the buyer decides on something the numbers do not say.
pub(crate) fn get_tender_best_lines<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "comparing starts from at least one request for quotation".into(),
            ));
        }
        let orders = group_members(&ctx, &ctx.ids).await?;
        let rows = ctx
            .registry
            .read(ctx.pool, ORDER, &orders, &["state", "order_line"])
            .await?;
        let mut line_ids: Vec<i64> = Vec::new();
        for row in &rows {
            // a cancelled offer is out of the running; Odoo also skips
            // lines already bought, which here means the whole order
            if text(row, "state") == "cancel" {
                continue;
            }
            line_ids.extend(ids_of(row, "order_line"));
        }
        let mut best_total: HashMap<i64, (f64, BTreeSet<i64>)> = HashMap::new();
        let mut best_unit: HashMap<i64, (f64, BTreeSet<i64>)> = HashMap::new();
        if !line_ids.is_empty() {
            let lines = ctx
                .registry
                .read(
                    ctx.pool,
                    "purchase.order.line",
                    &line_ids,
                    &["product_id", "product_qty", "price_subtotal"],
                )
                .await?;
            for line in &lines {
                let (Some(id), Some(product)) = (
                    line.get("id").and_then(Value::as_i64),
                    line.get("product_id").and_then(first_id),
                ) else {
                    continue;
                };
                let quantity = number(line, "product_qty");
                let total = number(line, "price_subtotal");
                // a line nobody priced is not an offer
                if quantity <= 0.0 || total <= 0.0 {
                    continue;
                }
                remember(&mut best_total, product, total, id);
                remember(&mut best_unit, product, total / quantity, id);
            }
        }
        Ok(json!({
            "best_price_ids": winners(&best_total),
            "best_price_unit_ids": winners(&best_unit),
        }))
    })
}

/// `action_remove_from_group` — this offer is no longer part of the
/// tender.
///
/// Port of `PurchaseOrderGroup.write`'s self-implode: a group of one is
/// not a comparison, so it goes, and the order left behind stops
/// claiming to have alternatives.
pub(crate) fn action_remove_from_group<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "removing needs at least one request for quotation".into(),
            ));
        }
        let rows = ctx
            .registry
            .read(ctx.pool, ORDER, &ctx.ids, &["purchase_group_id"])
            .await?;
        let groups: BTreeSet<i64> = rows
            .iter()
            .filter_map(|row| row.get("purchase_group_id").and_then(first_id))
            .collect();
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                ORDER,
                &ctx.ids,
                vec![("purchase_group_id", Value::Null)],
            )
            .await?;
        for group in groups {
            dissolve_if_alone(&ctx, group).await?;
        }
        Ok(json!(true))
    })
}

// ── the pieces the buttons share ────────────────────────────────────

/// Keep `id` if `value` ties the best seen for `product`, replace the
/// winners if it beats them.
fn remember(best: &mut HashMap<i64, (f64, BTreeSet<i64>)>, product: i64, value: f64, id: i64) {
    match best.get_mut(&product) {
        None => {
            best.insert(product, (value, BTreeSet::from([id])));
        }
        Some((current, ids)) if value < *current => {
            *current = value;
            *ids = BTreeSet::from([id]);
        }
        Some((current, ids)) if (value - *current).abs() < f64::EPSILON => {
            ids.insert(id);
        }
        Some(_) => {}
    }
}

fn winners(best: &HashMap<i64, (f64, BTreeSet<i64>)>) -> Vec<i64> {
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for (_, lines) in best.values() {
        ids.extend(lines.iter().copied());
    }
    ids.into_iter().collect()
}

/// Every order in the groups `ids` belong to, `ids` included.
///
/// Read through `alternative_po_ids`, which is the group's membership
/// seen from an order — the same field the form shows.
async fn group_members(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Vec<i64>, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, ORDER, ids, &["alternative_po_ids"])
        .await?;
    let mut members: BTreeSet<i64> = ids.iter().copied().collect();
    for row in &rows {
        members.extend(ids_of(row, "alternative_po_ids"));
    }
    Ok(members.into_iter().collect())
}

/// The rival offers of `ids` that nobody has decided on yet.
async fn open_alternatives(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Vec<i64>, RusdooError> {
    let others: Vec<i64> = group_members(ctx, ids)
        .await?
        .into_iter()
        .filter(|id| !ids.contains(id))
        .collect();
    still_open(ctx, &others).await
}

/// Which of `ids` are still requests for quotation.
async fn still_open(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Vec<i64>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx.registry.read(ctx.pool, ORDER, ids, &["state"]).await?;
    Ok(rows
        .iter()
        .filter(|row| OPEN_RFQ_STATES.contains(&text(row, "state")))
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .collect())
}

/// Put `extra` into `origin`'s group, creating the group if this is the
/// first alternative.
async fn link_into_group(
    ctx: &MethodCtx<'_>,
    origin: i64,
    extra: &[i64],
) -> Result<i64, RusdooError> {
    let existing = ctx
        .registry
        .read(ctx.pool, ORDER, &[origin], &["purchase_group_id"])
        .await?
        .first()
        .and_then(|row| row.get("purchase_group_id"))
        .and_then(first_id);
    let group = match existing {
        Some(group) => group,
        None => {
            ctx.registry
                .create_as(
                    ctx.pool,
                    ctx.uid,
                    GROUP,
                    vec![("order_ids", json!([[6, 0, [origin]]]))],
                )
                .await?
        }
    };
    if !extra.is_empty() {
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                ORDER,
                extra,
                vec![("purchase_group_id", json!(group))],
            )
            .await?;
    }
    Ok(group)
}

/// A group with fewer than two orders left in it is deleted.
async fn dissolve_if_alone(ctx: &MethodCtx<'_>, group: i64) -> Result<(), RusdooError> {
    let left = ctx
        .registry
        .read(ctx.pool, GROUP, &[group], &["order_ids"])
        .await?
        .first()
        .map(|row| ids_of(row, "order_ids"))
        .unwrap_or_default();
    if left.len() > 1 {
        return Ok(());
    }
    ctx.registry.unlink_as(ctx.pool, ctx.uid, GROUP, &[group]).await?;
    Ok(())
}

/// The two lists the warning dialog was opened with.
async fn warning_wizard(ctx: &MethodCtx<'_>) -> Result<(Vec<i64>, Vec<i64>), RusdooError> {
    let [wizard_id] = ctx.ids[..] else {
        return Err(RusdooError::Validation("the wizard is gone".into()));
    };
    let wizard = ctx
        .registry
        .read(
            ctx.pool,
            WARNING_WIZARD,
            &[wizard_id],
            &["po_ids", "alternative_po_ids"],
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation("the wizard is gone".into()))?;
    let orders = ids_of(&wizard, "po_ids");
    if orders.is_empty() {
        return Err(RusdooError::Validation(
            "the dialog points at no request for quotation to confirm".into(),
        ));
    }
    Ok((orders, ids_of(&wizard, "alternative_po_ids")))
}

/// Draft to confirmed, the state change `purchase`'s own confirm makes.
async fn confirm_orders(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Value, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, ORDER, ids, &["name", "state"])
        .await?;
    for row in &rows {
        let state = text(row, "state");
        if state != "draft" {
            let name = text(row, "name");
            return Err(RusdooError::Validation(format!(
                "order {name} is {state:?} and cannot be confirmed"
            )));
        }
    }
    ctx.registry
        .write_as(ctx.pool, ctx.uid, ORDER, ids, vec![("state", json!("purchase"))])
        .await?;
    Ok(json!(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cheapest_offer_wins_and_a_tie_keeps_both() {
        let mut best: HashMap<i64, (f64, BTreeSet<i64>)> = HashMap::new();
        // one offer at 100, one at 80, one more at 80
        remember(&mut best, 1, 100.0, 10);
        remember(&mut best, 1, 80.0, 11);
        remember(&mut best, 1, 80.0, 12);
        assert_eq!(winners(&best), vec![11, 12]);
    }

    #[test]
    fn a_dearer_offer_does_not_displace_the_best() {
        let mut best: HashMap<i64, (f64, BTreeSet<i64>)> = HashMap::new();
        remember(&mut best, 1, 50.0, 10);
        remember(&mut best, 1, 90.0, 11);
        assert_eq!(winners(&best), vec![10]);
    }

    #[test]
    fn each_product_is_compared_on_its_own() {
        let mut best: HashMap<i64, (f64, BTreeSet<i64>)> = HashMap::new();
        remember(&mut best, 1, 10.0, 10);
        remember(&mut best, 2, 900.0, 20);
        // the expensive product's best line is still a winner
        assert_eq!(winners(&best), vec![10, 20]);
    }
}
