//! rusdoo-maintenance — port of `odoo/addons/maintenance/`: the
//! machines, and what breaks on them.
//!
//! An **equipment** is a thing the company keeps working: a press, a
//! van, a laptop. A **request** is somebody saying it needs attention,
//! and it moves through **stages** like any other pipeline, ending at one
//! marked `done`. Requests come in two kinds, and the difference is the
//! whole reason the module exists: **corrective** is something that
//! broke, **preventive** is something being seen to before it does.
//!
//! ## What is deliberately not here
//!
//! * **MTBF and MTTR.** Odoo computes mean time between failures and
//!   mean time to repair from the closed corrective requests of each
//!   piece of equipment, and schedules the next preventive round from
//!   them. The arithmetic is easy; what it needs and this port does not
//!   have is a cron that writes `next_action_date` on every equipment
//!   every night. The dates are here as columns anybody can fill; the
//!   prediction is not.
//! * **The maintenance team's dashboard**, which is a screen over
//!   `read_group`, and the equipment `.category`'s properties
//!   definition, which needs the `Properties` field type.

use rusdoo_core::RusdooError;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(stage())?;
    reg.register(team())?;
    reg.register(category())?;
    reg.register(equipment())?;
    reg.register(request())?;
    Ok(())
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
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

fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    Field::new(
        name,
        FieldType::One2many {
            comodel: comodel.to_string(),
            inverse: inverse.to_string(),
        },
    )
}

/// The ids a one2many dependency arrives as.
fn gathered(record: &Map<String, Value>, key: &str) -> Vec<Value> {
    record
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn equipment_count(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "equipment_ids").len() as i64)
}

fn maintenance_count(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "maintenance_ids").len() as i64)
}

/// How many requests on this equipment are still open — the number that
/// decides whether somebody has to do something today.
fn maintenance_open_count(record: &Map<String, Value>) -> Value {
    let done = gathered(record, "maintenance_ids.stage_done");
    json!(done
        .iter()
        .filter(|value| value.as_bool() != Some(true))
        .count() as i64)
}

/// A request that was closed before it was asked for is a date somebody
/// mistyped, and it would poison every repair-time average built on it.
fn the_repair_ends_after_it_starts(record: &Map<String, Value>) -> Result<(), String> {
    let parse = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| chrono::NaiveDate::parse_from_str(text, defaults::DATE_FORMAT).ok())
    };
    let (Some(requested), Some(closed)) = (parse("request_date"), parse("close_date")) else {
        return Ok(());
    };
    if closed < requested {
        return Err("a request cannot be closed before it was made".into());
    }
    Ok(())
}

