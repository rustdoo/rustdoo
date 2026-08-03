//! rusdoo-uom — port of `odoo/addons/uom/models/`: what a quantity
//! actually means.
//!
//! A unit points at the unit it is expressed in (`relative_uom_id`) and
//! says how many of those it contains (`relative_factor`). Follow that
//! chain to its end and you reach the unit that is its own reference —
//! what Odoo used to call the *category*, and what still decides whether
//! two units can be compared at all. Grams convert to tons; grams do not
//! convert to hours, and this module says so instead of returning a
//! number.
//!
//! Odoo 19 dropped `uom.category` for exactly this tree, so the port
//! follows the tree: the "category" of a unit is the root of its
//! reference chain.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Conversion factors need every digit they have: `float8`, not a
/// column truncated to two decimals. An ounce is 28.3495 grams, and a
/// factor rounded on the way in is wrong for every quantity afterwards.
const RATIO: FieldType = FieldType::Float { digits: None };

/// How far a reference chain may go before the walk gives up. Real data
/// is three or four links deep (mm → cm → m → km); anything past this is
/// a loop somebody created by hand, and looping forever is not an answer.
const MAX_CHAIN: usize = 16;

/// The precision a unit is measured to, when nobody said otherwise.
/// Odoo 19 keeps this in `decimal.precision('Product Unit')`, one value
/// for the whole database; without that model the value lives on the
/// unit, like it did up to Odoo 18.
const DEFAULT_ROUNDING: f64 = 0.01;

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(uom())?;
    Ok(())
}

/// Conversion is a method, not a computed field: it takes an argument
/// (the unit to convert into) and it may refuse. Both need `Read` —
/// asking what 1020 kg is in tons changes nothing.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "uom.uom",
        "convert_quantity",
        Operation::Read,
        convert_quantity,
    )?;
    methods.register("uom.uom", "convert_price", Operation::Read, convert_price)?;
    Ok(())
}

// ── the model ───────────────────────────────────────────────────────

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// `uom.uom` — one unit of measure.
fn uom() -> Model {
    Model::new(
        meta("uom.uom", "uom_uom"),
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            // "contains": how many reference units one of this is worth
            Field::new("relative_factor", RATIO)
                .required()
                .default_value(json!(1.0)),
            Field::new(
                "relative_uom_id",
                FieldType::Many2one {
                    comodel: "uom.uom".into(),
                },
            ),
            Field::new(
                "related_uom_ids",
                FieldType::One2many {
                    comodel: "uom.uom".into(),
                    inverse: "relative_uom_id".into(),
                },
            ),
            // deliberately not stored: a stored factor goes stale the
            // moment somebody edits a unit higher up the chain, and a
            // stale conversion factor is worse than a read per link
            Field::new("factor", RATIO)
                .computed(&["relative_factor", "relative_uom_id.factor"], factor),
            // the root of the chain, which is what Odoo 19 replaced
            // uom.category with — the label a list needs to show that
            // "kg" and "hours" are not the same kind of thing
            Field::new("reference_name", FieldType::Char { size: None })
                .computed(&["name", "relative_uom_id.reference_name"], reference_name),
            Field::new("rounding", RATIO).default_value(json!(DEFAULT_ROUNDING)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // ordering only: the smaller the unit, the earlier it shows
            Field::new("sequence", FieldType::Integer)
                .computed(&["relative_factor"], sequence)
                .store(),
        ],
    )
    .constrained(
        "usable conversion factor",
        &["relative_factor", "relative_uom_id"],
        factor_is_usable,
    )
    .constrained(
        "usable rounding",
        &["rounding"],
        rounding_is_usable,
    )
    .constrained(
        "non-circular reference",
        &["relative_uom_id"],
        is_not_its_own_reference,
    )
}

// ── computes ────────────────────────────────────────────────────────

/// `factor` — how many units of the root reference one of this contains.
///
/// The chain multiplies: a ton is 1000 kg, a kg is 1000 g, so a ton is
/// 1_000_000 g. That is the number both sides of a conversion are said
/// in, which is why the conversion itself is a division.
fn factor(record: &Map<String, Value>) -> Value {
    let relative = number(record, "relative_factor");
    // a many2one dependency arrives as a one-element list
    let reference = record
        .get("relative_uom_id.factor")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(as_number)
        .filter(|value| *value != 0.0);
    json!(reference.map_or(relative, |reference| relative * reference))
}

