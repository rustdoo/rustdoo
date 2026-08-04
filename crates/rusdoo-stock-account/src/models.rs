//! The models `stock_account` adds and the ones it extends.
//!
//! Three of them are `_inherit`: the company gains its valuation policy,
//! the product gains what it is worth, the move gains what it was worth
//! when it happened. One is new: `product.value`, the trail of every hand
//! adjustment somebody made to a valuation.

use rusdoo_core::RusdooError;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_product::PRICE;
use serde_json::{json, Map, Value};

use crate::valuation::CostMethod;

/// `property_cost_method` as a selection, in Odoo's own order.
fn cost_method_selection() -> FieldType {
    FieldType::Selection(vec![
        (
            CostMethod::Standard.as_str().into(),
            "Standard Price".into(),
        ),
        (
            CostMethod::Fifo.as_str().into(),
            "First In First Out (FIFO)".into(),
        ),
        (
            CostMethod::Average.as_str().into(),
            "Average Cost (AVCO)".into(),
        ),
    ])
}

/// `property_valuation` / `inventory_valuation`: when the accounting is
/// told about the stock.
fn valuation_selection() -> FieldType {
    FieldType::Selection(vec![
        ("periodic".into(), "Periodic (at closing)".into()),
        ("real_time".into(), "Perpetual (at invoicing)".into()),
    ])
}

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

/// An `_inherit` that extends a model in place: same name, same table.
fn extending(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

/// A numeric field's value, whatever shape the driver decoded it in.
pub(crate) fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// The value at the end of a dotted dependency that went through a
/// many2one.
///
/// The ORM hands every dependency back as a list, one entry per record
/// the path reached — a one2many gives many, a many2one gives one, and an
/// empty reference gives none. A move whose picking was never set has no
/// state to read, and that is a `None`, not a `false` that would read as
/// "not done".
fn hop<'a>(record: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    record.get(path)?.as_array()?.first()
}

fn hop_str<'a>(record: &'a Map<String, Value>, path: &str) -> Option<&'a str> {
    hop(record, path)?.as_str()
}

/// Every field of `path` on the records an x2many reached.
fn over_lines<'a>(record: &'a Map<String, Value>, path: &str) -> &'a [Value] {
    record
        .get(path)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(company())?;
    reg.register(location())?;
    reg.register(product())?;
    reg.register(stock_move())?;
    reg.register(product_value())?;
    reg.register(account_move())?;
    Ok(())
}

// ---------------------------------------------------------------------
// res.company
// ---------------------------------------------------------------------

/// `res.company` — where the valuation policy is decided.
///
/// Odoo also hangs the stock journal and the stock valuation account
/// here. Neither is ported: this ORM has no `account.journal` and no
/// `account.account` yet, so a many2one to either would be a column
/// pointing at a table nobody creates.
fn company() -> Model {
    Model::new(
        extending("res.company", "res_company"),
        vec![
            Field::new("cost_method", cost_method_selection())
                .required()
                .default_value(json!(CostMethod::Standard.as_str())),
            Field::new("inventory_valuation", valuation_selection())
                .default_value(json!("periodic")),
            // how often the closing entry is drawn. The cron that reads
            // it is not ported (see the crate's doc comment), so today it
            // records the intent and nothing runs off it.
            Field::new(
                "inventory_period",
                FieldType::Selection(vec![
                    ("manual".into(), "Manual".into()),
                    ("daily".into(), "Daily".into()),
                    ("monthly".into(), "Monthly".into()),
                ]),
            )
            .required()
            .default_value(json!("manual")),
        ],
    )
}

// ---------------------------------------------------------------------
// stock.location
// ---------------------------------------------------------------------

/// Odoo's `_should_be_valued`: a location holds the company's own stock.
///
/// Odoo asks for a company *and* a usage in `internal`/`transit`. This
/// port's `stock.location` has neither a company nor a transit usage, so
/// what is left is the question that actually decides it: is this a place
/// inside the warehouse, or the customer's, the vendor's, or the
/// adjustment counterpart?
pub(crate) fn is_valued_usage(usage: &str) -> bool {
    usage == "internal"
}

