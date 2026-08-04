//! The buttons of the fleet, and the job that runs on a clock.
//!
//! Odoo hangs most of this off `create` and `write` overrides. This ORM
//! has no such hook, so what a user does is a method a client calls by
//! name — which is also what the cron runs, and what a test drives.

use crate::compute::{CLOSED, EXPIRED, FUTURE, OPEN};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use serde_json::{json, Map, Value};

/// The specifications a vehicle copies from its model, as
/// `(model field, vehicle field)` — Odoo's `MODEL_FIELDS_TO_VEHICLE`.
/// Most names match; the ones that do not are why the table exists.
pub const MODEL_FIELDS_TO_VEHICLE: [(&str, &str); 15] = [
    ("transmission", "transmission"),
    ("model_year", "model_year"),
    ("electric_assistance", "electric_assistance"),
    ("color", "color"),
    ("seats", "seats"),
    ("doors", "doors"),
    ("trailer_hook", "trailer_hook"),
    ("default_co2", "co2"),
    ("co2_standard", "co2_standard"),
    ("default_fuel_type", "fuel_type"),
    ("power", "power"),
    ("horsepower", "horsepower"),
    ("horsepower_tax", "horsepower_tax"),
    ("category_id", "category_id"),
    ("vehicle_range", "vehicle_range"),
];

/// Units the model and the vehicle spell the same way, copied along with
/// the numbers they qualify: a range of 400 means nothing without
/// knowing whether it is kilometres or miles.
const MODEL_UNITS_TO_VEHICLE: [(&str, &str); 2] =
    [("power_unit", "power_unit"), ("range_unit", "range_unit")];

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "fleet.vehicle",
        "action_load_from_model",
        Operation::Write,
        action_load_from_model,
    )?;
    methods.register(
        "fleet.vehicle",
        "update_odometer",
        Operation::Write,
        update_odometer,
    )?;
    methods.register(
        "fleet.vehicle",
        "action_assign_driver",
        Operation::Write,
        action_assign_driver,
    )?;
    methods.register(
        "fleet.vehicle",
        "action_plan_driver_change",
        Operation::Write,
        action_plan_driver_change,
    )?;
    methods.register(
        "fleet.vehicle",
        "action_accept_driver_change",
        Operation::Write,
        action_accept_driver_change,
    )?;
    methods.register(
        "fleet.vehicle",
        "action_archive",
        Operation::Write,
        action_archive,
    )?;
    // looking at the history changes nothing about the vehicle
    methods.register(
        "fleet.vehicle",
        "open_assignation_logs",
        Operation::Read,
        open_assignation_logs,
    )?;
    for (name, button) in [
        ("action_open", action_open as rusdoo_orm::methods::MethodFn),
        ("action_close", action_close),
        ("action_draft", action_draft),
        ("action_expire", action_expire),
    ] {
        methods.register("fleet.vehicle.log.contract", name, Operation::Write, button)?;
    }
    methods.register(
        "fleet.vehicle.log.contract",
        "scheduler_manage_contract_expiration",
        Operation::Write,
        scheduler_manage_contract_expiration,
    )?;
    // Odoo's `ir.cron` row calls `run_scheduler`, which forwards. Keeping
    // the alias means the data file reads like Odoo's.
    methods.register(
        "fleet.vehicle.log.contract",
        "run_scheduler",
        Operation::Write,
        scheduler_manage_contract_expiration,
    )?;
    methods.register(
        "fleet.vehicle.model",
        "action_model_vehicle",
        Operation::Read,
        action_model_vehicle,
    )?;
    methods.register(
        "fleet.vehicle.model.brand",
        "action_brand_model",
        Operation::Read,
        action_brand_model,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// The id inside a many2one, which reads back as `[id, name]`.
pub fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Today, as the wire spells a date.
fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

/// A named argument, whether the client passed it by keyword or by
/// position — the two shapes a call arrives in.
fn wanted<'a>(rest: &'a [Value], kwargs: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    kwargs.get(name).or_else(|| rest.first())
}

