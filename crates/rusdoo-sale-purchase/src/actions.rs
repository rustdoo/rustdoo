//! The two stat buttons: from a sale to the purchases it raised, and
//! back from a purchase to the sales behind it.
//!
//! Port of `sale_order.action_view_purchase_orders` and
//! `purchase_order.action_view_sale_orders`, including their shape: one
//! document opens as a form, several open as a list.

use crate::shared::{first_id, id_list, read_one, text};
use rusdoo_core::RusdooError;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// The action that opens `ids` of `model`: a form when there is one, a
/// list when there are several — Odoo's own answer, and the reason the
/// stat button feels different depending on what is behind it.
fn open(model: &str, name: &str, ids: &[i64]) -> Value {
    if let [only] = ids {
        return json!({
            "type": "ir.actions.act_window",
            "name": name,
            "res_model": model,
            "res_id": only,
            "views": [[false, "form"]],
            "target": "current",
        });
    }
    json!({
        "type": "ir.actions.act_window",
        "name": name,
        "res_model": model,
        "view_mode": "list,form",
        "domain": [["id", "in", ids]],
        "target": "current",
    })
}

/// The purchases raised by a sale order, port of `_get_purchase_orders`.
pub(crate) async fn purchase_orders_of(
    ctx: &MethodCtx<'_>,
    order: i64,
) -> Result<Vec<i64>, RusdooError> {
    let row = read_one(ctx, "sale.order", order, &["order_line"]).await?;
    let line_ids = id_list(&row, "order_line");
    if line_ids.is_empty() {
        return Ok(Vec::new());
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "sale.order.line",
            &line_ids,
            &["purchase_order_ids"],
        )
        .await?;
    Ok(crate::shared::deduplicated(
        lines
            .iter()
            .flat_map(|line| id_list(line, "purchase_order_ids")),
    ))
}

/// The sale orders behind a purchase, port of `_get_sale_orders`.
pub(crate) async fn sale_orders_of(
    ctx: &MethodCtx<'_>,
    purchase: i64,
) -> Result<Vec<i64>, RusdooError> {
    let row = read_one(ctx, "purchase.order", purchase, &["order_line"]).await?;
    let line_ids = id_list(&row, "order_line");
    if line_ids.is_empty() {
        return Ok(Vec::new());
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "purchase.order.line",
            &line_ids,
            &["sale_order_id"],
        )
        .await?;
    Ok(crate::shared::deduplicated(
        lines
            .iter()
            .filter_map(|line| line.get("sale_order_id").and_then(first_id)),
    ))
}

/// `action_view_purchase_orders` — what this sale had bought for it.
pub(crate) fn action_view_purchase_orders<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [order] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "open the purchases of one sale order at a time".into(),
            ));
        };
        let row = read_one(&ctx, "sale.order", order, &["name"]).await?;
        let purchases = purchase_orders_of(&ctx, order).await?;
        Ok(open(
            "purchase.order",
            &format!("Purchase orders generated from {}", text(&row, "name")),
            &purchases,
        ))
    })
}

/// `action_view_sale_orders` — which sales asked for this purchase.
pub(crate) fn action_view_sale_orders<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [purchase] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "open the sales of one purchase order at a time".into(),
            ));
        };
        let row = read_one(&ctx, "purchase.order", purchase, &["name"]).await?;
        let sales = sale_orders_of(&ctx, purchase).await?;
        Ok(open(
            "sale.order",
            &format!("Source sale orders of {}", text(&row, "name")),
            &sales,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_document_opens_as_a_form_and_several_as_a_list() {
        let single = open("purchase.order", "Purchases", &[7]);
        assert_eq!(single["res_id"], json!(7));
        assert_eq!(single["views"], json!([[false, "form"]]));

        let many = open("purchase.order", "Purchases", &[7, 9]);
        assert_eq!(many["view_mode"], "list,form");
        assert_eq!(many["domain"], json!([["id", "in", [7, 9]]]));
        // and a sale that bought nothing opens an empty list, not a form
        // onto a record that does not exist
        let none = open("purchase.order", "Purchases", &[]);
        assert_eq!(none["domain"], json!([["id", "in", []]]));
    }
}