fn location_is_valued_internal(record: &Map<String, Value>) -> Value {
    let usage = record.get("usage").and_then(Value::as_str).unwrap_or("");
    json!(is_valued_usage(usage))
}

fn location_is_valued_external(record: &Map<String, Value>) -> Value {
    let usage = record.get("usage").and_then(Value::as_str).unwrap_or("");
    json!(!is_valued_usage(usage))
}

/// `stock.location` — which side of the company's boundary a place is on.
fn location() -> Model {
    Model::new(
        extending("stock.location", "stock_location"),
        vec![
            Field::new("is_valued_internal", FieldType::Boolean)
                .computed(&["usage"], location_is_valued_internal),
            Field::new("is_valued_external", FieldType::Boolean)
                .computed(&["usage"], location_is_valued_external),
        ],
    )
}

// ---------------------------------------------------------------------
// product.product
// ---------------------------------------------------------------------

/// The paths the product's valuation reads off its moves. Every one of
/// them comes back as a list in the same order, so they line up.
const MOVE_VALUE: &str = "stock_move_ids.value";
const MOVE_IS_IN: &str = "stock_move_ids.is_in";
const MOVE_IS_OUT: &str = "stock_move_ids.is_out";
const MOVE_QUANTITY: &str = "stock_move_ids.quantity_done";

/// What a truthy value looks like coming back from a boolean column.
fn is_true(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.as_bool().unwrap_or(false))
}

