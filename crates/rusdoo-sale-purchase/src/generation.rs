//! Raising the purchase: which vendor, into which request for quotation,
//! and what happens to it when the sold quantity moves afterwards.
//!
//! Port of `sale_order_line.py`'s `_purchase_service_generation`,
//! `_purchase_service_create` and the two halves of its `write`.

use crate::notices;
use crate::shared::{deduplicated, first_id, id_list, number, post_notice, purchase_lines_of, read_one, text};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};

/// How close two quantities have to be to count as the same.
///
/// Odoo asks `decimal.precision` for "Product Unit" and compares with
/// `float_compare`. There is no precision model in this port; what
/// exists is the column, `numeric(16,2)`, so half of its last digit is
/// the finest difference a stored quantity can express.
const QUANTITY_EPSILON: f64 = 0.005;

/// The vendor a service is bought from, as `product.supplierinfo` holds
/// it — Odoo's `_select_seller` result, reduced to what the port reads.
pub(crate) struct Seller {
    pub partner: i64,
    pub price: f64,
    /// days the vendor takes, which is how far out the receipt is planned
    pub delay: i64,
}

/// One sale line about to raise a purchase, and how much of it.
pub(crate) struct Wanted {
    pub line: i64,
    pub order: i64,
    pub order_name: String,
    pub company: Option<i64>,
    pub product: i64,
    pub product_name: String,
    pub quantity: f64,
}

/// Which way a quantity moved, once rounding is out of the way.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Move {
    Increased,
    Decreased,
    Unchanged,
}

pub(crate) fn quantity_move(old: f64, new: f64) -> Move {
    if (new - old).abs() < QUANTITY_EPSILON {
        Move::Unchanged
    } else if new > old {
        Move::Increased
    } else {
        Move::Decreased
    }
}

/// The purchase's source documents once this sale order is among them,
/// or `None` when it already was.
///
/// Odoo splits the field on `", "` and joins it back with the new name.
/// It splits an empty `origin` into `[""]` too, so the first sale order
/// to write it leaves a leading comma behind; this drops the empties,
/// which is the same list without the artefact.
pub(crate) fn origin_with(existing: &str, order_name: &str) -> Option<String> {
    let mut origins: Vec<&str> = existing
        .split(", ")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if origins.contains(&order_name) {
        return None;
    }
    origins.push(order_name);
    Some(origins.join(", "))
}

/// The vendor for `quantity` of `product`, port of `_select_seller`.
///
/// The order is the model's: lowest `sequence`, then the smallest
/// minimum quantity — Odoo's own tie-break, so a vendor who sells from
/// one unit wins over one who only sells by the hundred. Left out: the
/// validity dates and the currency, neither of which this port's
/// supplier info carries.
pub(crate) async fn select_seller(
    ctx: &MethodCtx<'_>,
    product: i64,
    quantity: f64,
    partner: Option<i64>,
) -> Result<Option<Seller>, RusdooError> {
    let mut terms = vec![
        json!(["product_id", "=", product]),
        json!(["min_qty", "<=", quantity]),
    ];
    if let Some(partner) = partner {
        terms.push(json!(["partner_id", "=", partner]));
    }
    let domain = parse_domain(&Value::Array(terms))?;
    let found = ctx
        .registry
        .search(
            ctx.pool,
            "product.supplierinfo",
            &domain,
            &SearchOptions {
                limit: Some(1),
                ..SearchOptions::default()
            },
        )
        .await?;
    let Some(id) = found.first().copied() else {
        return Ok(None);
    };
    let row = read_one(ctx, "product.supplierinfo", id, &["partner_id", "price", "delay"]).await?;
    let partner = row
        .get("partner_id")
        .and_then(first_id)
        .ok_or_else(|| RusdooError::Validation(format!("supplier info {id} names no vendor")))?;
    Ok(Some(Seller {
        partner,
        price: number(&row, "price"),
        delay: number(&row, "delay").round() as i64,
    }))
}