/// `maintenance.stage` — a column of the maintenance pipeline.
fn stage() -> Model {
    Model::new(
        meta("maintenance.stage", "maintenance_stage"),
        vec![
            char("name").required().translatable(),
            Field::new("sequence", FieldType::Integer).default_value(json!(20)),
            Field::new("fold", FieldType::Boolean).default_value(json!(false)),
            // the one thing a stage says about the request in it
            Field::new("done", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .ordered("sequence, id")
}

/// `maintenance.team` — who gets the requests.
fn team() -> Model {
    Model::new(
        meta("maintenance.team", "maintenance_team"),
        vec![
            char("name").required().translatable(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new(
                "member_ids",
                FieldType::Many2many {
                    comodel: "res.users".into(),
                    relation: "maintenance_team_users_rel".into(),
                    column1: "team_id".into(),
                    column2: "user_id".into(),
                },
            ),
            Field::new("color", FieldType::Integer),
        ],
    )
    .ordered("name, id")
}

/// `maintenance.equipment.category` — presses, vans, laptops.
fn category() -> Model {
    Model::new(
        meta("maintenance.equipment.category", "maintenance_equipment_category"),
        vec![
            char("name").required().translatable(),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("technician_user_id", "res.users").default_from(defaults::CURRENT_USER),
            Field::new("note", FieldType::Html),
            Field::new("color", FieldType::Integer),
            o2m("equipment_ids", "maintenance.equipment", "category_id"),
            Field::new("equipment_count", FieldType::Integer)
                .computed(&["equipment_ids"], equipment_count),
        ],
    )
    .ordered("name, id")
}

/// `maintenance.equipment` — the thing itself.
fn equipment() -> Model {
    Model::new(
        meta("maintenance.equipment", "maintenance_equipment"),
        vec![
            char("name").required().translatable(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            // a category whose equipment is still there is not a
            // category anybody may delete out from under it
            m2o("category_id", "maintenance.equipment.category").ondelete(OnDelete::Restrict),
            m2o("technician_user_id", "res.users"),
            m2o("maintenance_team_id", "maintenance.team"),
            // who has it, in the two shapes it comes in: an employee, or
            // a department that shares it
            m2o("employee_id", "hr.employee"),
            m2o("department_id", "hr.department"),
            m2o("partner_id", "res.partner"),
            char("serial_no"),
            char("model"),
            char("location"),
            Field::new("assign_date", FieldType::Date),
            Field::new("effective_date", FieldType::Date)
                .required()
                .default_from(defaults::TODAY),
            Field::new("warranty_date", FieldType::Date),
            Field::new("cost", FieldType::Float { digits: Some((16, 2)) })
                .default_value(json!(0.0)),
            Field::new("note", FieldType::Html),
            Field::new("expected_mtbf", FieldType::Integer).default_value(json!(0)),
            Field::new("period", FieldType::Integer).default_value(json!(0)),
            Field::new("next_action_date", FieldType::Date),
            o2m("maintenance_ids", "maintenance.request", "equipment_id"),
            // not materialised: both move when a *request* is written
            Field::new("maintenance_count", FieldType::Integer)
                .computed(&["maintenance_ids"], maintenance_count),
            Field::new("maintenance_open_count", FieldType::Integer)
                .computed(&["maintenance_ids.stage_done"], maintenance_open_count),
        ],
    )
    .ordered("name, id")
}

/// `maintenance.request` — somebody saying a thing needs attention.
fn request() -> Model {
    Model::new(
        meta("maintenance.request", "maintenance_request"),
        vec![
            char("name").required().translatable(),
            Field::new("description", FieldType::Html),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            m2o("owner_user_id", "res.users").default_from(defaults::CURRENT_USER),
            // the request outlives the equipment being archived, but not
            // its deletion: a repair on nothing is a row nobody can read
            m2o("equipment_id", "maintenance.equipment").ondelete(OnDelete::Cascade),
            m2o("category_id", "maintenance.equipment.category"),
            m2o("maintenance_team_id", "maintenance.team"),
            m2o("stage_id", "maintenance.stage"),
            // corrective is something that broke; preventive is
            // something being seen to before it does. Odoo's whole
            // scheduling story hangs off this one word.
            Field::new(
                "maintenance_type",
                FieldType::Selection(vec![
                    ("corrective".into(), "Corrective".into()),
                    ("preventive".into(), "Preventive".into()),
                ]),
            )
            .required()
            .default_value(json!("corrective")),
            Field::new(
                "priority",
                FieldType::Selection(vec![
                    ("0".into(), "Very Low".into()),
                    ("1".into(), "Low".into()),
                    ("2".into(), "Normal".into()),
                    ("3".into(), "High".into()),
                ]),
            )
            .default_value(json!("1")),
            Field::new("request_date", FieldType::Date)
                .required()
                .default_from(defaults::TODAY),
            Field::new("schedule_date", FieldType::Datetime),
            Field::new("close_date", FieldType::Date),
            Field::new("duration", FieldType::Float { digits: Some((16, 2)) })
                .default_value(json!(0.0)),
            // related to the stage, and what every other model here asks
            // about a request: is it still open?
            Field::new("stage_done", FieldType::Boolean).related("stage_id.done"),
            Field::new("color", FieldType::Integer),
        ],
    )
    .constrained(
        "a repair ends after it starts",
        &["request_date", "close_date"],
        the_repair_ends_after_it_starts,
    )
    .ordered("id desc")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Value) -> Map<String, Value> {
        pairs.as_object().expect("an object").clone()
    }

    #[test]
    fn what_is_open_is_what_is_not_done() {
        let row = record(json!({"maintenance_ids.stage_done": [true, false, false, null]}));
        assert_eq!(maintenance_count(&record(json!({}))), json!(0));
        // three of the four are not in a stage marked done — including
        // the one with no stage at all
        assert_eq!(maintenance_open_count(&row), json!(3));
    }

    #[test]
    fn a_repair_closed_before_it_was_asked_for_is_refused() {
        let backwards = record(json!({
            "request_date": "2026-08-10",
            "close_date": "2026-08-04",
        }));
        assert!(the_repair_ends_after_it_starts(&backwards).is_err());
        let open = record(json!({"request_date": "2026-08-10"}));
        assert!(the_repair_ends_after_it_starts(&open).is_ok());
        let same_day = record(json!({
            "request_date": "2026-08-10",
            "close_date": "2026-08-10",
        }));
        assert!(the_repair_ends_after_it_starts(&same_day).is_ok());
    }

    #[test]
    fn the_two_kinds_of_request_are_the_point_of_the_module() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_resource::extend(&mut reg).unwrap();
        rusdoo_hr::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        let request = reg.get("maintenance.request").expect("registered");
        let kind = request.field("maintenance_type").expect("declared");
        assert!(kind.required);
        match &kind.ty {
            FieldType::Selection(options) => {
                let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
                assert_eq!(values, ["corrective", "preventive"]);
            }
            other => panic!("maintenance_type is a {other:?}"),
        }
    }
}
