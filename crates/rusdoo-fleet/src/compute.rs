//! The derived values of the fleet, as pure functions.
//!
//! Everything here reads a record the ORM has already assembled and
//! answers a value. Keeping them apart from the model declarations is
//! what makes them testable without a database — and a contract that
//! expires in three days is exactly the kind of arithmetic that deserves
//! a test rather than a careful reading.

use chrono::NaiveDate;
use serde_json::{json, Map, Value};

/// How long before a contract runs out the fleet manager is warned.
///
/// Odoo reads `hr_fleet.delay_alert_contract` off `ir.config_parameter`
/// and falls back to 30. A compute here is a pure function with no
/// database in reach, so the fallback is all there is; the parameter is
/// noted in the port's report as something the framework has to hand a
/// compute before this can honour it.
pub const DELAY_ALERT_CONTRACT_DAYS: i64 = 30;

/// What a vehicle with no plate is called on a list
/// (`_compute_vehicle_name`).
pub const NO_PLATE: &str = "No Plate";

/// The values of a dotted dependency, which the ORM always hands over as
/// a list — one entry per record the relation points at, and a
/// one-element list when the hop was a many2one.
pub fn many(record: &Map<String, Value>, path: &str) -> Vec<Value> {
    record
        .get(path)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The single value behind a many2one dependency.
pub fn one(record: &Map<String, Value>, path: &str) -> Option<Value> {
    many(record, path)
        .into_iter()
        .find(|value| !value.is_null())
}

/// A dotted dependency's text, empty when the hop found nothing.
fn text_at(record: &Map<String, Value>, path: &str) -> String {
    one(record, path)
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A plain field's text.
fn text(record: &Map<String, Value>, name: &str) -> String {
    record
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A date column, which reads back as `"YYYY-MM-DD"`.
pub fn date_of(value: &Value) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.as_str()?, "%Y-%m-%d").ok()
}

/// A numeric column, whatever shape the driver decoded it in — `numeric`
/// comes back as text on some paths and as a float on others.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Today, in UTC.
///
/// Odoo asks the user's timezone (`fields.Date.context_today`); this ORM
/// has no timezone to ask yet, so a manager three hours west of UTC sees
/// a contract turn overdue a few hours early. That is the timezone
/// field's job to fix, not this function's to guess.
fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

// ---------------------------------------------------------------------
// fleet.vehicle
// ---------------------------------------------------------------------

/// `name` — what a vehicle is called everywhere it is listed.
///
/// Odoo builds it out of three parts and keeps the separators even when
/// a part is missing, so a vehicle that is only half filled in still
/// sorts next to its brand.
pub fn vehicle_name(record: &Map<String, Value>) -> Value {
    let brand = text_at(record, "model_id.brand_id.name");
    let model = text_at(record, "model_id.name");
    let plate = match text(record, "license_plate") {
        plate if plate.is_empty() => NO_PLATE.to_string(),
        plate => plate,
    };
    json!(format!("{brand}/{model}/{plate}"))
}

/// `co2_emission_unit` — grams per kilometre, or per mile.
///
/// It follows the range unit and nothing else: a vehicle whose range is
/// quoted in miles has its emissions quoted the same way, or the two
/// numbers on the form are in different systems with nothing saying so.
pub fn emission_unit(record: &Map<String, Value>) -> Value {
    match record.get("range_unit").and_then(Value::as_str) {
        Some("mi") => json!("g/mi"),
        _ => json!("g/km"),
    }
}

/// `odometer` — the highest reading the vehicle has ever had.
///
/// Odoo searches the logs ordered by value and takes the first. The
/// logs are already in reach here as a dependency, so this is a maximum
/// over them and not a query.
pub fn last_odometer(record: &Map<String, Value>) -> Value {
    let highest = many(record, "odometer_ids.value")
        .iter()
        .filter_map(number)
        .fold(f64::NEG_INFINITY, f64::max);
    json!(if highest.is_finite() { highest } else { 0.0 })
}

/// How many records a to-many dependency points at — the number on a
/// smart button.
pub fn count_of(record: &Map<String, Value>, field: &str) -> Value {
    json!(record
        .get(field)
        .and_then(Value::as_array)
        .map_or(0, Vec::len))
}

pub fn odometer_count(record: &Map<String, Value>) -> Value {
    count_of(record, "odometer_ids")
}

pub fn service_count(record: &Map<String, Value>) -> Value {
    count_of(record, "log_services")
}

pub fn history_count(record: &Map<String, Value>) -> Value {
    count_of(record, "log_drivers")
}

/// `contract_count` — the contracts that still mean something.
///
/// Cancelled ones are left out, like Odoo's `state != 'closed'`: the
/// button is an invitation to look at what is running, and a vehicle
/// with nine cancelled contracts and none open is not a vehicle with
/// nine contracts.
pub fn contract_count(record: &Map<String, Value>) -> Value {
    let live = many(record, "log_contracts.state")
        .iter()
        .filter(|state| state.as_str() != Some(CLOSED))
        .count();
    json!(live)
}

/// A contract state as the model spells it.
pub const FUTURE: &str = "futur";
pub const OPEN: &str = "open";
pub const EXPIRED: &str = "expired";
pub const CLOSED: &str = "closed";

/// The contract that decides what a vehicle's reminder says: among the
/// ones that are not cancelled and do have an expiry, the one that runs
/// out last.
///
/// Odoo groups by state and takes the greatest expiry; taking the latest
/// contract is the same answer and says why: a vehicle whose insurance
/// was renewed for another year is not overdue because last year's
/// policy is still on file.
fn deciding_contract(record: &Map<String, Value>) -> Option<(NaiveDate, String)> {
    let dates = many(record, "log_contracts.expiration_date");
    let states = many(record, "log_contracts.state");
    dates
        .iter()
        .zip(states.iter())
        .filter(|(_, state)| state.as_str() != Some(CLOSED))
        .filter_map(|(date, state)| {
            Some((
                date_of(date)?,
                state.as_str().unwrap_or(OPEN).to_string(),
            ))
        })
        .max_by_key(|(date, _)| *date)
}

/// `contract_renewal_overdue` — a contract ran out and nothing replaced
/// it.
pub fn contract_renewal_overdue(record: &Map<String, Value>) -> Value {
    json!(match deciding_contract(record) {
        Some((expiry, _)) => expiry < today(),
        None => false,
    })
}

/// `contract_renewal_due_soon` — it has not run out yet, but it will
/// within the warning window.
pub fn contract_renewal_due_soon(record: &Map<String, Value>) -> Value {
    let Some((expiry, _)) = deciding_contract(record) else {
        return json!(false);
    };
    let days = (expiry - today()).num_days();
    json!((0..DELAY_ALERT_CONTRACT_DAYS).contains(&days))
}

/// `contract_state` — the state of the contract the reminder is about.
pub fn contract_state(record: &Map<String, Value>) -> Value {
    match deciding_contract(record) {
        Some((_, state)) => json!(state),
        // Odoo writes "" rather than false here; an empty selection reads
        // as unset on the form either way
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------
// fleet.vehicle.model / brand
// ---------------------------------------------------------------------

pub fn vehicle_count(record: &Map<String, Value>) -> Value {
    count_of(record, "vehicle_ids")
}

pub fn model_count(record: &Map<String, Value>) -> Value {
    count_of(record, "model_ids")
}

// ---------------------------------------------------------------------
// fleet.vehicle.odometer
// ---------------------------------------------------------------------

/// `name` — which vehicle was read, and when.
pub fn odometer_name(record: &Map<String, Value>) -> Value {
    let vehicle = text_at(record, "vehicle_id.name");
    let date = text(record, "date");
    json!(match (vehicle.is_empty(), date.is_empty()) {
        (true, _) => date,
        (false, true) => vehicle,
        (false, false) => format!("{vehicle} / {date}"),
    })
}

// ---------------------------------------------------------------------
// fleet.vehicle.log.contract
// ---------------------------------------------------------------------

/// `name` — the kind of contract, then the vehicle it covers.
pub fn contract_name(record: &Map<String, Value>) -> Value {
    let vehicle = text_at(record, "vehicle_id.name");
    let subtype = text_at(record, "cost_subtype_id.name");
    if vehicle.is_empty() || subtype.is_empty() {
        return json!(vehicle);
    }
    json!(format!("{subtype} {vehicle}"))
}

/// How many days a contract has left, as Odoo counts them: `-1` once it
/// is cancelled or has no expiry at all, `0` when it is already overdue,
/// otherwise the days remaining.
///
/// The two meanings of a non-positive answer are deliberate in Odoo: a
/// list sorted by this column puts what needs attention today at the top
/// and the contracts nobody has to think about at the bottom.
fn days_remaining(record: &Map<String, Value>) -> Option<i64> {
    let state = record.get("state").and_then(Value::as_str)?;
    if state != OPEN && state != EXPIRED {
        return None;
    }
    let expiry = record.get("expiration_date").and_then(date_of)?;
    Some((expiry - today()).num_days())
}

pub fn days_left(record: &Map<String, Value>) -> Value {
    match days_remaining(record) {
        Some(days) => json!(days.max(0)),
        None => json!(-1),
    }
}

pub fn expires_today(record: &Map<String, Value>) -> Value {
    json!(days_remaining(record) == Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Value) -> Map<String, Value> {
        match pairs {
            Value::Object(map) => map,
            _ => panic!("a record is an object"),
        }
    }

    /// A date `days` from today, as the column reads back.
    fn in_days(days: i64) -> Value {
        json!((today() + chrono::Duration::days(days)).to_string())
    }

    #[test]
    fn a_vehicle_is_named_after_its_brand_model_and_plate() {
        let vehicle = record(json!({
            "model_id.brand_id.name": ["Audi"],
            "model_id.name": ["A3"],
            "license_plate": "ABC-1234",
        }));
        assert_eq!(vehicle_name(&vehicle), json!("Audi/A3/ABC-1234"));
    }

    #[test]
    fn a_vehicle_without_a_plate_says_so_instead_of_going_blank() {
        // a car that was ordered but not registered yet has no plate, and
        // a name ending in "//" is a row nobody can tell from the next
        let vehicle = record(json!({
            "model_id.brand_id.name": ["Audi"],
            "model_id.name": ["A3"],
        }));
        assert_eq!(vehicle_name(&vehicle), json!("Audi/A3/No Plate"));
    }

    #[test]
    fn emissions_are_quoted_in_the_same_system_as_the_range() {
        assert_eq!(emission_unit(&record(json!({"range_unit": "km"}))), json!("g/km"));
        assert_eq!(emission_unit(&record(json!({"range_unit": "mi"}))), json!("g/mi"));
        // a model that never chose falls back to the metric default
        assert_eq!(emission_unit(&Map::new()), json!("g/km"));
    }

    #[test]
    fn the_odometer_is_the_highest_reading_ever_taken() {
        let vehicle = record(json!({"odometer_ids.value": [12000.0, 45000.0, 30000.0]}));
        assert_eq!(last_odometer(&vehicle), json!(45000.0));
        // a vehicle nobody read yet is at zero, not null: the form shows a
        // number
        assert_eq!(last_odometer(&Map::new()), json!(0.0));
    }

    #[test]
    fn cancelled_contracts_do_not_count_towards_the_button() {
        let vehicle = record(json!({"log_contracts.state": ["open", "closed", "futur"]}));
        assert_eq!(contract_count(&vehicle), json!(2));
    }

    #[test]
    fn a_contract_that_ran_out_makes_the_vehicle_overdue() {
        let vehicle = record(json!({
            "log_contracts.expiration_date": [in_days(-10)],
            "log_contracts.state": ["open"],
        }));
        assert_eq!(contract_renewal_overdue(&vehicle), json!(true));
        assert_eq!(contract_renewal_due_soon(&vehicle), json!(false));
        assert_eq!(contract_state(&vehicle), json!("open"));
    }

    #[test]
    fn a_renewed_contract_clears_the_overdue_flag() {
        // Odoo's own test: an expired policy plus a fresh one is not a
        // vehicle anybody has to chase
        let vehicle = record(json!({
            "log_contracts.expiration_date": [in_days(-2), in_days(365)],
            "log_contracts.state": ["expired", "open"],
        }));
        assert_eq!(contract_renewal_overdue(&vehicle), json!(false));
        assert_eq!(contract_renewal_due_soon(&vehicle), json!(false));
    }

    #[test]
    fn a_contract_inside_the_warning_window_is_due_soon() {
        let vehicle = record(json!({
            "log_contracts.expiration_date": [in_days(10)],
            "log_contracts.state": ["open"],
        }));
        assert_eq!(contract_renewal_due_soon(&vehicle), json!(true));
        assert_eq!(contract_renewal_overdue(&vehicle), json!(false));
    }

    #[test]
    fn a_contract_beyond_the_window_raises_nothing() {
        let vehicle = record(json!({
            "log_contracts.expiration_date": [in_days(DELAY_ALERT_CONTRACT_DAYS + 1)],
            "log_contracts.state": ["open"],
        }));
        assert_eq!(contract_renewal_due_soon(&vehicle), json!(false));
        assert_eq!(contract_renewal_overdue(&vehicle), json!(false));
    }

    #[test]
    fn a_cancelled_contract_decides_nothing() {
        let vehicle = record(json!({
            "log_contracts.expiration_date": [in_days(-100)],
            "log_contracts.state": ["closed"],
        }));
        assert_eq!(contract_renewal_overdue(&vehicle), json!(false));
        assert_eq!(contract_state(&vehicle), Value::Null);
    }

    #[test]
    fn a_vehicle_with_no_contract_at_all_is_quiet() {
        assert_eq!(contract_renewal_overdue(&Map::new()), json!(false));
        assert_eq!(contract_renewal_due_soon(&Map::new()), json!(false));
        assert_eq!(contract_state(&Map::new()), Value::Null);
    }

    #[test]
    fn days_left_counts_down_and_stops_at_zero() {
        let running = record(json!({"state": "open", "expiration_date": in_days(5)}));
        assert_eq!(days_left(&running), json!(5));
        assert_eq!(expires_today(&running), json!(false));

        let expiring = record(json!({"state": "open", "expiration_date": in_days(0)}));
        assert_eq!(days_left(&expiring), json!(0));
        assert_eq!(expires_today(&expiring), json!(true));

        // already overdue: zero, not a negative number nobody sorts by
        let overdue = record(json!({"state": "expired", "expiration_date": in_days(-7)}));
        assert_eq!(days_left(&overdue), json!(0));
        assert_eq!(expires_today(&overdue), json!(false));
    }

    #[test]
    fn a_cancelled_contract_has_no_countdown() {
        let cancelled = record(json!({"state": "closed", "expiration_date": in_days(5)}));
        assert_eq!(days_left(&cancelled), json!(-1));
        assert_eq!(expires_today(&cancelled), json!(false));
        // and neither has one that never had an expiry
        let endless = record(json!({"state": "open"}));
        assert_eq!(days_left(&endless), json!(-1));
    }

    #[test]
    fn a_contract_is_named_after_its_type_and_vehicle() {
        let contract = record(json!({
            "vehicle_id.name": ["Audi/A3/ABC-1234"],
            "cost_subtype_id.name": ["Leasing"],
        }));
        assert_eq!(contract_name(&contract), json!("Leasing Audi/A3/ABC-1234"));
        // without a type it is just the vehicle, like Odoo
        let untyped = record(json!({"vehicle_id.name": ["Audi/A3/ABC-1234"]}));
        assert_eq!(untyped_name(&untyped), "Audi/A3/ABC-1234");
    }

    fn untyped_name(record: &Map<String, Value>) -> String {
        contract_name(record).as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn an_odometer_reading_is_named_after_the_vehicle_and_the_day() {
        let reading = record(json!({
            "vehicle_id.name": ["Audi/A3/ABC-1234"],
            "date": "2026-08-03",
        }));
        assert_eq!(odometer_name(&reading), json!("Audi/A3/ABC-1234 / 2026-08-03"));
        // a reading whose vehicle is gone still says when it was taken
        let orphan = record(json!({"date": "2026-08-03"}));
        assert_eq!(odometer_name(&orphan), json!("2026-08-03"));
    }
}
