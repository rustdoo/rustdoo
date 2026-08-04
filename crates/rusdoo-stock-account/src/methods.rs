//! What somebody presses, and what it does to the valuation.
//!
//! Odoo does most of this from inside `_action_done`, `create` and
//! `write` overrides. This ORM has no way for one module to wrap a method
//! another module registered — there is no `super()` — so the same work
//! is attached to buttons that say what they do. Where that changes *when*
//! something happens rather than *what*, the comment says so.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use serde_json::{json, Map, Value};

use crate::models::number;
use crate::valuation::{
    fifo_stack, quantity_on_hand, run_average, run_fifo, value_at_standard, CostMethod, Direction,
    Layer, Movement,
};

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "stock.picking",
        "action_valuate",
        Operation::Write,
        action_valuate,
    )?;
    // opening a dialog reads; it is the dialog's own button that writes
    methods.register(
        "stock.move",
        "action_adjust_valuation",
        Operation::Read,
        action_adjust_valuation,
    )?;
    methods.register(
        "product.value",
        "action_apply",
        Operation::Write,
        action_apply,
    )?;
    methods.register(
        "product.product",
        "action_change_standard_price",
        Operation::Write,
        action_change_standard_price,
    )?;
    Ok(())
}

/// The id inside a many2one, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn flag(record: &Map<String, Value>, name: &str) -> bool {
    record
        .get(name)
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

/// Money as it is stored: two decimals. A value carried at full float
/// precision and then rounded on the way into the column would make the
/// running total disagree with the sum of what is on screen.
fn cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A move as the valuation needs it.
struct ValuedMove {
    id: i64,
    product: i64,
    quantity: f64,
    price_unit: f64,
    value: f64,
    direction: Direction,
}

impl ValuedMove {
    fn movement(&self) -> Movement {
        Movement {
            direction: self.direction,
            quantity: self.quantity,
            value: self.value,
        }
    }
}

/// How a product is valued and what it currently costs.
struct Costing {
    method: CostMethod,
    standard_price: f64,
}

/// Read the moves of `ids`, keeping only the ones that crossed the
/// company's boundary.
async fn read_valued_moves(
    ctx: &MethodCtx<'_>,
    ids: &[i64],
) -> Result<Vec<ValuedMove>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "stock.move",
            ids,
            &[
                "product_id",
                "quantity_done",
                "price_unit",
                "value",
                "is_in",
                "is_out",
            ],
        )
        .await?;
    let mut moves: Vec<ValuedMove> = rows
        .iter()
        .filter_map(|row| {
            let direction = match (flag(row, "is_in"), flag(row, "is_out")) {
                (true, false) => Direction::In,
                (false, true) => Direction::Out,
                _ => return None,
            };
            Some(ValuedMove {
                id: row.get("id").and_then(Value::as_i64)?,
                product: row.get("product_id").and_then(first_id)?,
                quantity: number(row, "quantity_done"),
                price_unit: number(row, "price_unit"),
                value: number(row, "value"),
                direction,
            })
        })
        .collect();
    // the order a FIFO stack is built in is the order things happened.
    // Odoo sorts by `date`; this port's `stock.move` has none, so the id
    // is what says which receipt came first.
    moves.sort_by_key(|movement| movement.id);
    Ok(moves)
}

/// Every valued move of `product`, except the ones being valued right
/// now.
///
/// A stored `is_in`/`is_out` could be searched on directly; here they are
/// computed from the *picking's* state and locations, and the framework
/// only recomputes a stored field when the record that owns it is
/// written. So the moves are read and filtered rather than searched —
/// correct, and honest about costing a read per product.
async fn history_of(
    ctx: &MethodCtx<'_>,
    product: i64,
    excluded: &[i64],
) -> Result<Vec<ValuedMove>, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "stock.move",
            &parse_domain(&json!([["product_id", "=", product]]))?,
            &SearchOptions::default(),
        )
        .await?;
    let wanted: Vec<i64> = ids
        .into_iter()
        .filter(|id| !excluded.contains(id))
        .collect();
    read_valued_moves(ctx, &wanted).await
}

