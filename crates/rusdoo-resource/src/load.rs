//! Reading a working schedule out of the database, and the wire formats
//! it arrives and leaves in.
//!
//! Kept apart from [`crate::calendar`] on purpose: the arithmetic there
//! takes plain structs and can therefore be tested without a database,
//! which is the only reason its edge cases *are* tested.

use crate::calendar::{Attendance, Leave, Schedule};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::MethodCtx;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use sqlx::PgPool;

/// Odoo's wire format for a datetime, and the date-only shape a client
/// sends when it means the start of a day.
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const DATE_FORMAT: &str = "%Y-%m-%d";

/// The id inside a many2one, which reads back as `[id, name]`.
pub fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// A datetime as it travels: `"2025-06-04 08:00:00"`, or a bare date
/// meaning that day's first instant.
///
/// A client that sends `"2025-06-04"` for the start of a window means
/// midnight, and refusing it would be pedantry — but a client that sends
/// nonsense is told so rather than being given today.
pub fn parse_datetime(value: &Value, what: &str) -> Result<NaiveDateTime, RusdooError> {
    let text = value.as_str().ok_or_else(|| {
        RusdooError::Validation(format!("{what} must be a datetime like \"2025-06-04 08:00:00\""))
    })?;
    NaiveDateTime::parse_from_str(text, DATETIME_FORMAT)
        .or_else(|_| {
            NaiveDate::parse_from_str(text, DATE_FORMAT).map(|date| date.and_time(NaiveTime::MIN))
        })
        .map_err(|_| {
            RusdooError::Validation(format!(
                "{what}: {text:?} is not a datetime like \"2025-06-04 08:00:00\""
            ))
        })
}

/// A datetime on its way back out.
pub fn format_datetime(moment: NaiveDateTime) -> String {
    moment.format(DATETIME_FORMAT).to_string()
}

pub fn format_date(date: NaiveDate) -> String {
    date.format(DATE_FORMAT).to_string()
}

/// The fields a schedule is built from.
const CALENDAR_FIELDS: [&str; 6] = [
    "two_weeks_calendar",
    "flexible_hours",
    "duration_based",
    "hours_per_day",
    "hours_per_week",
    "attendance_ids",
];

const ATTENDANCE_FIELDS: [&str; 7] = [
    "dayofweek",
    "hour_from",
    "hour_to",
    "day_period",
    "week_type",
    "display_type",
    "sequence",
];

/// Load one calendar's schedule.
pub async fn load_schedule(
    registry: &Registry,
    pool: &PgPool,
    calendar_id: i64,
) -> Result<Schedule, RusdooError> {
    let rows = registry
        .read(pool, "resource.calendar", &[calendar_id], &CALENDAR_FIELDS)
        .await?;
    let calendar = rows.first().ok_or_else(|| {
        RusdooError::Validation(format!("working schedule {calendar_id} does not exist"))
    })?;
    let attendance_ids: Vec<i64> = calendar
        .get("attendance_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let attendances = load_attendances(registry, pool, &attendance_ids).await?;

    Ok(Schedule {
        two_weeks_calendar: flag(calendar.get("two_weeks_calendar")),
        flexible_hours: flag(calendar.get("flexible_hours")),
        duration_based: flag(calendar.get("duration_based")),
        hours_per_day: number(calendar.get("hours_per_day")),
        hours_per_week: number(calendar.get("hours_per_week")),
        attendances,
    })
}

/// The attendance rows of a calendar, in the order the model declares
/// (`sequence, week_type, dayofweek, hour_from`) — which is the order a
/// two-week calendar's sections depend on.
pub async fn load_attendances(
    registry: &Registry,
    pool: &PgPool,
    ids: &[i64],
) -> Result<Vec<Attendance>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = registry
        .read(pool, "resource.calendar.attendance", ids, &ATTENDANCE_FIELDS)
        .await?;
    let mut attendances: Vec<Attendance> = rows
        .iter()
        .filter_map(|row| {
            Some(Attendance {
                id: row.get("id").and_then(Value::as_i64)?,
                // the selection holds "0".."6"; a row whose day cannot be
                // read is a row nothing can be scheduled on
                dayofweek: text(row.get("dayofweek")).parse().ok()?,
                hour_from: number(row.get("hour_from")),
                hour_to: number(row.get("hour_to")),
                day_period: {
                    let period = text(row.get("day_period"));
                    if period.is_empty() {
                        "morning".to_string()
                    } else {
                        period
                    }
                },
                week_type: text(row.get("week_type")).parse().ok(),
                display_type: Some(text(row.get("display_type"))).filter(|it| !it.is_empty()),
                sequence: row.get("sequence").and_then(Value::as_i64).unwrap_or(10) as i32,
            })
        })
        .collect();
    attendances.sort_by(|a, b| {
        (a.sequence, a.week_type, a.dayofweek)
            .cmp(&(b.sequence, b.week_type, b.dayofweek))
            .then(a.hour_from.total_cmp(&b.hour_from))
    });
    Ok(attendances)
}