/// When the vendor says the service will be done, counted from today.
///
/// Odoo dates the purchase at `commitment_date - delay` so the work
/// lands on the day the customer was promised, and plans the line for
/// `date_order + delay`. This port's `sale.order` has no commitment
/// date; backdating the request for quotation would be all that survived
/// of that intent, and a document dated in the past is harder to explain
/// than one planned `delay` days out. So the promise is read forwards:
/// ordered today, expected in `delay` days.
fn planned_date(delay: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(delay))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// The draft request for quotation this sale order already has open with
/// this vendor, port of `_purchase_service_match_purchase_order`.
///
/// Odoo matches on partner, draft state, company and the sale order the
/// purchase lines came from — the last one is what keeps two customers'
/// services out of one request for quotation. The port asks the same
/// question from the link it has: the purchase lines raised by this
/// order's lines, and the orders behind them.
async fn matching_purchase_order(
    ctx: &MethodCtx<'_>,
    sale_order: i64,
    partner: i64,
) -> Result<Option<i64>, RusdooError> {
    let order = read_one(ctx, "sale.order", sale_order, &["order_line"]).await?;
    let sale_lines = id_list(&order, "order_line");
    let lines = purchase_lines_of(ctx, &sale_lines, &["order_id"]).await?;
    let mut candidates = deduplicated(lines.iter().filter_map(|line| {
        line.get("order_id").and_then(first_id)
    }));
    // the lowest number first, like Odoo's `order='order_id'`: which of
    // two open requests receives the line must not depend on the order
    // the database happened to answer in
    candidates.sort_unstable();
    if candidates.is_empty() {
        return Ok(None);
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "purchase.order", &candidates, &["partner_id", "state"])
        .await?;
    Ok(rows
        .iter()
        .find(|row| {
            row.get("partner_id").and_then(first_id) == Some(partner)
                && row.get("state").and_then(Value::as_str) == Some("draft")
        })
        .and_then(|row| row.get("id").and_then(Value::as_i64)))
}

/// A new request for quotation to this vendor, port of
/// `_purchase_service_prepare_order_values`.
///
/// Left out for want of the fields: the vendor's reference and payment
/// term, its purchase currency, and the fiscal position — none of them
/// exists on this port's `res.partner`.
async fn create_purchase_order(
    ctx: &MethodCtx<'_>,
    wanted: &Wanted,
    seller: &Seller,
) -> Result<i64, RusdooError> {
    let mut values = vec![
        ("partner_id", json!(seller.partner)),
        ("origin", json!(wanted.order_name)),
        ("date_planned", json!(planned_date(seller.delay))),
    ];
    // the company of the sale, not of whoever pressed the button: a
    // purchase raised by one company's order belongs to that company
    if let Some(company) = wanted.company {
        values.push(("company_id", json!(company)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, "purchase.order", values)
        .await
}

/// Add the sale order to a purchase's source documents.
async fn add_origin(ctx: &MethodCtx<'_>, purchase: i64, order_name: &str) -> Result<(), RusdooError> {
    let row = read_one(ctx, "purchase.order", purchase, &["origin"]).await?;
    let Some(origin) = origin_with(&text(&row, "origin"), order_name) else {
        return Ok(());
    };
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "purchase.order",
            &[purchase],
            vec![("origin", json!(origin))],
        )
        .await
}

/// Raise the purchases for `wanted`, port of `_purchase_service_create`.
///
/// Two lines going to the same vendor land in one request for quotation,
/// which is the map Odoo keeps for the length of the call: a vendor who
/// receives two documents for one order has to answer twice, for no
/// reason the customer would recognise.
pub(crate) async fn raise_purchases(
    ctx: &MethodCtx<'_>,
    wanted: &[Wanted],
) -> Result<Vec<i64>, RusdooError> {
    let mut per_vendor: Vec<(i64, i64)> = Vec::new();
    let mut raised: Vec<i64> = Vec::new();
    for line in wanted {
        let seller = select_seller(ctx, line.product, line.quantity, None)
            .await?
            .ok_or_else(|| {
                RusdooError::Validation(format!(
                    "there is no vendor for the product {}: define one before selling it \
                     as a subcontracted service",
                    line.product_name
                ))
            })?;
        let known = per_vendor
            .iter()
            .find(|(partner, _)| *partner == seller.partner)
            .map(|(_, purchase)| *purchase);
        let purchase = match known {
            Some(purchase) => purchase,
            None => {
                let purchase = match matching_purchase_order(ctx, line.order, seller.partner).await?
                {
                    Some(purchase) => purchase,
                    None => create_purchase_order(ctx, line, &seller).await?,
                };
                per_vendor.push((seller.partner, purchase));
                purchase
            }
        };
        add_origin(ctx, purchase, &line.order_name).await?;
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "purchase.order.line",
                vec![
                    ("order_id", json!(purchase)),
                    ("product_id", json!(line.product)),
                    ("name", json!(line.product_name)),
                    ("product_qty", json!(line.quantity)),
                    ("price_unit", json!(seller.price)),
                    ("sale_line_id", json!(line.line)),
                ],
            )
            .await?;
        if !raised.contains(&purchase) {
            raised.push(purchase);
        }
    }
    Ok(raised)
}