/// The single record a button was pressed on.
fn only_one(ids: &[i64], complaint: &str) -> Result<i64, RusdooError> {
    match ids {
        [id] => Ok(*id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

fn no_selection(ids: &[i64], complaint: &str) -> Result<(), RusdooError> {
    if ids.is_empty() {
        return Err(RusdooError::Validation(complaint.to_string()));
    }
    Ok(())
}

/// Read one record, complaining by name when it is not there.
async fn one_row(
    ctx: &MethodCtx<'_>,
    model: &str,
    id: i64,
    fields: &[&str],
) -> Result<Map<String, Value>, RusdooError> {
    ctx.registry
        .read(ctx.pool, model, &[id], fields)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation(format!("{model} {id} does not exist")))
}

async fn search(
    ctx: &MethodCtx<'_>,
    model: &str,
    domain: Value,
) -> Result<Vec<i64>, RusdooError> {
    ctx.registry
        .search(
            ctx.pool,
            model,
            &parse_domain(&domain)?,
            &SearchOptions::default(),
        )
        .await
}

// ---------------------------------------------------------------------
// fleet.vehicle
// ---------------------------------------------------------------------

/// `action_load_from_model` — fill the vehicle in from its model.
///
/// Port of `_load_fields_from_model`, which Odoo runs as a compute on
/// every field it touches. Only a truthy value on the model is copied,
/// exactly as Odoo does: a model that never said how many doors it has
/// must not blank the number somebody typed on the vehicle.
fn action_load_from_model<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        no_selection(&ctx.ids, "choose at least one vehicle to fill in")?;
        let vehicles = ctx
            .registry
            .read(ctx.pool, "fleet.vehicle", &ctx.ids, &["model_id"])
            .await?;
        let mut filled = 0usize;
        for vehicle in &vehicles {
            let (Some(id), Some(model_id)) = (
                vehicle.get("id").and_then(Value::as_i64),
                vehicle.get("model_id").and_then(first_id),
            ) else {
                // a vehicle with no model has nothing to copy from; Odoo
                // filters the same way rather than failing the batch
                continue;
            };
            let wanted: Vec<&str> = MODEL_FIELDS_TO_VEHICLE
                .iter()
                .chain(MODEL_UNITS_TO_VEHICLE.iter())
                .map(|(from, _)| *from)
                .collect();
            let specs = one_row(&ctx, "fleet.vehicle.model", model_id, &wanted).await?;
            let mut values: Vec<(&str, Value)> = Vec::new();
            for (from, to) in MODEL_FIELDS_TO_VEHICLE
                .iter()
                .chain(MODEL_UNITS_TO_VEHICLE.iter())
            {
                let Some(value) = specs.get(*from) else {
                    continue;
                };
                if !is_meaningful(value) {
                    continue;
                }
                // a many2one arrives as [id, name] and has to leave as an id
                let value = match value {
                    Value::Array(_) => match first_id(value) {
                        Some(id) => json!(id),
                        None => continue,
                    },
                    other => other.clone(),
                };
                values.push((to, value));
            }
            if values.is_empty() {
                continue;
            }
            ctx.registry
                .write_as(ctx.pool, ctx.uid, "fleet.vehicle", &[id], values)
                .await?;
            filled += 1;
        }
        Ok(json!(filled))
    })
}

/// Odoo's truthiness, which is what decides whether a model's value is
/// worth copying: zero, false, an empty string and null are all "the
/// model did not say".
fn is_meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `update_odometer(value)` — record a new reading.
///
/// Odoo does this from `write`, where `_set_odometer` creates the log and
/// the write itself refuses a value below the last one. Both halves are
/// here: an odometer that can go down is an odometer nobody can bill a
/// lease against.
fn update_odometer<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let vehicle_id = only_one(&ctx.ids, "record a reading for one vehicle at a time")?;
        let value = wanted(&ctx.rest, kwargs, "value")
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                RusdooError::Validation("say what the odometer reads: pass value".into())
            })?;
        // Odoo's `_set_odometer` refuses to blank a reading, and creating
        // a zero one is the same thing said differently
        if value <= 0.0 {
            return Err(RusdooError::Validation(
                "emptying the odometer value of a vehicle is not allowed".into(),
            ));
        }
        let vehicle = one_row(
            &ctx,
            "fleet.vehicle",
            vehicle_id,
            &["odometer", "driver_id"],
        )
        .await?;
        let current = vehicle.get("odometer").and_then(Value::as_f64).unwrap_or(0.0);
        if value < current {
            return Err(RusdooError::Validation(
                "the odometer value cannot be lower than the previous one".into(),
            ));
        }
        let reading = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle.odometer",
                vec![
                    ("value", json!(value)),
                    ("date", json!(today())),
                    ("vehicle_id", json!(vehicle_id)),
                    ("driver_id", json!(vehicle.get("driver_id").and_then(first_id))),
                ],
            )
            .await?;
        Ok(json!(reading))
    })
}