/// `reference_name` — the name of the unit at the end of the chain.
fn reference_name(record: &Map<String, Value>) -> Value {
    let inherited = record
        .get("relative_uom_id.reference_name")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_str);
    json!(inherited.unwrap_or_else(|| record.get("name").and_then(Value::as_str).unwrap_or("")))
}

/// `sequence` — Odoo's `_compute_sequence`, kept because it is what puts
/// grams before kilos in a list nobody sorted by hand. Capped, or a unit
/// with a huge factor would sit alone at the bottom forever.
fn sequence(record: &Map<String, Value>) -> Value {
    let ordered = (number(record, "relative_factor") * 100.0).trunc() as i64;
    json!(ordered.min(1000))
}

// ── constraints ─────────────────────────────────────────────────────

/// Odoo checks `relative_factor != 0` in SQL. Zero is not the only
/// unusable value: a negative factor is the same illness with a sign,
/// and it would turn every converted quantity upside down.
fn factor_is_usable(record: &Map<String, Value>) -> Result<(), String> {
    let relative = number(record, "relative_factor");
    if relative <= 0.0 {
        return Err(format!(
            "a unit's conversion factor must be greater than zero (got {relative})"
        ));
    }
    if first_id(record.get("relative_uom_id").unwrap_or(&Value::Null)).is_none() && relative != 1.0
    {
        return Err(format!(
            "a unit with no reference unit is its category's own reference and \
             precisa ter fator 1: informe a reference unit ou volte o fator de {relative} \
             para 1"
        ));
    }
    Ok(())
}

/// A rounding of zero would ask for a division by zero on every
/// conversion; a negative one has no meaning at all.
fn rounding_is_usable(record: &Map<String, Value>) -> Result<(), String> {
    let rounding = number(record, "rounding");
    if rounding <= 0.0 {
        return Err(format!(
            "the rounding precision must be greater than zero (got {rounding}); use \
             0.01 for two decimals or 1 for whole units"
        ));
    }
    Ok(())
}

/// A unit that references itself is a chain with no end: reading its
/// factor would walk forever. Longer loops are caught when a conversion
/// walks the chain, but this one is cheap to refuse at the source.
fn is_not_its_own_reference(record: &Map<String, Value>) -> Result<(), String> {
    let id = record.get("id").and_then(Value::as_i64);
    let reference = first_id(record.get("relative_uom_id").unwrap_or(&Value::Null));
    if id.is_some() && id == reference {
        return Err(
            "a unit cannot be its own reference unit: pick another unit \
             or leave the field empty"
                .into(),
        );
    }
    Ok(())
}

// ── the arithmetic ──────────────────────────────────────────────────

/// How a converted quantity meets the destination unit's precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingMethod {
    /// away from zero — what Odoo converts with by default
    Up,
    Down,
    HalfUp,
}

impl RoundingMethod {
    /// Odoo names these in data and in RPC calls, so the port answers to
    /// the same spellings.
    pub fn parse(name: &str) -> Result<Self, RusdooError> {
        match name.to_ascii_uppercase().as_str() {
            "UP" => Ok(RoundingMethod::Up),
            "DOWN" => Ok(RoundingMethod::Down),
            "HALF-UP" | "HALF_UP" => Ok(RoundingMethod::HalfUp),
            other => Err(RusdooError::Validation(format!(
                "rounding {other:?} does not exist: use \"UP\", \"DOWN\" ou \"HALF-UP\""
            ))),
        }
    }
}

/// Binary floats do not divide cleanly: `8 / 1.6` lands on
/// `4.999999999999999`, and rounding that down loses a whole unit. Both
/// the ratio and the product are pulled back onto the grid before and
/// after the rounding decision, which is what Odoo's `float_round` does
/// with its own epsilon.
const NOISE_GRID: f64 = 1e12;

fn denoise(value: f64) -> f64 {
    (value * NOISE_GRID).round() / NOISE_GRID
}