/// How each product involved is valued.
async fn costing_of(
    ctx: &MethodCtx<'_>,
    products: &[i64],
) -> Result<std::collections::HashMap<i64, Costing>, RusdooError> {
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "product.product",
            products,
            &["cost_method", "standard_price"],
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_i64)?;
            let method = CostMethod::parse(row.get("cost_method").and_then(Value::as_str)?);
            Some((
                id,
                Costing {
                    method,
                    standard_price: number(row, "standard_price"),
                },
            ))
        })
        .collect())
}

/// The receipts in a history, in the order they happened.
fn incoming_layers(history: &[Movement]) -> Vec<Layer> {
    history
        .iter()
        .filter(|movement| movement.direction == Direction::In)
        .map(|movement| Layer::new(movement.quantity, movement.value))
        .collect()
}

/// What a delivery is worth, port of the outgoing half of `_set_value`.
///
/// Odoo values an outgoing move at the product's `standard_price` for
/// every cost method but FIFO — because for AVCO the standard price *is*
/// the running average, kept up to date at each receipt by
/// `_update_standard_price`. Only FIFO has to walk back through the
/// receipts still in stock.
fn value_going_out(costing: &Costing, history: &[Movement], quantity: f64) -> f64 {
    match costing.method {
        CostMethod::Fifo => {
            let layers = incoming_layers(history);
            let (stack, qty_on_oldest) = fifo_stack(&layers, quantity_on_hand(history));
            run_fifo(&stack, qty_on_oldest, quantity, costing.standard_price)
        }
        _ => value_at_standard(quantity, costing.standard_price),
    }
}

/// What a receipt is worth, port of `_get_value_data`'s priority chain.
///
/// Odoo asks, in order: a manual adjustment, the invoice, the production
/// order, the purchase order line, the original move of a return, and
/// finally the product's cost. This port has the first and the last of
/// those, plus the price the receipt itself carries — which is where a
/// purchase order's price lands when `purchase_stock` fills it in.
fn value_coming_in(costing: &Costing, movement: &ValuedMove, adjusted: Option<f64>) -> f64 {
    if let Some(value) = adjusted {
        return value;
    }
    if movement.price_unit != 0.0 {
        return movement.price_unit * movement.quantity;
    }
    value_at_standard(movement.quantity, costing.standard_price)
}

/// The latest hand adjustment recorded against a move, if any.
async fn adjustment_for(ctx: &MethodCtx<'_>, move_id: i64) -> Result<Option<f64>, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "product.value",
            &parse_domain(&json!([["move_id", "=", move_id]]))?,
            &SearchOptions {
                // `product.value` is ordered newest first, so one row is
                // the last word on this move
                limit: Some(1),
                ..SearchOptions::default()
            },
        )
        .await?;
    let Some(id) = ids.first() else {
        return Ok(None);
    };
    let rows = ctx
        .registry
        .read(ctx.pool, "product.value", &[*id], &["value"])
        .await?;
    Ok(rows.first().map(|row| number(row, "value")))
}

/// What the product costs after this batch, port of
/// `_update_standard_price`.
///
/// Standard price products are left alone — the whole point of the method
/// is that the cost is decided by hand. FIFO takes the value of what is
/// still in stock over the quantity still in stock; AVCO recomputes the
/// running average over everything that happened.
fn new_standard_price(costing: &Costing, history: &[Movement]) -> Option<f64> {
    match costing.method {
        CostMethod::Standard => None,
        CostMethod::Average => Some(run_average(history).0),
        CostMethod::Fifo => {
            let on_hand = quantity_on_hand(history);
            if on_hand > 0.0 {
                let layers = incoming_layers(history);
                let (stack, qty_on_oldest) = fifo_stack(&layers, on_hand);
                return Some(
                    run_fifo(&stack, qty_on_oldest, on_hand, costing.standard_price) / on_hand,
                );
            }
            // nothing left in stock: Odoo falls back to what the last
            // receipt cost, so the next quotation is not priced at zero
            history
                .iter()
                .rev()
                .find(|movement| movement.direction == Direction::In)
                .and_then(|movement| Layer::new(movement.quantity, movement.value).unit_price())
        }
    }
}