/// Hand a vehicle to a driver, and keep the history straight.
///
/// Odoo opens a new assignment row and schedules a to-do asking somebody
/// to fill in the previous one's end date. There are no activities in
/// this port, and a history with two rows that both look current is a
/// history that answers "who had the car in March" wrongly — so the
/// previous row is closed here, on the day the new one opens.
async fn hand_over(
    ctx: &MethodCtx<'_>,
    vehicle_id: i64,
    driver: Option<i64>,
) -> Result<(), RusdooError> {
    let vehicle = one_row(ctx, "fleet.vehicle", vehicle_id, &["driver_id"]).await?;
    let previous = vehicle.get("driver_id").and_then(first_id);
    if previous == driver {
        return Ok(());
    }
    let open_logs = search(
        ctx,
        "fleet.vehicle.assignation.log",
        json!([["vehicle_id", "=", vehicle_id], ["date_end", "=", null]]),
    )
    .await?;
    if !open_logs.is_empty() {
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle.assignation.log",
                &open_logs,
                vec![("date_end", json!(today()))],
            )
            .await?;
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "fleet.vehicle",
            &[vehicle_id],
            vec![("driver_id", json!(driver))],
        )
        .await?;
    if let Some(driver) = driver {
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle.assignation.log",
                vec![
                    ("vehicle_id", json!(vehicle_id)),
                    ("driver_id", json!(driver)),
                    ("date_start", json!(today())),
                ],
            )
            .await?;
    }
    Ok(())
}

/// `action_assign_driver(driver_id)` — give the vehicle to somebody.
fn action_assign_driver<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let vehicle_id = only_one(&ctx.ids, "assign one vehicle at a time")?;
        let driver = wanted(&ctx.rest, kwargs, "driver_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| RusdooError::Validation("say who drives it: pass driver_id".into()))?;
        hand_over(&ctx, vehicle_id, Some(driver)).await?;
        Ok(json!(true))
    })
}

/// The other vehicles of `driver`, of the same type as this one — what
/// Odoo marks "plan to change" when a driver is queued for a new one.
///
/// `vehicle_type` lives on the model, not on the vehicle, so it cannot
/// be a search term here: the candidates come back by driver and the
/// type is read off their models.
async fn same_type_vehicles_of(
    ctx: &MethodCtx<'_>,
    driver: i64,
    vehicle_type: &str,
    except: &[i64],
) -> Result<Vec<i64>, RusdooError> {
    let candidates = search(
        ctx,
        "fleet.vehicle",
        json!([["driver_id", "=", driver]]),
    )
    .await?;
    let candidates: Vec<i64> = candidates
        .into_iter()
        .filter(|id| !except.contains(id))
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "fleet.vehicle", &candidates, &["vehicle_type"])
        .await?;
    Ok(rows
        .iter()
        .filter(|row| row.get("vehicle_type").and_then(Value::as_str) == Some(vehicle_type))
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .collect())
}

/// The field that says "this driver is queued for a different one",
/// which Odoo keeps separately per vehicle type.
fn plan_to_change_field(vehicle_type: &str) -> &'static str {
    match vehicle_type {
        "bike" => "plan_to_change_bike",
        _ => "plan_to_change_car",
    }
}

/// `action_plan_driver_change(driver_id)` — queue somebody for this
/// vehicle.
///
/// Port of what Odoo's `create`/`write` do when `future_driver_id` is
/// set: the vehicle the person drives *today* is flagged, so the fleet
/// manager sees on their own board that it is about to come back.
fn action_plan_driver_change<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let vehicle_id = only_one(&ctx.ids, "queue a driver for one vehicle at a time")?;
        let driver = wanted(&ctx.rest, kwargs, "driver_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| RusdooError::Validation("say who is next: pass driver_id".into()))?;
        let vehicle = one_row(&ctx, "fleet.vehicle", vehicle_id, &["vehicle_type"]).await?;
        let vehicle_type = vehicle
            .get("vehicle_type")
            .and_then(Value::as_str)
            .unwrap_or("car")
            .to_string();
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle",
                &[vehicle_id],
                vec![("future_driver_id", json!(driver))],
            )
            .await?;
        let current = same_type_vehicles_of(&ctx, driver, &vehicle_type, &[vehicle_id]).await?;
        if !current.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "fleet.vehicle",
                    &current,
                    vec![(plan_to_change_field(&vehicle_type), json!(true))],
                )
                .await?;
        }
        Ok(json!(current.len()))
    })
}