/// Round `value` to a multiple of `rounding`.
///
/// A rounding of zero or less means "do not round" rather than a panic:
/// the constraint keeps stored units above zero, and a caller passing
/// nothing should get the exact number back, not a division by zero.
pub fn round_to(value: f64, rounding: f64, method: RoundingMethod) -> f64 {
    if rounding <= 0.0 || !value.is_finite() {
        return value;
    }
    let scaled = denoise(value / rounding);
    let stepped = match method {
        RoundingMethod::Up => scaled.abs().ceil() * scaled.signum(),
        RoundingMethod::Down => scaled.abs().floor() * scaled.signum(),
        RoundingMethod::HalfUp => scaled.abs().round() * scaled.signum(),
    };
    denoise(stepped * rounding)
}

/// `qty` of a unit worth `from_factor` reference units, said in a unit
/// worth `to_factor` of them.
pub fn convert(qty: f64, from_factor: f64, to_factor: f64) -> Result<f64, RusdooError> {
    if to_factor == 0.0 {
        return Err(RusdooError::Validation(
            "a unidade de destino tem fator zero: conserte o fator dela antes de converter".into(),
        ));
    }
    Ok(denoise(qty * from_factor / to_factor))
}

/// A price travels the other way round: what costs 24 per dozen costs 2
/// per unit. Dividing the quantity and dividing the price by the same
/// number would leave the total wrong.
pub fn convert_price_between(
    price: f64,
    from_factor: f64,
    to_factor: f64,
) -> Result<f64, RusdooError> {
    if from_factor == 0.0 {
        return Err(RusdooError::Validation(
            "a unidade de origem tem fator zero: conserte o fator dela antes de converter".into(),
        ));
    }
    Ok(denoise(price * to_factor / from_factor))
}

// ── the methods ─────────────────────────────────────────────────────

/// What a conversion needs to know about one unit.
struct UnitFacts {
    name: String,
    factor: f64,
    rounding: f64,
}

/// Read a unit, refusing loudly when it is not there — a conversion
/// against a unit that does not exist must not quietly return the
/// original quantity.
async fn facts(ctx: &MethodCtx<'_>, id: i64) -> Result<UnitFacts, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, "uom.uom", &[id], &["name", "factor", "rounding"])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| RusdooError::Validation(format!("unit of measure {id} does not exist")))?;
    Ok(UnitFacts {
        name: row
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        factor: number(row, "factor"),
        rounding: number(row, "rounding"),
    })
}

/// Walk up the reference chain and answer where it ends. That root is
/// the category: two units convert if and only if they share it.
async fn reference_root(ctx: &MethodCtx<'_>, start: i64) -> Result<(i64, String), RusdooError> {
    let mut current = start;
    for _ in 0..MAX_CHAIN {
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "uom.uom",
                &[current],
                &["name", "relative_uom_id"],
            )
            .await?;
        let row = rows.first().ok_or_else(|| {
            RusdooError::Validation(format!("unit of measure {current} does not exist"))
        })?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match first_id(row.get("relative_uom_id").unwrap_or(&Value::Null)) {
            Some(reference) if reference != current => current = reference,
            _ => return Ok((current, name)),
        }
    }
    Err(RusdooError::Validation(format!(
        "unit of measure {start} is in a circular reference chain: fix the reference \
         unit before converting"
    )))
}

/// Both units must be measurable against the same thing. This is the
/// refusal the whole module exists for: converting kilos into hours
/// gives a number, and a number nobody can defend is how wrong stock
/// levels get written.
async fn ensure_same_reference(
    ctx: &MethodCtx<'_>,
    from: (i64, &str),
    to: (i64, &str),
) -> Result<(), RusdooError> {
    let (from_root, from_root_name) = reference_root(ctx, from.0).await?;
    let (to_root, to_root_name) = reference_root(ctx, to.0).await?;
    if from_root == to_root {
        return Ok(());
    }
    Err(RusdooError::Validation(format!(
        "cannot convert {} into {}: {} is measured in {from_root_name} and {} in {to_root_name}, \
         which are different categories",
        from.1, to.1, from.1, to.1
    )))
}

/// The single unit a `call_kw` was made on.
fn only_id(ctx: &MethodCtx<'_>) -> Result<i64, RusdooError> {
    match ctx.ids[..] {
        [id] => Ok(id),
        _ => Err(RusdooError::Validation(
            "convert from one unit of measure at a time".into(),
        )),
    }
}