fn as_number(value: Option<&Value>) -> f64 {
    value
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// What the product's valued moves add up to: value and quantity.
///
/// Odoo reaches this through the quants and the cost method's own batch
/// function; here the moves are the only record of what crossed the
/// boundary, so they are what is counted. What comes in adds, what goes
/// out takes away — and an outgoing move was already valued at whatever
/// its cost method said when it happened, which is exactly why the value
/// lives on the move.
fn valued_totals(record: &Map<String, Value>) -> (f64, f64) {
    let values = over_lines(record, MOVE_VALUE);
    let ins = over_lines(record, MOVE_IS_IN);
    let outs = over_lines(record, MOVE_IS_OUT);
    let quantities = over_lines(record, MOVE_QUANTITY);

    let mut value = 0.0;
    let mut quantity = 0.0;
    for index in 0..values.len() {
        let incoming = is_true(ins.get(index));
        let outgoing = is_true(outs.get(index));
        if incoming == outgoing {
            // neither, or a move the port cannot classify: it did not
            // cross the boundary, so it is worth nothing here
            continue;
        }
        let sign = if incoming { 1.0 } else { -1.0 };
        value += sign * as_number(values.get(index));
        quantity += sign * as_number(quantities.get(index));
    }
    (value, quantity)
}

/// `total_value` — what the stock of this product is worth.
fn product_total_value(record: &Map<String, Value>) -> Value {
    let (value, _) = valued_totals(record);
    json!((value * 100.0).round() / 100.0)
}

/// `avg_cost` — the value divided by what is on hand.
///
/// With nothing on hand there is nothing to divide, and Odoo answers the
/// product's own cost rather than zero: a catalogue that shows 0.00 for
/// every product that happens to be out of stock is a catalogue nobody
/// can price from.
fn product_avg_cost(record: &Map<String, Value>) -> Value {
    let (value, quantity) = valued_totals(record);
    let cost = if quantity == 0.0 {
        number(record, "standard_price")
    } else {
        value / quantity
    };
    json!((cost * 100.0).round() / 100.0)
}

/// `product.product` — how this product is valued, and what it is worth.
///
/// In Odoo `cost_method` and `valuation` are computed from the product's
/// category, falling back to the company. This port has no
/// `product.category`, so they are stored on the product with the same
/// defaults the company carries — the effective value is the same one
/// Odoo's compute would land on for a category that says nothing.
fn product() -> Model {
    Model::new(
        extending("product.product", "product_product"),
        vec![
            Field::new("cost_method", cost_method_selection())
                .default_value(json!(CostMethod::Standard.as_str())),
            Field::new("valuation", valuation_selection()).default_value(json!("periodic")),
            Field::new(
                "stock_move_ids",
                FieldType::One2many {
                    comodel: "stock.move".into(),
                    inverse: "product_id".into(),
                },
            ),
            // not materialized: the totals change when a *move* is
            // written, and a recompute only follows the fields of the
            // record being written — a stored column here would go stale
            // the first time a picking was validated
            Field::new("total_value", PRICE).computed(
                &[MOVE_VALUE, MOVE_IS_IN, MOVE_IS_OUT, MOVE_QUANTITY],
                product_total_value,
            ),
            Field::new("avg_cost", PRICE).computed(
                &[
                    MOVE_VALUE,
                    MOVE_IS_IN,
                    MOVE_IS_OUT,
                    MOVE_QUANTITY,
                    "standard_price",
                ],
                product_avg_cost,
            ),
        ],
    )
}

// ---------------------------------------------------------------------
// stock.move
// ---------------------------------------------------------------------

/// The picking's fields a move has to look at to know which way it went.
const PICKING_STATE: &str = "picking_id.state";
const PICKING_SOURCE: &str = "picking_id.location_id.usage";
const PICKING_DEST: &str = "picking_id.location_dest_id.usage";

/// Which way this move crossed the company's boundary, or neither.
///
/// Odoo asks the move *lines* (`_get_in_move_lines`), because a single
/// move can be picked from two places at once. This port's `stock.move`
/// has no lines and no locations of its own, so the picking's are what
/// says it — one direction per document, which is what a delivery or a
/// receipt is anyway.
fn crossing(record: &Map<String, Value>) -> Option<crate::valuation::Direction> {
    if hop_str(record, PICKING_STATE) != Some("done") {
        return None;
    }
    let source = is_valued_usage(hop_str(record, PICKING_SOURCE)?);
    let destination = is_valued_usage(hop_str(record, PICKING_DEST)?);
    match (source, destination) {
        (false, true) => Some(crate::valuation::Direction::In),
        (true, false) => Some(crate::valuation::Direction::Out),
        // internal to internal moves nothing across the boundary, and
        // supplier to customer is a dropship, which this port does not
        // value (see the crate's doc comment)
        _ => None,
    }
}

fn move_is_in(record: &Map<String, Value>) -> Value {
    json!(crossing(record) == Some(crate::valuation::Direction::In))
}

fn move_is_out(record: &Map<String, Value>) -> Value {
    json!(crossing(record) == Some(crate::valuation::Direction::Out))
}

fn move_is_valued(record: &Map<String, Value>) -> Value {
    json!(crossing(record).is_some())
}

/// `stock.move` — what the move was worth.
fn stock_move() -> Model {
    Model::new(
        extending("stock.move", "stock_move"),
        vec![
            // Odoo defaults this on, and for the reason the help text
            // gives: a return that does not undo the invoiced quantity
            // leaves the customer billed for goods they sent back
            Field::new("to_refund", FieldType::Boolean).default_value(json!(true)),
            // the move's own value, in the company's currency. Zero until
            // the move is valued, which is what Odoo's help says too.
            Field::new("value", PRICE).default_value(json!(0.0)),
            // what a receipt says it cost, when whoever created it knows
            // — a purchase order line's price, an inventory adjustment's
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            Field::new("is_in", FieldType::Boolean)
                .computed(&[PICKING_STATE, PICKING_SOURCE, PICKING_DEST], move_is_in),
            Field::new("is_out", FieldType::Boolean)
                .computed(&[PICKING_STATE, PICKING_SOURCE, PICKING_DEST], move_is_out),
            Field::new("is_valued", FieldType::Boolean).computed(
                &[PICKING_STATE, PICKING_SOURCE, PICKING_DEST],
                move_is_valued,
            ),
            Field::new("standard_price", PRICE).related("product_id.standard_price"),
            // the entry this move was posted through. Nothing writes it
            // yet — the port has no accounts to post to — but the link is
            // the one an integrator with `account.account` needs, and it
            // costs a column.
            m2o("account_move_id", "account.move"),
            Field::new(
                "product_value_ids",
                FieldType::One2many {
                    comodel: "product.value".into(),
                    inverse: "move_id".into(),
                },
            ),
        ],
    )
}

// ---------------------------------------------------------------------
// product.value
// ---------------------------------------------------------------------

/// A value adjustment that says nothing is an adjustment nobody can
/// audit: Odoo's own form makes the description the point of the dialog.
fn adjustment_is_explained(record: &Map<String, Value>) -> Result<(), String> {
    let named = ["product_id", "move_id"]
        .iter()
        .any(|field| record.get(*field).is_some_and(points_somewhere));
    if !named {
        return Err("say what is being revalued: a product or a move".into());
    }
    Ok(())
}

/// Whether a many2one holds a reference, in either of the two shapes it
/// travels in: the raw id a write carries, and the `[id, name]` pair a
/// read answers.
fn points_somewhere(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.as_i64().is_some_and(|id| id != 0),
        Value::Array(items) => items.first().and_then(Value::as_i64).is_some(),
        _ => false,
    }
}