/// `action_generate_purchase_orders` — the sale raises its purchases.
///
/// Odoo runs this inside `sale.order._action_confirm`, where confirming
/// is the whole trigger. This framework cannot extend a method another
/// module registered, so the step is a method of its own: it refuses an
/// order that is not confirmed, and it is safe to call twice — a line
/// that already raised a purchase does not raise a second one, which is
/// the same guard Odoo needs for the cancel-and-reconfirm case.
pub(crate) fn action_generate_purchase_orders<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "say which sale order should raise its purchases".into(),
            ));
        }
        let orders = ctx
            .registry
            .read(
                ctx.pool,
                "sale.order",
                &ctx.ids,
                &["name", "state", "company_id", "order_line"],
            )
            .await?;
        let mut wanted: Vec<Wanted> = Vec::new();
        for order in &orders {
            let name = text(order, "name");
            let state = order.get("state").and_then(Value::as_str).unwrap_or("draft");
            if state != "sale" {
                return Err(RusdooError::Validation(format!(
                    "order {name} is {state:?}: confirm it before it raises a purchase"
                )));
            }
            let order_id = order
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| RusdooError::Validation("the order is gone".into()))?;
            let company = order.get("company_id").and_then(first_id);
            let line_ids = id_list(order, "order_line");
            wanted.extend(lines_to_buy(&ctx, order_id, &name, company, &line_ids).await?);
        }
        let raised = raise_purchases(&ctx, &wanted).await?;
        Ok(json!({ "purchase_order_ids": raised }))
    })
}

/// The lines of one order that should raise a purchase, read in a batch:
/// the lines, then the products they sell, whatever the number of rows.
async fn lines_to_buy(
    ctx: &MethodCtx<'_>,
    order: i64,
    order_name: &str,
    company: Option<i64>,
    line_ids: &[i64],
) -> Result<Vec<Wanted>, RusdooError> {
    if line_ids.is_empty() {
        return Ok(Vec::new());
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "sale.order.line",
            line_ids,
            &["product_id", "product_uom_qty", "purchase_line_count"],
        )
        .await?;
    let products = deduplicated(
        lines
            .iter()
            .filter_map(|line| line.get("product_id").and_then(first_id)),
    );
    if products.is_empty() {
        return Ok(Vec::new());
    }
    let catalogue = ctx
        .registry
        .read(
            ctx.pool,
            "product.product",
            &products,
            &["name", "service_to_purchase"],
        )
        .await?;
    let mut wanted = Vec::new();
    for line in &lines {
        let Some(product) = line.get("product_id").and_then(first_id) else {
            continue;
        };
        // a line that already raised a purchase does not raise another:
        // an order cancelled and confirmed again would otherwise order
        // the service twice (`_purchase_service_generation`)
        if number(line, "purchase_line_count") > 0.0 {
            continue;
        }
        let Some(sold) = catalogue
            .iter()
            .find(|row| row.get("id").and_then(Value::as_i64) == Some(product))
        else {
            continue;
        };
        if !sold
            .get("service_to_purchase")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        wanted.push(Wanted {
            line: line
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| RusdooError::Validation("a sale line has no id".into()))?,
            order,
            order_name: order_name.to_string(),
            company,
            product,
            product_name: text(sold, "name"),
            quantity: number(line, "product_uom_qty"),
        });
    }
    Ok(wanted)
}