/// The destination unit, given positionally like Odoo's
/// `_compute_quantity(qty, to_unit)` or by name.
/// `ctx.rest` already comes without the recordset, so the method's
/// first argument is the quantity and the second is the target unit.
fn target_id(args: &[Value], kwargs: &Map<String, Value>) -> Result<i64, RusdooError> {
    args.get(1)
        .or_else(|| kwargs.get("to_uom_id"))
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            RusdooError::Validation(
                "say which unit of measure to convert into (second argument, or \"to_uom_id\")"
                    .into(),
            )
        })
}

fn amount(args: &[Value], kwargs: &Map<String, Value>, name: &str) -> Result<f64, RusdooError> {
    args.first()
        .or_else(|| kwargs.get(name))
        .and_then(as_number)
        .ok_or_else(|| {
            RusdooError::Validation(format!(
                "say which {name} to convert (first argument, or {name:?})"
            ))
        })
}

/// `convert_quantity(qty, to_uom_id)` — Odoo's `_compute_quantity`.
///
/// Rounds up by default, like Odoo: a quantity converted to reserve or
/// to ship must never come out below what was asked for. `round: false`
/// gives the raw ratio back, for callers that will round once at the end
/// of a longer sum.
fn convert_quantity<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let from_id = only_id(&ctx)?;
        // the method's arguments come after the recordset
        let to_id = target_id(&ctx.rest, kwargs)?;
        let qty = amount(&ctx.rest, kwargs, "qty")?;
        let from = facts(&ctx, from_id).await?;
        if from_id == to_id {
            // same unit, same number: no rounding, nothing to lose
            return Ok(json!(qty));
        }
        let to = facts(&ctx, to_id).await?;
        ensure_same_reference(&ctx, (from_id, &from.name), (to_id, &to.name)).await?;

        let converted = convert(qty, from.factor, to.factor)?;
        let wants_rounding = kwargs.get("round").and_then(Value::as_bool).unwrap_or(true);
        if !wants_rounding {
            return Ok(json!(converted));
        }
        let method = match kwargs.get("rounding_method").and_then(Value::as_str) {
            Some(name) => RoundingMethod::parse(name)?,
            None => RoundingMethod::Up,
        };
        Ok(json!(round_to(converted, to.rounding, method)))
    })
}

/// `convert_price(price, to_uom_id)` — Odoo's `_compute_price`.
///
/// Never rounded: a unit price is an input to a total, and rounding it
/// before the multiplication is how a line ends up off by cents.
fn convert_price<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let from_id = only_id(&ctx)?;
        let to_id = target_id(&ctx.rest, kwargs)?;
        let price = amount(&ctx.rest, kwargs, "price")?;
        if from_id == to_id {
            return Ok(json!(price));
        }
        let from = facts(&ctx, from_id).await?;
        let to = facts(&ctx, to_id).await?;
        ensure_same_reference(&ctx, (from_id, &from.name), (to_id, &to.name)).await?;
        Ok(json!(convert_price_between(price, from.factor, to.factor)?))
    })
}

// ── reading values back out of a record ─────────────────────────────