/// `product.value` — the trail of every hand adjustment to a valuation.
///
/// Odoo's docstring: the history of a *manual* update of a value — a new
/// standard price, or a move whose value somebody corrected. It is the
/// only record of a decision that a recomputation would otherwise erase,
/// which is why it is a model and not a field.
fn product_value() -> Model {
    Model::new(
        ModelMeta {
            name: "product.value".into(),
            table: "product_value".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            m2o("product_id", "product.product"),
            // the adjustment belongs to the move: deleting the move
            // leaves a correction to a document that no longer exists
            m2o("move_id", "stock.move").ondelete(OnDelete::Cascade),
            Field::new("value", PRICE).required(),
            // not required, unlike Odoo. Its `_compute_company_id` falls
            // back to `self.env.company`, which is always set; this port
            // has no such fallback, and a user with no company would then
            // be unable to correct anything.
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new("date", FieldType::Datetime)
                .required()
                .default_from(defaults::NOW),
            // who decided it. Required, as in Odoo: an adjustment with no
            // author is the one thing an audit cannot work with.
            m2o("user_id", "res.users")
                .required()
                .default_from(defaults::CURRENT_USER),
            char("description"),
        ],
    )
    .constrained(
        "an adjustment names what it revalues",
        &["product_id", "move_id"],
        adjustment_is_explained,
    )
    // newest first: what somebody opening the history wants is the last
    // decision, not the first
    .ordered("date desc, id desc")
}

// ---------------------------------------------------------------------
// account.move
// ---------------------------------------------------------------------

