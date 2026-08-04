//! The cost methods themselves, port of `stock_account/models/product.py`:
//! `_run_standard_batch`, `_run_average_batch`, `_run_fifo_get_stack` and
//! `_run_fifo`.
//!
//! Plain functions over plain numbers, on purpose. Deciding what a
//! delivery is worth is the one thing in this addon a warehouse cannot
//! check by looking at a screen — it has to be *read*, and a function
//! that takes a list of movements and answers a number can be read, and
//! tested, without a database anywhere near it. The methods in
//! `methods.rs` fetch the moves and call these.

/// How Odoo's `_run_*` functions decide a product is valued: by its
/// category's `property_cost_method`, falling back to the company's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostMethod {
    /// the price on the product, whatever it cost to buy
    Standard,
    /// first in, first out: what leaves is worth what the oldest
    /// remaining receipt cost
    Fifo,
    /// weighted average (AVCO), recomputed at every receipt
    Average,
}

impl CostMethod {
    /// The value as it travels in a selection field.
    pub fn as_str(self) -> &'static str {
        match self {
            CostMethod::Standard => "standard",
            CostMethod::Fifo => "fifo",
            CostMethod::Average => "average",
        }
    }

    /// Read a stored selection back. An unknown value is the standard
    /// price rather than an error: a product whose cost method could not
    /// be read is still worth something, and Odoo's own compute falls
    /// back the same way when the category says nothing.
    pub fn parse(value: &str) -> CostMethod {
        match value {
            "fifo" => CostMethod::Fifo,
            "average" => CostMethod::Average,
            _ => CostMethod::Standard,
        }
    }
}

/// One receipt as the valuation sees it: how much came in, and what it
/// was worth. Odoo carries the `stock.move` itself around; the two
/// numbers are all any of the cost methods ever read off it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    pub quantity: f64,
    pub value: f64,
}

impl Layer {
    pub fn new(quantity: f64, value: f64) -> Layer {
        Layer { quantity, value }
    }

    /// What one unit of this receipt cost, or `None` when the receipt
    /// moved nothing — a unit price out of a division by zero is a number
    /// that looks like a cost and is not one.
    pub fn unit_price(&self) -> Option<f64> {
        (self.quantity != 0.0).then(|| self.value / self.quantity)
    }
}

/// Which way a move crosses the company's boundary
/// (`stock_account/models/stock_move.py`: `_is_in`, `_is_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

/// One valued move in the history a running average walks over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Movement {
    pub direction: Direction,
    pub quantity: f64,
    /// what the move was worth when it came in; ignored for an outgoing
    /// move, whose value the average itself decides
    pub value: f64,
}

impl Movement {
    pub fn incoming(quantity: f64, value: f64) -> Movement {
        Movement {
            direction: Direction::In,
            quantity,
            value,
        }
    }

    pub fn outgoing(quantity: f64) -> Movement {
        Movement {
            direction: Direction::Out,
            quantity,
            value: 0.0,
        }
    }
}

/// What a quantity is worth at the product's own cost
/// (`_run_standard_batch`).
pub fn value_at_standard(quantity: f64, standard_price: f64) -> f64 {
    quantity * standard_price
}

/// What is on hand after a history of valued moves.
///
/// Odoo reads this off the quants (`qty_available`); this port has no
/// quants, so it is the moves that say it — which is the same number as
/// long as every move that crossed the boundary is in the list.
pub fn quantity_on_hand(history: &[Movement]) -> f64 {
    history
        .iter()
        .map(|movement| match movement.direction {
            Direction::In => movement.quantity,
            Direction::Out => -movement.quantity,
        })
        .sum()
}

/// The weighted average and the total value after `history`, port of the
/// accumulation loop in `_run_average_batch`.
///
/// The odd-looking branch on `previous_qty <= 0` is Odoo's and it earns
/// its place: after a delivery that took the stock negative, the value
/// accumulated so far is not the value of anything real, so the next
/// receipt does not add to it — it *replaces* the average, and the value
/// is recomputed from the quantity now on hand.
pub fn run_average(history: &[Movement]) -> (f64, f64) {
    let mut quantity = 0.0f64;
    let mut value = 0.0f64;
    // Odoo seeds the average with the first move's unit price so that a
    // history starting with a delivery is not valued at zero
    let mut average_cost = history.first().and_then(seed_price).unwrap_or(0.0);

    for movement in history {
        match movement.direction {
            Direction::In => {
                let previous_qty = quantity;
                quantity += movement.quantity;
                if previous_qty > 0.0 {
                    value += movement.value;
                    if quantity != 0.0 {
                        average_cost = value / quantity;
                    }
                } else {
                    // coming back from a negative (or empty) stock: the
                    // receipt's own price is the truth, not the average
                    if movement.quantity != 0.0 {
                        average_cost = movement.value / movement.quantity;
                    }
                    value = average_cost * quantity;
                }
            }
            Direction::Out => {
                value -= movement.quantity * average_cost;
                quantity -= movement.quantity;
            }
        }
    }
    (average_cost, value)
}