/// `action_update_service_qty(quantity)` — sell more, or less, of a
/// service somebody else performs.
///
/// Odoo does this from `sale.order.line.write`: writing the quantity is
/// what carries the consequence. The port cannot hook a write, so this
/// is the method that writes it — and the consequence is the one Odoo
/// draws. More of it: the draft request for quotation is raised to the
/// new quantity, and when it was already confirmed a second one is
/// raised for the difference, because a confirmed order is a promise the
/// vendor is already keeping. Less of it: nothing is touched and the
/// buyer is told, since only they know whether it can still be trimmed.
pub(crate) fn action_update_service_qty<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [line_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "change the quantity of one line at a time".into(),
            ));
        };
        let quantity = wanted_quantity(&ctx.rest, kwargs)?;
        let line = read_one(
            &ctx,
            "sale.order.line",
            line_id,
            &[
                "order_id",
                "product_id",
                "product_uom_qty",
                "purchase_line_count",
            ],
        )
        .await?;
        let old = number(&line, "product_uom_qty");
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "sale.order.line",
                &[line_id],
                vec![("product_uom_qty", json!(quantity))],
            )
            .await?;
        // a line that never raised a purchase is an ordinary sale line:
        // the write above is the whole of it
        let raised = number(&line, "purchase_line_count") > 0.0;
        let movement = quantity_move(old, quantity);
        if !raised || movement == Move::Unchanged {
            return Ok(json!(true));
        }
        match movement {
            Move::Increased => increase_purchase(&ctx, line_id, old, quantity).await,
            _ => warn_of_decrease(&ctx, line_id, old, quantity).await,
        }
    })
}

/// The quantity the caller asked for: the first positional argument, or
/// `quantity` in the keywords — the two shapes a client sends.
fn wanted_quantity(rest: &[Value], kwargs: &Map<String, Value>) -> Result<f64, RusdooError> {
    let quantity = kwargs
        .get("quantity")
        .or_else(|| kwargs.get("product_uom_qty"))
        .or_else(|| rest.first())
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .ok_or_else(|| {
            RusdooError::Validation("say the new quantity: pass it as the first argument".into())
        })?;
    if quantity <= 0.0 {
        return Err(RusdooError::Validation(
            "a line's quantity must be greater than zero".into(),
        ));
    }
    Ok(quantity)
}

/// Port of `_purchase_increase_ordered_qty`.
async fn increase_purchase(
    ctx: &MethodCtx<'_>,
    line_id: i64,
    old: f64,
    quantity: f64,
) -> Result<Value, RusdooError> {
    // the last purchase line this sale line raised. Odoo orders by
    // `create_date DESC`; the id says the same thing and stays decided
    // when two lines are created inside one second
    let lines = purchase_lines_of(ctx, &[line_id], &["order_id"]).await?;
    let Some(last) = lines.last() else {
        return Ok(json!(true));
    };
    let purchase = last
        .get("order_id")
        .and_then(first_id)
        .ok_or_else(|| RusdooError::Validation("a purchase line has no order".into()))?;
    let purchase_line = last
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| RusdooError::Validation("a purchase line has no id".into()))?;
    let order = read_one(ctx, "purchase.order", purchase, &["state"]).await?;
    let state = order.get("state").and_then(Value::as_str).unwrap_or("draft");
    if state == "draft" {
        // still a request for quotation: it is simply asked for more
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "purchase.order.line",
                &[purchase_line],
                vec![("product_qty", json!(quantity))],
            )
            .await?;
        return Ok(json!({ "purchase_order_ids": [purchase] }));
    }
    // confirmed or cancelled: what was promised stays promised, and only
    // the difference is bought again
    let wanted = wanted_from_line(ctx, line_id, quantity - old).await?;
    let raised = raise_purchases(ctx, &[wanted]).await?;
    Ok(json!({ "purchase_order_ids": raised }))
}

