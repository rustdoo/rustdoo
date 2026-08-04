//! rusdoo-hr-attendance — port of `odoo/addons/hr_attendance/`: who is
//! at work right now, and for how long they were.
//!
//! One model and two moments. A **check-in** opens a record with no
//! check-out; the person is at work. The check-out closes it, and the
//! hours between the two are what payroll, project costing and every
//! "where is everyone" screen read.
//!
//! The two rules the model exists to keep are the ones a hurried screen
//! breaks: **a check-out is never before its check-in**, and **nobody is
//! checked in twice**. The second is the one that quietly doubles
//! somebody's month if it is left to the interface.
//!
//! ## What is deliberately not here
//!
//! * **Overtime.** Odoo compares the hours worked against the employee's
//!   working schedule, per day, and keeps the difference in
//!   `hr.attendance.overtime` with an approval state. It needs the
//!   schedule arithmetic to run per employee per day and a rule about
//!   which days count, which is a module's worth of decisions; what is
//!   ported is the raw hours, which is what overtime is computed *from*.
//! * **The kiosk**, which is a screen and a barcode reader, and
//!   `hr.employee.attendance_state`, which is that screen's cache of the
//!   question this model already answers.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(attendance())?;
    Ok(())
}

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "hr.employee",
        "attendance_manual",
        Operation::Write,
        attendance_manual,
    )?;
    Ok(())
}

/// `%Y-%m-%d %H:%M:%S`, the format the ORM stores a datetime in.
fn moment(value: Option<&Value>) -> Option<chrono::NaiveDateTime> {
    let text = value?.as_str()?;
    chrono::NaiveDateTime::parse_from_str(text, defaults::DATETIME_FORMAT).ok()
}

/// `worked_hours` — the hours between the two moments, and nothing while
/// the person is still at work.
fn worked_hours(record: &Map<String, Value>) -> Value {
    let (Some(check_in), Some(check_out)) = (
        moment(record.get("check_in")),
        moment(record.get("check_out")),
    ) else {
        return json!(0.0);
    };
    let seconds = (check_out - check_in).num_seconds() as f64;
    if seconds <= 0.0 {
        return json!(0.0);
    }
    json!((seconds / 36.0).round() / 100.0)
}

/// The day the attendance belongs to, which is the day it started —
/// a night shift is one day's work, not two halves.
fn date(record: &Map<String, Value>) -> Value {
    match moment(record.get("check_in")) {
        Some(check_in) => json!(check_in.date().format(defaults::DATE_FORMAT).to_string()),
        None => Value::Null,
    }
}

/// Odoo's `_check_validity`, the half of it a single record can answer:
/// leaving before arriving is a typo somebody will otherwise be paid
/// for.
fn the_day_runs_forwards(record: &Map<String, Value>) -> Result<(), String> {
    let (Some(check_in), Some(check_out)) = (
        moment(record.get("check_in")),
        moment(record.get("check_out")),
    ) else {
        return Ok(());
    };
    if check_out < check_in {
        return Err("a check-out cannot be before its check-in".into());
    }
    Ok(())
}

/// `hr.attendance` — one stretch of somebody being at work.
fn attendance() -> Model {
    Model::new(
        ModelMeta {
            name: "hr.attendance".into(),
            table: "hr_attendance".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            // the attendance of somebody who is no longer an employee is
            // still what the company paid for: `restrict`, not cascade
            Field::new(
                "employee_id",
                FieldType::Many2one {
                    comodel: "hr.employee".into(),
                },
            )
            .required()
            .ondelete(OnDelete::Restrict),
            Field::new("check_in", FieldType::Datetime)
                .required()
                .default_from(defaults::NOW),
            Field::new("check_out", FieldType::Datetime),
            // stored: every report groups by it, and it never changes
            // once the record has a check-in
            Field::new("date", FieldType::Date)
                .computed(&["check_in"], date)
                .store(),
            Field::new("worked_hours", FieldType::Float { digits: Some((16, 2)) })
                .computed(&["check_in", "check_out"], worked_hours)
                .store(),
        ],
    )
    .constrained(
        "the day runs forwards",
        &["check_in", "check_out"],
        the_day_runs_forwards,
    )
    // newest first: the screen that opens is "who is here now"
    .ordered("check_in desc, id desc")
}

/// `attendance_manual` — the button: check in if out, check out if in.
///
/// Odoo's takes a next-action argument for the kiosk; here it is the
/// toggle, which is the whole of what the button does.
fn attendance_manual<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        use rusdoo_orm::crud::SearchOptions;
        let [employee] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "check one employee in or out at a time".into(),
            ));
        };
        // the one that is still open, if there is one
        let open = ctx
            .registry
            .search(
                ctx.pool,
                "hr.attendance",
                &rusdoo_orm::domain::parse_domain(&json!([
                    ["employee_id", "=", employee],
                    ["check_out", "=", false]
                ]))?,
                &SearchOptions {
                    limit: Some(1),
                    ..SearchOptions::default()
                },
            )
            .await?;
        let now = chrono::Utc::now()
            .format(defaults::DATETIME_FORMAT)
            .to_string();
        match open.first() {
            Some(attendance) => {
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "hr.attendance",
                        &[*attendance],
                        vec![("check_out", json!(now))],
                    )
                    .await?;
                Ok(json!({"action": "checked_out", "attendance_id": attendance}))
            }
            None => {
                let attendance = ctx
                    .registry
                    .create_as(
                        ctx.pool,
                        ctx.uid,
                        "hr.attendance",
                        vec![("employee_id", json!(employee)), ("check_in", json!(now))],
                    )
                    .await?;
                Ok(json!({"action": "checked_in", "attendance_id": attendance}))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Value) -> Map<String, Value> {
        pairs.as_object().expect("an object").clone()
    }

    #[test]
    fn the_hours_are_what_lies_between_the_two_moments() {
        let row = record(json!({
            "check_in": "2026-08-04 08:00:00",
            "check_out": "2026-08-04 12:30:00",
        }));
        assert_eq!(worked_hours(&row), json!(4.5));
        // somebody still at work has worked no *closed* hours yet
        let open = record(json!({"check_in": "2026-08-04 08:00:00"}));
        assert_eq!(worked_hours(&open), json!(0.0));
    }

    #[test]
    fn a_night_shift_belongs_to_the_day_it_started() {
        let row = record(json!({
            "check_in": "2026-08-04 22:00:00",
            "check_out": "2026-08-05 06:00:00",
        }));
        assert_eq!(date(&row), json!("2026-08-04"));
        assert_eq!(worked_hours(&row), json!(8.0));
    }

    #[test]
    fn leaving_before_arriving_is_a_typo_somebody_would_be_paid_for() {
        let backwards = record(json!({
            "check_in": "2026-08-04 18:00:00",
            "check_out": "2026-08-04 09:00:00",
        }));
        assert!(the_day_runs_forwards(&backwards).is_err());
        // and an open attendance is not yet wrong
        let open = record(json!({"check_in": "2026-08-04 09:00:00"}));
        assert!(the_day_runs_forwards(&open).is_ok());
    }
}