/// The first movement's unit price, which is what the running average
/// starts from (`_run_average_batch`: `first_move.value /
/// first_move._get_valued_qty()`).
fn seed_price(movement: &Movement) -> Option<f64> {
    (movement.quantity != 0.0).then(|| movement.value / movement.quantity)
}

/// The receipts that make up what is on hand, newest last, together with
/// how much of the *oldest* one is still there — port of
/// `_run_fifo_get_stack`.
///
/// Odoo walks the receipts backwards from the newest until it has covered
/// the quantity on hand, then reverses: what is left in stock is the tail
/// of the receipts, and the oldest one in that tail is usually only
/// partly still there. `incoming` is oldest first.
pub fn fifo_stack(incoming: &[Layer], on_hand: f64) -> (Vec<Layer>, f64) {
    if on_hand <= 0.0 {
        return (Vec::new(), 0.0);
    }
    let mut stack: Vec<Layer> = Vec::new();
    let mut remaining = on_hand;
    let mut qty_on_oldest = 0.0;
    for layer in incoming.iter().rev() {
        if remaining <= 0.0 {
            break;
        }
        stack.push(*layer);
        qty_on_oldest = layer.quantity.min(remaining);
        remaining -= layer.quantity;
    }
    stack.reverse();
    (stack, qty_on_oldest)
}