/// Port of `_purchase_decrease_ordered_qty`.
async fn warn_of_decrease(
    ctx: &MethodCtx<'_>,
    line_id: i64,
    old: f64,
    quantity: f64,
) -> Result<Value, RusdooError> {
    let line = read_one(
        ctx,
        "sale.order.line",
        line_id,
        &["order_id", "product_id"],
    )
    .await?;
    let order_name = line
        .get("order_id")
        .map(crate::shared::linked_name)
        .unwrap_or_default();
    let product = line
        .get("product_id")
        .map(crate::shared::linked_name)
        .unwrap_or_default();
    let body = notices::quantity_decreased(&order_name, &product, quantity, old);
    let purchases = deduplicated(
        purchase_lines_of(ctx, &[line_id], &["order_id"])
            .await?
            .iter()
            .filter_map(|row| row.get("order_id").and_then(first_id)),
    );
    for purchase in &purchases {
        post_notice(ctx, "purchase.order", *purchase, body.clone()).await?;
    }
    Ok(json!({ "purchase_order_ids": purchases }))
}

/// One sale line, ready to raise a purchase for `quantity` of it.
async fn wanted_from_line(
    ctx: &MethodCtx<'_>,
    line_id: i64,
    quantity: f64,
) -> Result<Wanted, RusdooError> {
    let line = read_one(ctx, "sale.order.line", line_id, &["order_id", "product_id"]).await?;
    let order = line
        .get("order_id")
        .and_then(first_id)
        .ok_or_else(|| RusdooError::Validation("the sale line has no order".into()))?;
    let product = line
        .get("product_id")
        .and_then(first_id)
        .ok_or_else(|| RusdooError::Validation("the sale line sells no product".into()))?;
    let order_row = read_one(ctx, "sale.order", order, &["name", "company_id"]).await?;
    let product_row = read_one(ctx, "product.product", product, &["name"]).await?;
    Ok(Wanted {
        line: line_id,
        order,
        order_name: text(&order_row, "name"),
        company: order_row.get("company_id").and_then(first_id),
        product,
        product_name: text(&product_row, "name"),
        quantity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quantity_that_moved_by_less_than_a_cent_did_not_move() {
        assert_eq!(quantity_move(4.0, 4.001), Move::Unchanged);
        assert_eq!(quantity_move(4.0, 16.0), Move::Increased);
        assert_eq!(quantity_move(16.0, 13.0), Move::Decreased);
    }

    #[test]
    fn a_purchase_names_every_order_that_fed_it_once() {
        assert_eq!(origin_with("", "SO00001").as_deref(), Some("SO00001"));
        assert_eq!(
            origin_with("SO00001", "SO00002").as_deref(),
            Some("SO00001, SO00002")
        );
        // the same order twice adds nothing: nothing is written
        assert_eq!(origin_with("SO00001, SO00002", "SO00001"), None);
    }

    #[test]
    fn a_quantity_is_read_from_the_argument_or_from_the_keyword() {
        let kwargs = Map::new();
        assert_eq!(wanted_quantity(&[json!(3.5)], &kwargs).unwrap(), 3.5);
        let mut kwargs = Map::new();
        kwargs.insert("quantity".into(), json!(7));
        assert_eq!(wanted_quantity(&[], &kwargs).unwrap(), 7.0);
        // and a call that says nothing is a call nobody can act on
        assert!(wanted_quantity(&[], &Map::new()).is_err());
        // zero of something is not a sale, like the sale line's own rule
        assert!(wanted_quantity(&[json!(0)], &Map::new()).is_err());
    }
}