/// `account.move` — the stock moves an entry accounts for.
fn account_move() -> Model {
    Model::new(
        extending("account.move", "account_move"),
        vec![Field::new(
            "stock_move_ids",
            FieldType::One2many {
                comodel: "stock.move".into(),
                inverse: "account_move_id".into(),
            },
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::valuation::Direction;

    /// A dependency as the ORM hands it over: one value per record the
    /// path reached, which for a many2one is exactly one.
    fn moved(state: &str, source: &str, destination: &str) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert(PICKING_STATE.into(), json!([state]));
        record.insert(PICKING_SOURCE.into(), json!([source]));
        record.insert(PICKING_DEST.into(), json!([destination]));
        record
    }

    #[test]
    fn only_an_internal_location_holds_the_company_s_own_stock() {
        assert!(is_valued_usage("internal"));
        for outside in ["customer", "supplier", "inventory", ""] {
            assert!(!is_valued_usage(outside), "{outside}");
        }
    }

    #[test]
    fn a_location_is_valued_on_exactly_one_side() {
        let mut record = Map::new();
        record.insert("usage".into(), json!("internal"));
        assert_eq!(location_is_valued_internal(&record), json!(true));
        assert_eq!(location_is_valued_external(&record), json!(false));
        record.insert("usage".into(), json!("customer"));
        assert_eq!(location_is_valued_internal(&record), json!(false));
        assert_eq!(location_is_valued_external(&record), json!(true));
    }

    #[test]
    fn a_receipt_comes_in_and_a_delivery_goes_out() {
        assert_eq!(
            crossing(&moved("done", "supplier", "internal")),
            Some(Direction::In)
        );
        assert_eq!(
            crossing(&moved("done", "internal", "customer")),
            Some(Direction::Out)
        );
        assert_eq!(
            move_is_in(&moved("done", "supplier", "internal")),
            json!(true)
        );
        assert_eq!(
            move_is_out(&moved("done", "internal", "customer")),
            json!(true)
        );
    }

    #[test]
    fn an_internal_transfer_crosses_nothing() {
        let record = moved("done", "internal", "internal");
        assert_eq!(crossing(&record), None);
        assert_eq!(move_is_in(&record), json!(false));
        assert_eq!(move_is_out(&record), json!(false));
        assert_eq!(move_is_valued(&record), json!(false));
    }

    #[test]
    fn a_dropship_is_not_valued_by_this_port() {
        // supplier straight to customer: nothing of the company's ever
        // held it, and Odoo values it through a path this port does not
        // have
        assert_eq!(crossing(&moved("done", "supplier", "customer")), None);
    }

    #[test]
    fn a_move_that_has_not_happened_yet_is_worth_nothing() {
        for state in ["draft", "confirmed", "cancel"] {
            let record = moved(state, "supplier", "internal");
            assert_eq!(crossing(&record), None, "{state}");
            assert_eq!(move_is_valued(&record), json!(false));
        }
    }

    #[test]
    fn a_move_without_a_picking_is_not_guessed_at() {
        // the paths come back empty, which must not read as "done, from
        // nowhere, to nowhere"
        let mut record = Map::new();
        record.insert(PICKING_STATE.into(), json!([]));
        record.insert(PICKING_SOURCE.into(), json!([]));
        record.insert(PICKING_DEST.into(), json!([]));
        assert_eq!(crossing(&record), None);
        assert_eq!(crossing(&Map::new()), None);
    }

    /// The four aligned lists a product's valuation reads.
    fn with_moves(values: Value, ins: Value, outs: Value, quantities: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert(MOVE_VALUE.into(), values);
        record.insert(MOVE_IS_IN.into(), ins);
        record.insert(MOVE_IS_OUT.into(), outs);
        record.insert(MOVE_QUANTITY.into(), quantities);
        record
    }

    #[test]
    fn a_product_is_worth_what_came_in_minus_what_went_out() {
        let record = with_moves(
            json!([120.0, 100.0, 60.0]),
            json!([true, true, false]),
            json!([false, false, true]),
            json!([10.0, 5.0, 5.0]),
        );
        assert_eq!(product_total_value(&record), json!(160.0));
        // 10 on hand for 160
        assert_eq!(product_avg_cost(&record), json!(16.0));
    }

    #[test]
    fn a_move_that_crossed_nothing_is_left_out_of_the_total() {
        let record = with_moves(
            json!([120.0, 999.0]),
            json!([true, false]),
            json!([false, false]),
            json!([10.0, 7.0]),
        );
        assert_eq!(product_total_value(&record), json!(120.0));
    }

    #[test]
    fn a_product_with_nothing_on_hand_is_priced_at_its_own_cost() {
        let mut record = with_moves(json!([]), json!([]), json!([]), json!([]));
        record.insert("standard_price".into(), json!(9.5));
        assert_eq!(product_total_value(&record), json!(0.0));
        assert_eq!(product_avg_cost(&record), json!(9.5));
    }

    #[test]
    fn an_adjustment_has_to_name_what_it_revalues() {
        let mut record = Map::new();
        assert!(adjustment_is_explained(&record).is_err());
        // a reference travels as a raw id on the way in and as
        // `[id, name]` on the way out; both say the same thing
        record.insert("move_id".into(), json!(7));
        assert!(adjustment_is_explained(&record).is_ok());
        let mut other = Map::new();
        other.insert("product_id".into(), json!([3, "Chair"]));
        assert!(adjustment_is_explained(&other).is_ok());
        // and an empty reference names nothing
        let mut empty = Map::new();
        empty.insert("product_id".into(), Value::Null);
        empty.insert("move_id".into(), json!(false));
        assert!(adjustment_is_explained(&empty).is_err());
    }
}