/// The time off that bears on a calendar in a window.
///
/// Odoo's domain, made explicit: a leave counts when it overlaps the
/// window, when it is of the type asked about, when it belongs to this
/// calendar or to no calendar at all (a public holiday), and when it is
/// this resource's or nobody's. The calendar and resource halves are
/// filtered here rather than in the domain because "or no calendar" is a
/// null test, and a null test written as a domain reads worse than the
/// two lines it saves.
pub async fn load_leaves(
    registry: &Registry,
    pool: &PgPool,
    calendar_id: Option<i64>,
    resource_id: Option<i64>,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Result<Vec<Leave>, RusdooError> {
    let domain = parse_domain(&json!([
        ["time_type", "=", "leave"],
        ["date_from", "<=", format_datetime(end)],
        ["date_to", ">=", format_datetime(start)]
    ]))?;
    let ids = registry
        .search(
            pool,
            "resource.calendar.leaves",
            &domain,
            &SearchOptions::default(),
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = registry
        .read(
            pool,
            "resource.calendar.leaves",
            &ids,
            &["date_from", "date_to", "calendar_id", "resource_id"],
        )
        .await?;
    let mut leaves = Vec::new();
    for row in &rows {
        let on_calendar = row.get("calendar_id").and_then(first_id);
        if on_calendar.is_some() && on_calendar != calendar_id {
            continue;
        }
        let for_resource = row.get("resource_id").and_then(first_id);
        if for_resource.is_some() && for_resource != resource_id {
            continue;
        }
        let (Some(id), Some(from), Some(to)) = (
            row.get("id").and_then(Value::as_i64),
            row.get("date_from").and_then(|v| parse_datetime(v, "date_from").ok()),
            row.get("date_to").and_then(|v| parse_datetime(v, "date_to").ok()),
        ) else {
            continue;
        };
        leaves.push(Leave {
            id,
            date_from: from,
            date_to: to,
        });
    }
    Ok(leaves)
}

/// The one calendar a method was called on.
pub fn only_one(ctx: &MethodCtx<'_>, complaint: &str) -> Result<i64, RusdooError> {
    match ctx.ids[..] {
        [id] => Ok(id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

/// An argument by name or by position — the two shapes a client sends.
pub fn argument<'a>(
    ctx: &'a MethodCtx<'a>,
    kwargs: &'a serde_json::Map<String, Value>,
    name: &str,
    position: usize,
) -> Option<&'a Value> {
    kwargs
        .get(name)
        .or_else(|| ctx.rest.get(position))
        .filter(|value| !value.is_null())
}

fn flag(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_datetime_arrives_in_odoos_own_format() {
        let parsed = parse_datetime(&json!("2025-06-04 08:30:00"), "start").unwrap();
        assert_eq!(format_datetime(parsed), "2025-06-04 08:30:00");
    }

    #[test]
    fn a_bare_date_means_that_days_first_instant() {
        let parsed = parse_datetime(&json!("2025-06-04"), "start").unwrap();
        assert_eq!(format_datetime(parsed), "2025-06-04 00:00:00");
    }

    #[test]
    fn something_that_is_not_a_datetime_says_so_and_names_the_argument() {
        let error = parse_datetime(&json!("tomorrow"), "start_dt").expect_err("not a datetime");
        assert!(error.to_string().contains("start_dt"), "{error}");
        let error = parse_datetime(&json!(17), "start_dt").expect_err("not even a string");
        assert!(error.to_string().contains("start_dt"), "{error}");
    }

    #[test]
    fn a_many2one_gives_up_its_id_whichever_shape_it_arrives_in() {
        assert_eq!(first_id(&json!([7, "40 hours/week"])), Some(7));
        assert_eq!(first_id(&json!(7)), Some(7));
        assert_eq!(first_id(&Value::Null), None);
    }
}