/// `action_valuate` — value the moves of a validated transfer.
///
/// Port of the valuation `stock_account` bolts onto `_action_done`:
/// deliveries first, then receipts, then the products' cost. The order is
/// Odoo's and it matters: a delivery has to see the FIFO stack as it was
/// *before* the receipts on the same document, or a transfer that both
/// receives and ships would let today's goods pay for yesterday's order.
fn action_valuate<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "valuing needs at least one transfer".into(),
            ));
        }
        let pickings = ctx
            .registry
            .read(
                ctx.pool,
                "stock.picking",
                &ctx.ids,
                &["name", "state", "move_ids"],
            )
            .await?;
        let mut move_ids: Vec<i64> = Vec::new();
        for row in &pickings {
            let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
            if state != "done" {
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                return Err(RusdooError::Validation(format!(
                    "transfer {name} is {state:?}: validate it before valuing it"
                )));
            }
            move_ids.extend(
                row.get("move_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_i64),
            );
        }
        let batch = read_valued_moves(&ctx, &move_ids).await?;
        if batch.is_empty() {
            // an internal transfer values nothing, and saying "0 moves
            // valued" is better than pretending it did something
            return Ok(json!(0));
        }

        let mut products: Vec<i64> = batch.iter().map(|movement| movement.product).collect();
        products.sort_unstable();
        products.dedup();
        let costing = costing_of(&ctx, &products).await?;
        // the whole batch is left out of every history: Odoo values the
        // deliveries before the document is done at all, so none of these
        // moves is in the stack any of them is valued against
        let excluded: Vec<i64> = batch.iter().map(|movement| movement.id).collect();

        let mut history: std::collections::HashMap<i64, Vec<Movement>> =
            std::collections::HashMap::new();
        for product in &products {
            let past = history_of(&ctx, *product, &excluded).await?;
            history.insert(
                *product,
                past.iter().map(ValuedMove::movement).collect::<Vec<_>>(),
            );
        }

        let mut valued = 0usize;
        // deliveries first, receipts after — see the doc comment
        for direction in [Direction::Out, Direction::In] {
            for movement in batch.iter().filter(|m| m.direction == direction) {
                let Some(costing) = costing.get(&movement.product) else {
                    continue;
                };
                let past = history.entry(movement.product).or_default();
                let value = match direction {
                    Direction::Out => value_going_out(costing, past, movement.quantity),
                    Direction::In => {
                        let adjusted = adjustment_for(&ctx, movement.id).await?;
                        value_coming_in(costing, movement, adjusted)
                    }
                };
                let value = cents(value);
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "stock.move",
                        &[movement.id],
                        vec![("value", json!(value))],
                    )
                    .await?;
                // the move joins the history it was valued against, so
                // the next one on the same document sees it
                past.push(Movement {
                    direction,
                    quantity: movement.quantity,
                    value,
                });
                valued += 1;
            }
        }

        // and the products' cost follows what just happened
        for product in &products {
            let Some(costing) = costing.get(product) else {
                continue;
            };
            let Some(past) = history.get(product) else {
                continue;
            };
            if let Some(price) = new_standard_price(costing, past) {
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "product.product",
                        &[*product],
                        vec![("standard_price", json!(cents(price)))],
                    )
                    .await?;
            }
        }
        Ok(json!(valued))
    })
}

/// `action_adjust_valuation` — open the dialog that corrects one move's
/// value.
fn action_adjust_valuation<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [move_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "adjust the valuation of one move at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(ctx.pool, "stock.move", &[move_id], &["is_valued"])
            .await?;
        let valued = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("move {move_id} does not exist")))?;
        if !flag(valued, "is_valued") {
            return Err(RusdooError::Validation(
                "this move never crossed the company's stock: there is no value to adjust".into(),
            ));
        }
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Adjust Valuation",
            "res_model": "product.value",
            "view_mode": "form",
            "views": [[false, "form"]],
            "context": {"default_move_id": move_id},
            // a dialog over the move, not a screen replacing it
            "target": "new",
        }))
    })
}