/// What the next `quantity` going out is worth, port of `_run_fifo`.
///
/// `qty_on_oldest` is what [`fifo_stack`] answered: the part of the
/// oldest receipt that is still in stock. When the stack runs out before
/// the quantity does — a delivery of more than was ever received, which
/// happens the moment somebody backdates a receipt — Odoo extrapolates
/// with the last price it saw rather than valuing the rest at nothing.
pub fn run_fifo(stack: &[Layer], qty_on_oldest: f64, quantity: f64, standard_price: f64) -> f64 {
    if quantity <= 0.0 {
        return quantity * standard_price;
    }
    let mut wanted = quantity;
    let mut cost = 0.0;
    let mut qty_on_first = qty_on_oldest;
    let mut last: Option<Layer> = None;

    for layer in stack {
        if wanted <= 0.0 {
            break;
        }
        last = Some(*layer);
        let (mut in_qty, mut in_value) = if qty_on_first > 0.0 {
            // only part of the oldest receipt is still in stock, so only
            // that part of its value may be consumed
            let taken = qty_on_first;
            qty_on_first = 0.0;
            match layer.unit_price() {
                Some(unit) => (taken, unit * taken),
                None => (taken, 0.0),
            }
        } else {
            (layer.quantity, layer.value)
        };
        if in_qty > wanted {
            if in_qty != 0.0 {
                in_value = in_value * wanted / in_qty;
            }
            in_qty = wanted;
        }
        cost += in_value;
        wanted -= in_qty;
    }

    if wanted > 0.0 {
        let unit = last
            .and_then(|layer| layer.unit_price())
            .unwrap_or(standard_price);
        cost += wanted * unit;
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two decimals is what the port stores money in; comparing raw f64
    /// sums would fail on 0.1 + 0.2 and say nothing about the algorithm.
    fn cents(value: f64) -> f64 {
        (value * 100.0).round() / 100.0
    }

    #[test]
    fn a_cost_method_survives_the_round_trip_through_a_selection() {
        for method in [CostMethod::Standard, CostMethod::Fifo, CostMethod::Average] {
            assert_eq!(CostMethod::parse(method.as_str()), method);
        }
        // a product whose category said nothing is valued at its own cost
        assert_eq!(CostMethod::parse(""), CostMethod::Standard);
        assert_eq!(CostMethod::parse("nonsense"), CostMethod::Standard);
    }

    #[test]
    fn the_standard_price_values_the_quantity_and_nothing_else() {
        assert_eq!(value_at_standard(3.0, 12.5), 37.5);
        assert_eq!(value_at_standard(0.0, 12.5), 0.0);
    }

    #[test]
    fn what_is_on_hand_is_what_came_in_minus_what_went_out() {
        let history = [
            Movement::incoming(10.0, 100.0),
            Movement::outgoing(4.0),
            Movement::incoming(5.0, 75.0),
        ];
        assert_eq!(quantity_on_hand(&history), 11.0);
        assert_eq!(quantity_on_hand(&[]), 0.0);
    }

    #[test]
    fn the_average_is_weighted_by_what_each_receipt_brought() {
        // 10 at 10, then 10 at 20: the average is 15, not 10 or 20
        let history = [
            Movement::incoming(10.0, 100.0),
            Movement::incoming(10.0, 200.0),
        ];
        let (average, value) = run_average(&history);
        assert_eq!(cents(average), 15.0);
        assert_eq!(cents(value), 300.0);
    }

    #[test]
    fn a_delivery_leaves_the_average_alone_and_takes_its_share_of_the_value() {
        let history = [
            Movement::incoming(10.0, 100.0),
            Movement::incoming(10.0, 200.0),
            Movement::outgoing(5.0),
        ];
        let (average, value) = run_average(&history);
        // what leaves is valued at the average, so the average does not
        // move: 15 stays 15, and 15 units are worth 225
        assert_eq!(cents(average), 15.0);
        assert_eq!(cents(value), 225.0);
    }

    #[test]
    fn a_receipt_onto_negative_stock_replaces_the_average_instead_of_adding_to_it() {
        // delivering 5 of a product that has none takes the stock to -5
        // and the value to -50 at the seeded price; the receipt that
        // follows is the only honest price there is
        let history = [
            Movement::incoming(0.0, 0.0),
            Movement::outgoing(5.0),
            Movement::incoming(10.0, 300.0),
        ];
        let (average, value) = run_average(&history);
        assert_eq!(cents(average), 30.0, "the receipt's own price wins");
        // 5 units on hand at 30
        assert_eq!(cents(value), 150.0);
    }

    #[test]
    fn an_average_over_nothing_is_zero_and_not_a_division_by_zero() {
        let (average, value) = run_average(&[]);
        assert_eq!(average, 0.0);
        assert_eq!(value, 0.0);
    }

    #[test]
    fn the_fifo_stack_is_the_tail_of_the_receipts_that_is_still_in_stock() {
        let incoming = [
            Layer::new(10.0, 100.0),
            Layer::new(5.0, 100.0),
            Layer::new(8.0, 240.0),
        ];
        // 11 on hand: all 8 of the last receipt, 3 of the one before
        let (stack, qty_on_oldest) = fifo_stack(&incoming, 11.0);
        assert_eq!(stack, vec![Layer::new(5.0, 100.0), Layer::new(8.0, 240.0)]);
        assert_eq!(qty_on_oldest, 3.0);
    }

    #[test]
    fn an_empty_stock_has_no_fifo_stack() {
        let incoming = [Layer::new(10.0, 100.0)];
        assert_eq!(fifo_stack(&incoming, 0.0), (Vec::new(), 0.0));
        assert_eq!(fifo_stack(&incoming, -3.0), (Vec::new(), 0.0));
        assert_eq!(fifo_stack(&[], 5.0), (Vec::new(), 0.0));
    }

    #[test]
    fn fifo_takes_the_oldest_receipts_first() {
        // 10 at 12, then 5 at 20; deliver 12
        let (stack, qty_on_oldest) =
            fifo_stack(&[Layer::new(10.0, 120.0), Layer::new(5.0, 100.0)], 15.0);
        let cost = run_fifo(&stack, qty_on_oldest, 12.0, 0.0);
        // all 10 of the first receipt, then 2 of the second
        assert_eq!(cents(cost), 160.0);
    }

    #[test]
    fn fifo_only_consumes_the_part_of_the_oldest_receipt_still_in_stock() {
        // 10 at 12 and 5 at 20 came in, 7 already went out: 3 of the
        // first receipt are left, and they are what the next delivery
        // starts eating
        let (stack, qty_on_oldest) =
            fifo_stack(&[Layer::new(10.0, 120.0), Layer::new(5.0, 100.0)], 8.0);
        assert_eq!(qty_on_oldest, 3.0);
        let cost = run_fifo(&stack, qty_on_oldest, 4.0, 0.0);
        // 3 at 12, then 1 at 20
        assert_eq!(cents(cost), 56.0);
    }

    #[test]
    fn fifo_extrapolates_with_the_last_price_when_more_leaves_than_ever_came_in() {
        let (stack, qty_on_oldest) = fifo_stack(&[Layer::new(4.0, 80.0)], 4.0);
        let cost = run_fifo(&stack, qty_on_oldest, 6.0, 5.0);
        // 4 at 20, then 2 more at the last price seen (20), not at the
        // product's 5 and not at nothing
        assert_eq!(cents(cost), 120.0);
    }

    #[test]
    fn fifo_falls_back_to_the_product_cost_when_there_is_no_receipt_at_all() {
        let cost = run_fifo(&[], 0.0, 3.0, 7.0);
        assert_eq!(cents(cost), 21.0);
    }

    #[test]
    fn fifo_of_nothing_is_worth_nothing() {
        assert_eq!(run_fifo(&[Layer::new(4.0, 80.0)], 4.0, 0.0, 5.0), 0.0);
    }

    #[test]
    fn a_free_receipt_does_not_make_the_next_delivery_free_by_dividing_by_zero() {
        // a receipt of zero units carries no price; consuming it must not
        // produce a NaN that then poisons every later total
        let (stack, qty_on_oldest) =
            fifo_stack(&[Layer::new(0.0, 0.0), Layer::new(5.0, 50.0)], 5.0);
        let cost = run_fifo(&stack, qty_on_oldest, 5.0, 3.0);
        assert!(cost.is_finite(), "{cost}");
        assert_eq!(cents(cost), 50.0);
    }
}