/// `action_accept_driver_change` — the queued driver takes the vehicle.
///
/// The vehicle they had is released first, then this one is handed over.
/// Both halves matter: the point of the button is that one person does
/// not end up holding two cars.
fn action_accept_driver_change<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let vehicle_id = only_one(&ctx.ids, "accept one driver change at a time")?;
        let vehicle = one_row(
            &ctx,
            "fleet.vehicle",
            vehicle_id,
            &["future_driver_id", "vehicle_type"],
        )
        .await?;
        let future_driver = vehicle
            .get("future_driver_id")
            .and_then(first_id)
            .ok_or_else(|| {
                RusdooError::Validation("no driver is queued for this vehicle".into())
            })?;
        let vehicle_type = vehicle
            .get("vehicle_type")
            .and_then(Value::as_str)
            .unwrap_or("car")
            .to_string();

        let released =
            same_type_vehicles_of(&ctx, future_driver, &vehicle_type, &[vehicle_id]).await?;
        for id in &released {
            hand_over(&ctx, *id, None).await?;
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "fleet.vehicle",
                    &[*id],
                    vec![
                        ("plan_to_change_car", json!(false)),
                        ("plan_to_change_bike", json!(false)),
                    ],
                )
                .await?;
        }

        hand_over(&ctx, vehicle_id, Some(future_driver)).await?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle",
                &[vehicle_id],
                vec![
                    ("future_driver_id", Value::Null),
                    ("plan_to_change_car", json!(false)),
                    ("plan_to_change_bike", json!(false)),
                ],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `action_archive` — take the vehicle off the board, and its logs with
/// it.
///
/// Port of what `write` does when `active` goes false. A vehicle that is
/// gone but whose contracts still show up on the renewal list is a
/// reminder nobody can act on.
fn action_archive<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        no_selection(&ctx.ids, "choose at least one vehicle to archive")?;
        for model in ["fleet.vehicle.log.contract", "fleet.vehicle.log.services"] {
            let logs = search(&ctx, model, json!([["vehicle_id", "in", ctx.ids]])).await?;
            if logs.is_empty() {
                continue;
            }
            ctx.registry
                .write_as(ctx.pool, ctx.uid, model, &logs, vec![("active", json!(false))])
                .await?;
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "fleet.vehicle",
                &ctx.ids,
                vec![("active", json!(false))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `open_assignation_logs` — the drivers this vehicle has had.
fn open_assignation_logs<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let vehicle_id = only_one(&ctx.ids, "open the history of one vehicle at a time")?;
        let vehicle = one_row(&ctx, "fleet.vehicle", vehicle_id, &["driver_id"]).await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Assignment Logs",
            "res_model": "fleet.vehicle.assignation.log",
            "view_mode": "list,form",
            "domain": [["vehicle_id", "=", vehicle_id]],
            "context": {
                "default_vehicle_id": vehicle_id,
                "default_driver_id": vehicle.get("driver_id").and_then(first_id),
            },
            "target": "current",
        }))
    })
}

// ---------------------------------------------------------------------
// fleet.vehicle.log.contract
// ---------------------------------------------------------------------

/// The four state buttons, which differ only in what they write.
macro_rules! contract_state_button {
    ($func:ident, $state:expr, $complaint:literal) => {
        fn $func<'a>(
            ctx: MethodCtx<'a>,
            _args: &'a [Value],
            _kwargs: &'a Map<String, Value>,
        ) -> MethodFuture<'a> {
            Box::pin(async move {
                no_selection(&ctx.ids, $complaint)?;
                set_contract_state(&ctx, &ctx.ids, $state).await?;
                Ok(json!(true))
            })
        }
    };
}

contract_state_button!(action_open, OPEN, "choose at least one contract to start");
contract_state_button!(action_close, CLOSED, "choose at least one contract to cancel");
contract_state_button!(action_draft, FUTURE, "choose at least one contract to queue");
contract_state_button!(
    action_expire,
    EXPIRED,
    "choose at least one contract to expire"
);

async fn set_contract_state(
    ctx: &MethodCtx<'_>,
    ids: &[i64],
    state: &str,
) -> Result<(), RusdooError> {
    if ids.is_empty() {
        return Ok(());
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "fleet.vehicle.log.contract",
            ids,
            vec![("state", json!(state))],
        )
        .await
}

