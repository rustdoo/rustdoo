//! What each side says when the other one is cancelled.
//!
//! Port of `sale_order._activity_cancel_on_purchase` and
//! `purchase_order._activity_cancel_on_sale`. Odoo runs them from inside
//! `_action_cancel` and `button_cancel`; this framework cannot extend a
//! method another module registered, so each is a method of its own that
//! checks the document really is cancelled before it announces it — a
//! notice about a cancellation that did not happen is worse than none.

use crate::notices::{self, Exception};
use crate::shared::{
    deduplicated, first_id, id_list, linked_name, number, post_notice, purchase_lines_of, text,
};
use rusdoo_core::RusdooError;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// Every id of `model` in `ids`, refusing the ones that are not
/// cancelled.
async fn cancelled_documents(
    ctx: &MethodCtx<'_>,
    model: &str,
    extra: &[&str],
) -> Result<Vec<Map<String, Value>>, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "say which document was cancelled".into(),
        ));
    }
    let mut fields = vec!["name", "state"];
    fields.extend_from_slice(extra);
    let rows = ctx.registry.read(ctx.pool, model, &ctx.ids, &fields).await?;
    for row in &rows {
        let state = row.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "cancel" {
            return Err(RusdooError::Validation(format!(
                "{model} {} is {state:?}, not cancelled: there is nothing to warn about",
                text(row, "name")
            )));
        }
    }
    Ok(rows)
}

/// `action_notify_purchase_of_cancellation` — the cancelled sale tells
/// the purchases it raised.
///
/// One notice per purchase, whatever the number of lines behind it: a
/// buyer who receives four warnings for one order reads none of them.
pub(crate) fn action_notify_purchase_of_cancellation<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let orders = cancelled_documents(&ctx, "sale.order", &["order_line"]).await?;
        let sale_lines: Vec<i64> = orders
            .iter()
            .flat_map(|order| id_list(order, "order_line"))
            .collect();
        let raised = purchase_lines_of(&ctx, &sale_lines, &["order_id", "sale_line_id"]).await?;
        if raised.is_empty() {
            return Ok(json!({ "purchase_order_ids": [] }));
        }
        // a purchase that is already cancelled is not warned: it has no
        // decision left to take
        let purchases = live_documents(
            &ctx,
            "purchase.order",
            &deduplicated(
                raised
                    .iter()
                    .filter_map(|line| line.get("order_id").and_then(first_id)),
            ),
        )
        .await?;
        let lines = read_sale_lines(&ctx, &sale_lines).await?;

        let mut warned: Vec<i64> = Vec::new();
        for purchase in purchases {
            let exceptions: Vec<Exception> = raised
                .iter()
                .filter(|line| line.get("order_id").and_then(first_id) == Some(purchase))
                .filter_map(|line| line.get("sale_line_id").and_then(first_id))
                .filter_map(|sale_line| exception_for(&lines, sale_line))
                .collect();
            if exceptions.is_empty() {
                continue;
            }
            post_notice(
                &ctx,
                "purchase.order",
                purchase,
                notices::sale_cancelled(&exceptions),
            )
            .await?;
            warned.push(purchase);
        }
        Ok(json!({ "purchase_order_ids": warned }))
    })
}

/// `action_notify_sale_of_cancellation` — the cancelled purchase tells
/// the sales that asked for it.
pub(crate) fn action_notify_sale_of_cancellation<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let purchases = cancelled_documents(&ctx, "purchase.order", &["order_line"]).await?;
        let mut exceptions_per_sale: Vec<(i64, Vec<Exception>)> = Vec::new();
        for purchase in &purchases {
            let name = text(purchase, "name");
            let line_ids = id_list(purchase, "order_line");
            if line_ids.is_empty() {
                continue;
            }
            let lines = ctx
                .registry
                .read(
                    ctx.pool,
                    "purchase.order.line",
                    &line_ids,
                    &["sale_order_id", "product_id", "product_qty"],
                )
                .await?;
            for line in &lines {
                // only the lines a sale raised: the rest of the purchase
                // is nobody's promise to a customer
                let Some(sale) = line.get("sale_order_id").and_then(first_id) else {
                    continue;
                };
                let exception = Exception {
                    document: name.clone(),
                    product: line.get("product_id").map(linked_name).unwrap_or_default(),
                    quantity: number(line, "product_qty"),
                };
                match exceptions_per_sale.iter_mut().find(|(id, _)| *id == sale) {
                    Some((_, gathered)) => gathered.push(exception),
                    None => exceptions_per_sale.push((sale, vec![exception])),
                }
            }
        }
        let mut warned: Vec<i64> = Vec::new();
        for (sale, exceptions) in &exceptions_per_sale {
            post_notice(
                &ctx,
                "sale.order",
                *sale,
                notices::purchase_cancelled(exceptions),
            )
            .await?;
            warned.push(*sale);
        }
        Ok(json!({ "sale_order_ids": warned }))
    })
}

/// The documents of `ids` that are not cancelled themselves.
async fn live_documents(
    ctx: &MethodCtx<'_>,
    model: &str,
    ids: &[i64],
) -> Result<Vec<i64>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx.registry.read(ctx.pool, model, ids, &["state"]).await?;
    Ok(rows
        .iter()
        .filter(|row| row.get("state").and_then(Value::as_str) != Some("cancel"))
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .collect())
}

/// The sale lines named by the purchase lines, read once for the batch.
async fn read_sale_lines(
    ctx: &MethodCtx<'_>,
    ids: &[i64],
) -> Result<Vec<Map<String, Value>>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    ctx.registry
        .read(
            ctx.pool,
            "sale.order.line",
            ids,
            &["order_id", "product_id", "product_uom_qty"],
        )
        .await
}

/// What one sale line contributes to the notice: its order's number, the
/// product and how much of it will not be needed after all.
fn exception_for(lines: &[Map<String, Value>], sale_line: i64) -> Option<Exception> {
    let line = lines
        .iter()
        .find(|row| row.get("id").and_then(Value::as_i64) == Some(sale_line))?;
    Some(Exception {
        document: line.get("order_id").map(linked_name).unwrap_or_default(),
        product: line.get("product_id").map(linked_name).unwrap_or_default(),
        quantity: number(line, "product_uom_qty"),
    })
}