/// A numeric field's value, whatever shape the driver decoded it in.
fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record.get(name).and_then(as_number).unwrap_or(0.0)
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_multiplies_into_one_absolute_factor() {
        // a kilo is 1000 g; a ton is 1000 kg, so a ton is 1_000_000 g
        let mut ton = Map::new();
        ton.insert("relative_factor".into(), json!(1000.0));
        ton.insert("relative_uom_id.factor".into(), json!([1000.0]));
        assert_eq!(factor(&ton), json!(1_000_000.0));

        // a unit with no reference is its own: the factor stands alone
        let mut gram = Map::new();
        gram.insert("relative_factor".into(), json!(1.0));
        assert_eq!(factor(&gram), json!(1.0));
    }

    #[test]
    fn the_category_of_a_unit_is_the_root_of_its_chain() {
        let mut ton = Map::new();
        ton.insert("name".into(), json!("t"));
        ton.insert("relative_uom_id.reference_name".into(), json!(["g"]));
        assert_eq!(reference_name(&ton), json!("g"));

        let mut gram = Map::new();
        gram.insert("name".into(), json!("g"));
        assert_eq!(reference_name(&gram), json!("g"));
    }

    #[test]
    fn odoos_own_conversion_cases_come_out_the_same() {
        // 1_020_000 g is 1.02 t (odoo/addons/uom/tests/test_uom.py)
        let converted = convert(1_020_000.0, 1.0, 1_000_000.0).unwrap();
        assert_eq!(round_to(converted, 0.01, RoundingMethod::Up), 1.02);

        // 1234 g is 1.234 kg, and rounding up to two digits gives 1.24
        let converted = convert(1234.0, 1.0, 1000.0).unwrap();
        assert_eq!(round_to(converted, 0.01, RoundingMethod::Up), 1.24);

        // one dozen is exactly twelve units — not 13, which is what a
        // stored 1/12 factor plus rounding up used to produce
        let converted = convert(1.0, 12.0, 1.0).unwrap();
        assert_eq!(round_to(converted, 0.01, RoundingMethod::Up), 12.0);

        // and a price goes the other way: 2 per gram is 2M per ton
        assert_eq!(
            convert_price_between(2.0, 1.0, 1_000_000.0).unwrap(),
            2_000_000.0
        );
    }

    #[test]
    fn rounding_is_a_choice_the_caller_makes() {
        // two units of something sold by the score, rounded to whole
        // scores: up gives one, down gives none
        let converted = convert(2.0, 1.0, 20.0).unwrap();
        assert_eq!(round_to(converted, 1.0, RoundingMethod::Up), 1.0);
        assert_eq!(round_to(converted, 1.0, RoundingMethod::Down), 0.0);
        assert_eq!(round_to(converted, 1.0, RoundingMethod::HalfUp), 0.0);

        // the binary tail of 8 / 1.6 must not cost a whole step
        assert_eq!(round_to(8.0, 1.6, RoundingMethod::Up), 8.0);
        // half-up is away from zero, on both sides
        assert_eq!(round_to(1.005, 0.01, RoundingMethod::HalfUp), 1.01);
        assert_eq!(round_to(-1.005, 0.01, RoundingMethod::HalfUp), -1.01);
        // and asking for no precision returns the number untouched
        assert_eq!(round_to(1.234, 0.0, RoundingMethod::Up), 1.234);
    }

    #[test]
    fn a_unit_nobody_can_convert_with_is_refused() {
        let mut zero = Map::new();
        zero.insert("relative_factor".into(), json!(0));
        assert!(factor_is_usable(&zero).is_err());

        let mut negative = Map::new();
        negative.insert("relative_factor".into(), json!(-3));
        assert!(factor_is_usable(&negative).is_err());

        // no reference means it is the reference, and a reference that
        // contains twelve of itself is nonsense
        let mut orphan = Map::new();
        orphan.insert("relative_factor".into(), json!(12));
        let refusal = factor_is_usable(&orphan).expect_err("refused");
        assert!(refusal.contains("reference unit"), "{refusal}");

        let mut child = Map::new();
        child.insert("relative_factor".into(), json!(12));
        child.insert("relative_uom_id".into(), json!([7, "Unidades"]));
        assert!(factor_is_usable(&child).is_ok());
    }

    #[test]
    fn a_unit_cannot_be_its_own_reference() {
        let mut itself = Map::new();
        itself.insert("id".into(), json!(4));
        itself.insert("relative_uom_id".into(), json!([4, "Unidades"]));
        assert!(is_not_its_own_reference(&itself).is_err());

        let mut other = Map::new();
        other.insert("id".into(), json!(4));
        other.insert("relative_uom_id".into(), json!([3, "Unidades"]));
        assert!(is_not_its_own_reference(&other).is_ok());
    }

    #[test]
    fn the_model_registers_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let uom = reg.get("uom.uom").expect("registered");
        assert!(uom.field("name").is_some_and(|f| f.required));
        // the absolute factor is derived, never a column somebody may
        // edit out from under a conversion
        let factor = uom.field("factor").expect("factor");
        assert!(factor.compute.is_some() && !factor.stored);
    }

    #[test]
    fn the_unit_answers_the_two_conversion_calls() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("uom.uom"),
            vec!["convert_price", "convert_quantity"]
        );
    }
}