/// `scheduler_manage_contract_expiration` — the nightly pass over every
/// contract.
///
/// Odoo's cron, in its order: what ran out is expired, what has not
/// started yet is queued, and what starts today is started. The order
/// matters — a contract moved to "queued" in the second step is what the
/// third one picks up the day it begins.
///
/// The activity Odoo schedules for the responsible user is not here:
/// there is no `mail.activity` in this port yet. The state changes, which
/// are what the renewal list reads, all happen.
fn scheduler_manage_contract_expiration<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let today = today();
        let contract = "fleet.vehicle.log.contract";

        let expired = search(
            &ctx,
            contract,
            json!([
                ["state", "not in", [EXPIRED, CLOSED]],
                ["expiration_date", "<", today]
            ]),
        )
        .await?;
        set_contract_state(&ctx, &expired, EXPIRED).await?;

        let queued = search(
            &ctx,
            contract,
            json!([
                ["state", "not in", [FUTURE, CLOSED]],
                ["start_date", ">", today]
            ]),
        )
        .await?;
        set_contract_state(&ctx, &queued, FUTURE).await?;

        let started = search(
            &ctx,
            contract,
            json!([["state", "=", FUTURE], ["start_date", "<=", today]]),
        )
        .await?;
        set_contract_state(&ctx, &started, OPEN).await?;

        Ok(json!({
            "expired": expired.len(),
            "queued": queued.len(),
            "started": started.len(),
        }))
    })
}

// ---------------------------------------------------------------------
// The catalogue's two smart buttons
// ---------------------------------------------------------------------

/// `action_model_vehicle` — the vehicles built from this model.
fn action_model_vehicle<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let model_id = only_one(&ctx.ids, "open the vehicles of one model at a time")?;
        let model = one_row(&ctx, "fleet.vehicle.model", model_id, &["vehicle_count"]).await?;
        let built = model
            .get("vehicle_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        // a model nobody has bought yet opens the form to buy one, which
        // is the only useful thing an empty list could offer
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": if built > 0 { "Vehicles" } else { "Vehicle" },
            "res_model": "fleet.vehicle",
            "view_mode": if built > 0 { "kanban,list,form" } else { "form" },
            "domain": [["model_id", "=", model_id]],
            "context": {"default_model_id": model_id},
            "target": "current",
        }))
    })
}

/// `action_brand_model` — the models this manufacturer makes.
fn action_brand_model<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let brand_id = only_one(&ctx.ids, "open the models of one brand at a time")?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Models",
            "res_model": "fleet.vehicle.model",
            "view_mode": "list,form",
            "domain": [["brand_id", "=", brand_id]],
            "context": {"default_brand_id": brand_id},
            "target": "current",
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_value_the_model_actually_holds_is_copied() {
        // Odoo copies "only when the model has a truthy value": zero doors
        // means the model never said, not that the car has no doors
        assert!(!is_meaningful(&json!(0)));
        assert!(!is_meaningful(&json!("")));
        assert!(!is_meaningful(&json!(false)));
        assert!(!is_meaningful(&Value::Null));
        assert!(is_meaningful(&json!(5)));
        assert!(is_meaningful(&json!("diesel")));
        assert!(is_meaningful(&json!(true)));
    }

    #[test]
    fn the_two_names_that_differ_are_the_ones_odoo_renames() {
        let renamed: Vec<(&str, &str)> = MODEL_FIELDS_TO_VEHICLE
            .into_iter()
            .filter(|(from, to)| from != to)
            .collect();
        assert_eq!(
            renamed,
            vec![("default_co2", "co2"), ("default_fuel_type", "fuel_type")]
        );
    }

    #[test]
    fn a_bike_and_a_car_are_queued_on_different_fields() {
        assert_eq!(plan_to_change_field("bike"), "plan_to_change_bike");
        assert_eq!(plan_to_change_field("car"), "plan_to_change_car");
    }

    #[test]
    fn a_many2one_arrives_as_a_pair_and_leaves_as_an_id() {
        assert_eq!(first_id(&json!([7, "Audi"])), Some(7));
        assert_eq!(first_id(&json!(7)), Some(7));
        assert_eq!(first_id(&Value::Null), None);
    }

    #[test]
    fn the_contract_states_are_the_ones_the_model_stores() {
        assert_eq!(
            [FUTURE, OPEN, EXPIRED, CLOSED],
            ["futur", "open", "expired", "closed"]
        );
        // and the compute module agrees with the methods about them
        assert_eq!(crate::compute::CLOSED, CLOSED);
    }
}