/// `action_apply` — push a recorded adjustment onto what it corrects.
///
/// Port of the tail of `ProductValue.create`, which calls `_set_value` on
/// the move or `_update_standard_price` on the product. Here it is a
/// button because the framework has no create hook — which means an
/// adjustment written straight through the ORM is recorded and *not*
/// applied until somebody presses this.
fn action_apply<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [adjustment] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "apply one adjustment at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "product.value",
                &[adjustment],
                &["move_id", "product_id", "value"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("the adjustment is gone".into()))?;
        let value = cents(number(row, "value"));

        // a move's adjustment carries the value of the whole move; a
        // product's carries the new price of one unit. That asymmetry is
        // Odoo's own, and its docstring says so.
        if let Some(move_id) = row.get("move_id").and_then(first_id) {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "stock.move",
                    &[move_id],
                    vec![("value", json!(value))],
                )
                .await?;
            return Ok(json!(true));
        }
        let product = row
            .get("product_id")
            .and_then(first_id)
            .ok_or_else(|| RusdooError::Validation("the adjustment names nothing".into()))?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "product.product",
                &[product],
                vec![("standard_price", json!(value))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// The new price a caller asked for: `price` in the kwargs, or the first
/// positional argument.
fn wanted_price(rest: &[Value], kwargs: &Map<String, Value>) -> Result<f64, RusdooError> {
    kwargs
        .get("price")
        .or_else(|| rest.first())
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .ok_or_else(|| RusdooError::Validation("say the new cost: pass price".into()))
}

/// `action_change_standard_price` — set a product's cost, and say so.
///
/// Port of `_change_standard_price`. The row it leaves behind is the
/// point: a cost that changed with nobody's name on it is a margin
/// nobody can explain three months later.
fn action_change_standard_price<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [product] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "change the cost of one product at a time".into(),
            ));
        };
        let price = cents(wanted_price(&ctx.rest, kwargs)?);
        if price < 0.0 {
            return Err(RusdooError::Validation(
                "a cost below zero is not a discount, it is a typo".into(),
            ));
        }
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "product.product",
                &[product],
                &["name", "standard_price", "cost_method"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("product {product} does not exist")))?;
        let old_price = cents(number(row, "standard_price"));
        if old_price == price {
            // Odoo skips a no-op too: a history full of "changed from 10
            // to 10" is a history nobody reads
            return Ok(json!(false));
        }
        let method =
            CostMethod::parse(row.get("cost_method").and_then(Value::as_str).unwrap_or(""));

        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "product.product",
                &[product],
                vec![("standard_price", json!(price))],
            )
            .await?;

        // FIFO values what leaves from the receipts themselves, so the
        // cost on the product is only a hint and revaluing it changes
        // nothing that is already in stock. Odoo records no adjustment
        // for it, and neither does this.
        if method == CostMethod::Fifo {
            return Ok(json!(true));
        }
        let author = ctx
            .registry
            .read(ctx.pool, "res.users", &[ctx.uid], &["name"])
            .await?
            .first()
            .and_then(|user| user.get("name").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| format!("user {}", ctx.uid));
        let description = kwargs
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Price update from {old_price} to {price} by {author}"));
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "product.value",
                vec![
                    ("product_id", json!(product)),
                    ("value", json!(price)),
                    ("description", json!(description)),
                ],
            )
            .await?;
        Ok(json!(true))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn costing(method: CostMethod, standard_price: f64) -> Costing {
        Costing {
            method,
            standard_price,
        }
    }

    fn receipt(quantity: f64, value: f64) -> Movement {
        Movement::incoming(quantity, value)
    }

    #[test]
    fn a_standard_priced_delivery_is_worth_the_product_s_cost() {
        let history = [receipt(10.0, 200.0)];
        let value = value_going_out(&costing(CostMethod::Standard, 7.0), &history, 3.0);
        assert_eq!(value, 21.0, "the receipts do not price a standard product");
    }

    #[test]
    fn an_average_priced_delivery_is_worth_the_running_average_on_the_product() {
        // Odoo keeps the average *in* standard_price, so the delivery
        // reads it there and does not walk the history again
        let history = [receipt(10.0, 200.0), receipt(10.0, 100.0)];
        let value = value_going_out(&costing(CostMethod::Average, 15.0), &history, 4.0);
        assert_eq!(value, 60.0);
    }

    #[test]
    fn a_fifo_delivery_eats_the_oldest_receipts() {
        let history = [receipt(10.0, 120.0), receipt(5.0, 100.0)];
        let value = value_going_out(&costing(CostMethod::Fifo, 0.0), &history, 12.0);
        assert_eq!(value, 160.0);
    }

    #[test]
    fn a_receipt_is_worth_its_own_price_before_the_product_s() {
        let movement = ValuedMove {
            id: 1,
            product: 1,
            quantity: 4.0,
            price_unit: 25.0,
            value: 0.0,
            direction: Direction::In,
        };
        let costing = costing(CostMethod::Fifo, 9.0);
        assert_eq!(value_coming_in(&costing, &movement, None), 100.0);
        // and a hand adjustment beats both, which is the whole point of
        // `product.value`
        assert_eq!(value_coming_in(&costing, &movement, Some(70.0)), 70.0);
    }

    #[test]
    fn a_receipt_with_no_price_of_its_own_falls_back_to_the_product_s_cost() {
        let movement = ValuedMove {
            id: 1,
            product: 1,
            quantity: 4.0,
            price_unit: 0.0,
            value: 0.0,
            direction: Direction::In,
        };
        assert_eq!(
            value_coming_in(&costing(CostMethod::Standard, 9.0), &movement, None),
            36.0
        );
    }

    #[test]
    fn a_standard_priced_product_never_has_its_cost_rewritten() {
        let history = [receipt(10.0, 500.0)];
        assert_eq!(
            new_standard_price(&costing(CostMethod::Standard, 7.0), &history),
            None
        );
    }

    #[test]
    fn an_average_priced_product_takes_the_running_average() {
        let history = [receipt(10.0, 100.0), receipt(10.0, 300.0)];
        assert_eq!(
            new_standard_price(&costing(CostMethod::Average, 0.0), &history),
            Some(20.0)
        );
    }

    #[test]
    fn a_fifo_product_is_priced_by_what_is_still_in_stock() {
        // 10 at 12 came in, 5 went out, 5 at 30 came in: 10 on hand, made
        // of the 5 left at 12 and the 5 at 30
        let history = [
            receipt(10.0, 120.0),
            Movement::outgoing(5.0),
            receipt(5.0, 150.0),
        ];
        let price = new_standard_price(&costing(CostMethod::Fifo, 0.0), &history)
            .expect("fifo prices the stock");
        assert_eq!(cents(price), 21.0);
    }

    #[test]
    fn a_fifo_product_with_nothing_left_keeps_the_last_price_it_paid() {
        let history = [receipt(4.0, 80.0), Movement::outgoing(4.0)];
        assert_eq!(
            new_standard_price(&costing(CostMethod::Fifo, 1.0), &history),
            Some(20.0)
        );
    }

    #[test]
    fn a_fifo_product_that_never_received_anything_has_no_price_to_offer() {
        assert_eq!(
            new_standard_price(&costing(CostMethod::Fifo, 5.0), &[]),
            None
        );
    }

    #[test]
    fn the_new_cost_is_read_from_either_shape_a_client_sends() {
        let mut kwargs = Map::new();
        kwargs.insert("price".into(), json!(12.5));
        assert_eq!(wanted_price(&[], &kwargs).unwrap(), 12.5);
        assert_eq!(wanted_price(&[json!(9)], &Map::new()).unwrap(), 9.0);
        assert!(wanted_price(&[], &Map::new()).is_err());
    }

    #[test]
    fn only_the_receipts_of_a_history_become_fifo_layers() {
        let history = [
            receipt(10.0, 120.0),
            Movement::outgoing(4.0),
            receipt(2.0, 50.0),
        ];
        assert_eq!(
            incoming_layers(&history),
            vec![Layer::new(10.0, 120.0), Layer::new(2.0, 50.0)]
        );
    }
}
