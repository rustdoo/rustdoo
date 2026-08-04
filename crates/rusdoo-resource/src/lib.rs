//! rusdoo-resource — port of `odoo/addons/resource/`: when the people
//! and the machines are available.
//!
//! Four models and one question. A **resource** is anything that can be
//! scheduled — a developer, a work centre, a meeting room. A **working
//! schedule** (`resource.calendar`) says which hours of which weekdays it
//! is available, a **working period** (`resource.calendar.attendance`) is
//! one such stretch, and **time off** (`resource.calendar.leaves`) takes
//! hours back out. Every planning module in Odoo — manufacturing,
//! field service, payroll, the gantt views — asks this module the same
//! four things: how many hours is that, how many days is that, when will
//! N hours be done, and when will N days be.
//!
//! The arithmetic lives in [`intervals`] and [`calendar`], as plain
//! functions over plain structs, and it is where the tests are. The
//! models here are the part that stores what those functions are given.
//!
//! ## What is deliberately not here
//!
//! * **Time zones.** Odoo does every step in the calendar's `tz` and
//!   converts at the edges (`pytz`). The framework has no timezone-aware
//!   datetime type and the workspace has no `chrono-tz`, so `tz` is
//!   stored and shown but nothing converts by it: every datetime in and
//!   out of this module is the calendar's own wall clock. For the
//!   default UTC calendar that is exactly right; for any other, the
//!   caller carries the shift. This is the port's largest known
//!   deviation and the one worth closing first.
//! * **Resource-specific calendars inside a batch.** Odoo's
//!   `_..._batch` methods answer for many resources at once, each with
//!   its own calendar and its own timezone. Here a calendar answers for
//!   itself, and a resource answers through its own calendar — the shape
//!   a single screen needs, without the machinery a payroll run does.
//! * The two-week form's split `attendance_ids_1st_week` /
//!   `attendance_ids_2nd_week` pair, which exists only to draw two lists
//!   from one relation, and `two_weeks_explanation`, which is a sentence.
//! * `_get_flexible_resource_valid_work_intervals` and its weekly caps,
//!   which need locale-aware week numbering (`babel`) that the port has
//!   no equivalent for.

pub mod calendar;
pub mod intervals;
pub mod load;

use calendar::{Attendance, Schedule};
use load::{argument, first_id, format_date, format_datetime, only_one, parse_datetime};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{DefaultCtx, Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The timezone a calendar has until somebody picks one. Odoo takes the
/// creating user's; with no timezone arithmetic in the framework, saying
/// UTC out loud beats inheriting a value nothing honours.
const DEFAULT_TZ: &str = "UTC";

/// A full-time week, and Odoo's own default for `full_time_required_hours`
/// — the number every part-time percentage is measured against.
const FULL_TIME_HOURS: f64 = 40.0;

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn float(name: &str) -> Field {
    Field::new(name, FieldType::Float { digits: None })
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

fn selection(name: &str, options: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            options
                .iter()
                .map(|(key, label)| ((*key).to_string(), (*label).to_string()))
                .collect(),
        ),
    )
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn extends(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(working_schedule())?;
    reg.register(working_period())?;
    reg.register(resource())?;
    reg.register(time_off())?;
    reg.register(company())?;
    reg.register(users())?;
    Ok(())
}

/// What a schedule answers when somebody asks it a question.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    for (name, func) in [
        (
            "get_work_hours_count",
            get_work_hours_count as rusdoo_orm::methods::MethodFn,
        ),
        ("get_work_duration_data", get_work_duration_data),
        ("plan_hours", plan_hours),
        ("plan_days", plan_days),
        ("get_unusual_days", get_unusual_days),
        // asking a calendar whether its periods are sound reads it; it
        // is the *saving* of a clashing period that has to be refused,
        // and that is a write on the periods themselves
        ("validate_attendances", validate_attendances),
    ] {
        methods.register("resource.calendar", name, Operation::Read, func)?;
    }
    methods.register(
        "resource.calendar",
        "switch_calendar_type",
        Operation::Write,
        switch_calendar_type,
    )?;
    methods.register(
        "resource.resource",
        "adjust_to_calendar",
        Operation::Read,
        adjust_to_calendar,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Computes
// ---------------------------------------------------------------------

/// The parallel arrays a dotted dependency over a one2many arrives as,
/// zipped back into the rows they came from.
///
/// `@api.depends('attendance_ids.hour_from')` gives one list per field,
/// each in the same order; the schedule needs them as records again.
fn schedule_from_depends(record: &Map<String, Value>) -> Schedule {
    let column = |name: &str| -> Vec<Value> {
        record
            .get(name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let hour_from = column("attendance_ids.hour_from");
    let hour_to = column("attendance_ids.hour_to");
    let day_period = column("attendance_ids.day_period");
    let dayofweek = column("attendance_ids.dayofweek");
    let week_type = column("attendance_ids.week_type");
    let display_type = column("attendance_ids.display_type");

    let attendances = (0..hour_from.len())
        .map(|index| {
            let at = |column: &[Value]| column.get(index).cloned().unwrap_or(Value::Null);
            Attendance {
                // the compute never looks at ids; what it needs is the
                // shape of the week
                id: index as i64,
                dayofweek: at(&dayofweek)
                    .as_str()
                    .and_then(|day| day.parse().ok())
                    .unwrap_or(0),
                hour_from: at(&hour_from).as_f64().unwrap_or(0.0),
                hour_to: at(&hour_to).as_f64().unwrap_or(0.0),
                day_period: at(&day_period)
                    .as_str()
                    .unwrap_or("morning")
                    .to_string(),
                week_type: at(&week_type).as_str().and_then(|week| week.parse().ok()),
                display_type: at(&display_type)
                    .as_str()
                    .filter(|it| !it.is_empty())
                    .map(str::to_string),
                sequence: 10,
            }
        })
        .collect();

    Schedule {
        two_weeks_calendar: record
            .get("two_weeks_calendar")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        duration_based: record
            .get("duration_based")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        attendances,
        ..Schedule::default()
    }
}

/// The fields both weekly averages read.
const SCHEDULE_DEPENDS: [&str; 8] = [
    "attendance_ids.hour_from",
    "attendance_ids.hour_to",
    "attendance_ids.day_period",
    "attendance_ids.dayofweek",
    "attendance_ids.week_type",
    "attendance_ids.display_type",
    "two_weeks_calendar",
    "duration_based",
];

/// `_compute_hours_per_week`.
fn hours_per_week(record: &Map<String, Value>) -> Value {
    json!(round2(schedule_from_depends(record).computed_hours_per_week()))
}

/// `_compute_hours_per_day`.
///
/// Odoo's comment explains why it is not derived from `hours_per_week`:
/// both are rounded, and dividing one rounded number by another loses
/// the day length of anything that is not a whole hour.
fn hours_per_day(record: &Map<String, Value>) -> Value {
    json!(round2(schedule_from_depends(record).computed_hours_per_day()))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `_compute_flexible_hours` — the boolean the arithmetic branches on,
/// kept in step with the selection the user sees.
fn flexible_hours(record: &Map<String, Value>) -> Value {
    json!(record.get("schedule_type").and_then(Value::as_str) == Some("flexible"))
}

/// `_compute_work_time_rate` — this schedule as a percentage of a full
/// week. A schedule with no full-time reference is full time.
fn work_time_rate(record: &Map<String, Value>) -> Value {
    let full_time = record
        .get("full_time_required_hours")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let worked = record
        .get("hours_per_week")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if full_time == 0.0 {
        return json!(100.0);
    }
    json!(round2(worked / full_time * 100.0))
}

/// `is_fulltime` — compared at three decimals, like Odoo's
/// `float_compare(..., 3)`: 39.999999 hours is a full week.
fn is_fulltime(record: &Map<String, Value>) -> Value {
    let full_time = record
        .get("full_time_required_hours")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let worked = record
        .get("hours_per_week")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    json!((full_time - worked).abs() < 0.0005)
}

/// `work_resources_count` — how many resources this schedule governs.
fn work_resources_count(record: &Map<String, Value>) -> Value {
    json!(record
        .get("resource_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len))
}

/// `_compute_duration_hours` — a break lasts no working time.
fn duration_hours(record: &Map<String, Value>) -> Value {
    let period = record
        .get("day_period")
        .and_then(Value::as_str)
        .unwrap_or("morning");
    if period == "lunch" {
        return json!(0.0);
    }
    let from = record.get("hour_from").and_then(Value::as_f64).unwrap_or(0.0);
    let to = record.get("hour_to").and_then(Value::as_f64).unwrap_or(0.0);
    json!(to - from)
}

/// `_compute_duration_days`.
fn duration_days(record: &Map<String, Value>) -> Value {
    let period = record
        .get("day_period")
        .and_then(Value::as_str)
        .unwrap_or("morning");
    let hours = record
        .get("duration_hours")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    // a many2one dependency arrives as a one-element list
    let day_length = record
        .get("calendar_id.hours_per_day")
        .and_then(|value| match value {
            Value::Array(items) => items.first().and_then(Value::as_f64),
            other => other.as_f64(),
        })
        .filter(|hours| *hours > 0.0)
        .unwrap_or(calendar::HOURS_PER_DAY);
    let days = match period {
        "lunch" => 0.0,
        "full_day" => 1.0,
        _ if hours <= day_length * 3.0 / 4.0 => 0.5,
        _ => 1.0,
    };
    json!(days)
}

// ---------------------------------------------------------------------
// Constraints
// ---------------------------------------------------------------------

fn is_named(record: &Map<String, Value>) -> Result<(), String> {
    match record.get("name").and_then(Value::as_str).map(str::trim) {
        Some(name) if !name.is_empty() => Ok(()),
        _ => Err("give the working schedule a name: a nameless one cannot be picked".into()),
    }
}

/// The hours of a working period.
///
/// Odoo clamps these in an `@api.onchange`, which only ever runs while a
/// form is open — a create over RPC walks straight past it and stores
/// `hour_from: 30`. A rule only the form applies is not a rule, so here
/// it refuses instead of correcting.
fn hours_make_a_period(record: &Map<String, Value>) -> Result<(), String> {
    let from = record.get("hour_from").and_then(Value::as_f64).unwrap_or(0.0);
    let to = record.get("hour_to").and_then(Value::as_f64).unwrap_or(0.0);
    if !(0.0..=24.0).contains(&from) || !(0.0..=24.0).contains(&to) {
        return Err(format!(
            "a working period runs within the day: {from}–{to} is outside 0–24"
        ));
    }
    if to < from {
        return Err(format!(
            "a working period ends after it starts: {from}–{to} runs backwards"
        ));
    }
    Ok(())
}

/// `_check_day_period` — a calendar written as durations has no breaks
/// to encode: the hours are a length around midday, and a break in the
/// middle of a length means nothing.
fn no_break_on_a_duration_calendar(record: &Map<String, Value>) -> Result<(), String> {
    let is_break = record.get("day_period").and_then(Value::as_str) == Some("lunch");
    let duration_based = record
        .get("duration_based")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_break && duration_based {
        return Err(
            "a duration-based schedule has no breaks: its hours are a length, not clock times"
                .into(),
        );
    }
    Ok(())
}

/// `check_dates` — time off that ends before it starts.
fn time_off_runs_forwards(record: &Map<String, Value>) -> Result<(), String> {
    let from = record.get("date_from").and_then(Value::as_str);
    let to = record.get("date_to").and_then(Value::as_str);
    match (from, to) {
        // the wire format sorts as text exactly as it sorts as time
        (Some(from), Some(to)) if from > to => Err(format!(
            "time off ends before it starts: {from} to {to}"
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// `resource.calendar` — a weekly working schedule.
fn working_schedule() -> Model {
    Model::new(
        meta("resource.calendar", "resource_calendar"),
        vec![
            char("name").required(),
            // a schedule nobody uses any more is archived, never deleted:
            // the periods already worked under it still point at it
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            // stored and shown, but nothing converts by it yet — see the
            // crate docs
            char("tz").required().default_value(json!(DEFAULT_TZ)),
            Field::new("two_weeks_calendar", FieldType::Boolean).default_value(json!(false)),
            selection(
                "schedule_type",
                &[("flexible", "Flexible"), ("fully_fixed", "Fully Fixed")],
            )
            .required()
            .default_value(json!("fully_fixed")),
            // Odoo keeps the boolean and the selection in step with a
            // compute *and* an inverse. The port has no inverse, so the
            // selection is what a client writes and the boolean is what
            // the arithmetic reads — the same pair, one direction.
            Field::new("flexible_hours", FieldType::Boolean)
                .computed(&["schedule_type"], flexible_hours)
                .store(),
            Field::new("duration_based", FieldType::Boolean).default_value(json!(false)),
            o2m(
                "attendance_ids",
                "resource.calendar.attendance",
                "calendar_id",
            ),
            o2m("leave_ids", "resource.calendar.leaves", "calendar_id"),
            // Odoo counts these with a read_group; naming the relation is
            // how the port asks the same question
            o2m("resource_ids", "resource.resource", "calendar_id"),
            // not materialized: the average changes when a *period* is
            // written, and the recompute only follows a write to the
            // schedule itself — a stored column would go stale the first
            // time somebody edits a Monday morning
            float("hours_per_day").computed(&SCHEDULE_DEPENDS, hours_per_day),
            float("hours_per_week").computed(&SCHEDULE_DEPENDS, hours_per_week),
            float("full_time_required_hours").default_value(json!(FULL_TIME_HOURS)),
            float("work_time_rate")
                .computed(&["hours_per_week", "full_time_required_hours"], work_time_rate),
            Field::new("is_fulltime", FieldType::Boolean)
                .computed(&["hours_per_week", "full_time_required_hours"], is_fulltime),
            Field::new("work_resources_count", FieldType::Integer)
                .computed(&["resource_ids"], work_resources_count),
        ],
    )
    .constrained("schedule has a name", &["name"], is_named)
    .ordered("name, id")
}

/// `resource.calendar.attendance` — one stretch of one weekday.
fn working_period() -> Model {
    Model::new(
        meta(
            "resource.calendar.attendance",
            "resource_calendar_attendance",
        ),
        vec![
            char("name").required(),
            selection(
                "dayofweek",
                &[
                    ("0", "Monday"),
                    ("1", "Tuesday"),
                    ("2", "Wednesday"),
                    ("3", "Thursday"),
                    ("4", "Friday"),
                    ("5", "Saturday"),
                    ("6", "Sunday"),
                ],
            )
            .required()
            .default_value(json!("0")),
            float("hour_from").required().default_value(json!(0.0)),
            float("hour_to").required().default_value(json!(0.0)),
            // a period without a schedule is a period nothing reads, and
            // deleting the schedule takes its week with it
            m2o("calendar_id", "resource.calendar")
                .required()
                .ondelete(OnDelete::Cascade),
            selection(
                "day_period",
                &[
                    ("morning", "Morning"),
                    ("lunch", "Break"),
                    ("afternoon", "Afternoon"),
                    ("full_day", "Full Day"),
                ],
            )
            .required()
            .default_value(json!("morning")),
            // only set on a two-week schedule, hence no default
            selection("week_type", &[("0", "First"), ("1", "Second")]),
            selection("display_type", &[("line_section", "Section")]),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            float("duration_hours")
                .computed(&["hour_from", "hour_to", "day_period"], duration_hours),
            float("duration_days").computed(
                &["day_period", "duration_hours", "calendar_id.hours_per_day"],
                duration_days,
            ),
            Field::new("two_weeks_calendar", FieldType::Boolean)
                .related("calendar_id.two_weeks_calendar"),
            Field::new("duration_based", FieldType::Boolean).related("calendar_id.duration_based"),
        ],
    )
    .constrained(
        "period within the day",
        &["hour_from", "hour_to"],
        hours_make_a_period,
    )
    .constrained(
        "no break on a duration schedule",
        &["day_period", "duration_based"],
        no_break_on_a_duration_calendar,
    )
    .ordered("sequence, week_type, dayofweek, hour_from")
}

/// `resource.calendar.leaves` — time off, for everybody or for one.
fn time_off() -> Model {
    Model::new(
        meta("resource.calendar.leaves", "resource_calendar_leaves"),
        vec![
            char("name"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("calendar_id", "resource.calendar"),
            Field::new("date_from", FieldType::Datetime).required(),
            Field::new("date_to", FieldType::Datetime).required(),
            // empty means everybody's: a public holiday belongs to the
            // schedule, not to a person
            m2o("resource_id", "resource.resource"),
            selection("time_type", &[("leave", "Time Off"), ("other", "Other")])
                .default_value(json!("leave")),
        ],
    )
    .constrained(
        "time off runs forwards",
        &["date_from", "date_to"],
        time_off_runs_forwards,
    )
    .ordered("date_from, id")
}

/// `default=lambda self: self.env.company.resource_calendar_id` — a new
/// resource works the company's hours until somebody says otherwise.
///
/// A company with no default schedule of its own gives no default, and
/// the resource is fully flexible: that is what the field means when it
/// is empty, and it is the answer Odoo gives on a fresh database too.
fn company_schedule(
    ctx: DefaultCtx<'_>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RusdooError>> + Send + '_>> {
    Box::pin(async move {
        let found: Option<Option<i32>> = sqlx::query_scalar(
            r#"SELECT c."resource_calendar_id" FROM "res_company" c
               JOIN "res_users" u ON u."company_id" = c."id"
               WHERE u."id" = $1"#,
        )
        .bind(ctx.uid as i32)
        .fetch_optional(&mut *ctx.conn)
        .await
        .map_err(|error| {
            RusdooError::Database(format!("default de agenda em {}: {error}", ctx.model))
        })?;
        Ok(match found.flatten() {
            Some(schedule) => Value::from(i64::from(schedule)),
            None => Value::Null,
        })
    })
}

/// `resource.resource` — anything that can be scheduled.
fn resource() -> Model {
    Model::new(
        meta("resource.resource", "resource_resource"),
        vec![
            char("name").required(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            selection("resource_type", &[("user", "Human"), ("material", "Material")])
                .required()
                .default_value(json!("user")),
            m2o("user_id", "res.users"),
            // empty is not "no schedule" but "any hour": a fully flexible
            // resource, in Odoo's words. What fills it in when nobody
            // says otherwise is the company's own schedule, like the
            // `default=` of the field it is ported from.
            m2o("calendar_id", "resource.calendar").default_from(company_schedule),
            char("tz").required().default_value(json!(DEFAULT_TZ)),
            float("time_efficiency")
                .required()
                .default_value(json!(100.0)),
            char("email").related("user_id.email"),
            char("phone").related("user_id.phone"),
        ],
    )
    // a rule the database keeps: an efficiency of zero would make every
    // expected duration infinite, and a negative one would run work
    // backwards. A Rust check can be raced past by two writers; this
    // cannot.
    .sql_constrained(
        "resource_resource_time_efficiency_positive",
        r#"CHECK ("time_efficiency" > 0)"#,
        "the efficiency factor must be greater than zero",
    )
    .ordered("name, id")
}

/// `res.company` — the default schedule new resources inherit.
fn company() -> Model {
    Model::new(
        extends("res.company", "res_company"),
        vec![
            // refused rather than emptied: a company whose default
            // schedule vanished is a company where every new resource is
            // silently fully flexible
            m2o("resource_calendar_id", "resource.calendar").ondelete(OnDelete::Restrict),
            o2m("resource_calendar_ids", "resource.calendar", "company_id"),
        ],
    )
}

/// `res.users` — the resources a person is.
fn users() -> Model {
    Model::new(
        extends("res.users", "res_users"),
        vec![o2m("resource_ids", "resource.resource", "user_id")],
    )
}

// ---------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------

/// The window a method was asked about, and the schedule to answer with.
struct Question {
    calendar_id: i64,
    schedule: Schedule,
    start: chrono::NaiveDateTime,
    end: chrono::NaiveDateTime,
}

/// Read the two datetimes every computation method takes, and the
/// schedule they are asked of.
async fn question(
    ctx: &MethodCtx<'_>,
    kwargs: &Map<String, Value>,
    first: &str,
    second: &str,
) -> Result<Question, RusdooError> {
    let calendar_id = only_one(ctx, "ask one working schedule at a time")?;
    let start = argument(ctx, kwargs, first, 0)
        .ok_or_else(|| RusdooError::Validation(format!("say when the window starts ({first})")))?;
    let end = argument(ctx, kwargs, second, 1)
        .ok_or_else(|| RusdooError::Validation(format!("say when the window ends ({second})")))?;
    let start = parse_datetime(start, first)?;
    let end = parse_datetime(end, second)?;
    if end < start {
        return Err(RusdooError::Validation(format!(
            "the window ends before it starts: {first} is after {second}"
        )));
    }
    let schedule = load::load_schedule(&ctx.registry, ctx.pool, calendar_id).await?;
    Ok(Question {
        calendar_id,
        schedule,
        start,
        end,
    })
}

/// Whether the caller wants time off taken into account. Odoo's
/// `compute_leaves`, with Odoo's defaults — on for the two counting
/// methods, off for the two planning ones.
fn wants_leaves(
    ctx: &MethodCtx<'_>,
    kwargs: &Map<String, Value>,
    position: usize,
    fallback: bool,
) -> bool {
    argument(ctx, kwargs, "compute_leaves", position)
        .and_then(Value::as_bool)
        .unwrap_or(fallback)
}

async fn leaves_for(
    ctx: &MethodCtx<'_>,
    question: &Question,
    wanted: bool,
) -> Result<Vec<calendar::Leave>, RusdooError> {
    if !wanted {
        return Ok(Vec::new());
    }
    load::load_leaves(
        ctx.registry.as_ref(),
        ctx.pool,
        Some(question.calendar_id),
        None,
        question.start,
        question.end,
    )
    .await
}

/// `get_work_hours_count` — how many working hours the window holds.
fn get_work_hours_count<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let asked = question(&ctx, kwargs, "start_dt", "end_dt").await?;
        let leaves = leaves_for(&ctx, &asked, wants_leaves(&ctx, kwargs, 2, true)).await?;
        Ok(json!(calendar::work_hours_count(
            &asked.schedule,
            &leaves,
            asked.start,
            asked.end
        )))
    })
}

/// `get_work_duration_data` — the same window in days and in hours.
fn get_work_duration_data<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let asked = question(&ctx, kwargs, "from_datetime", "to_datetime").await?;
        let leaves = leaves_for(&ctx, &asked, wants_leaves(&ctx, kwargs, 2, true)).await?;
        let intervals = if leaves.is_empty() {
            calendar::attendance_intervals(&asked.schedule, asked.start, asked.end)
        } else {
            calendar::work_intervals(&asked.schedule, &leaves, asked.start, asked.end)
        };
        let data = calendar::attendance_days_data(&asked.schedule, &intervals);
        Ok(json!({"days": data.days, "hours": data.hours}))
    })
}

/// `plan_hours` — when `hours` of work will be done, counting from a
/// moment. A negative number counts backwards.
fn plan_hours<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let calendar_id = only_one(&ctx, "plan against one working schedule at a time")?;
        let hours = argument(&ctx, kwargs, "hours", 0)
            .and_then(Value::as_f64)
            .ok_or_else(|| RusdooError::Validation("say how many hours to plan".into()))?;
        let from = parse_datetime(
            argument(&ctx, kwargs, "day_dt", 1)
                .ok_or_else(|| RusdooError::Validation("say when to count from (day_dt)".into()))?,
            "day_dt",
        )?;
        let schedule = load::load_schedule(&ctx.registry, ctx.pool, calendar_id).await?;
        // Odoo's default here is *off*: planning is normally asked of the
        // schedule alone, and a caller who wants the holidays honoured
        // says so
        let leaves = if wants_leaves(&ctx, kwargs, 2, false) {
            // the horizon the search walks, so the leaves are loaded once
            // instead of once per fortnight
            let horizon = from + chrono::Duration::days(1400);
            let (start, end) = if hours >= 0.0 { (from, horizon) } else { (from - chrono::Duration::days(1400), from) };
            load::load_leaves(&ctx.registry, ctx.pool, Some(calendar_id), None, start, end).await?
        } else {
            Vec::new()
        };
        Ok(match calendar::plan_hours(&schedule, &leaves, hours, from) {
            Some(moment) => json!(format_datetime(moment)),
            // Odoo answers False when the horizon runs out; a client that
            // gets a date back for an impossible plan is worse off
            None => json!(false),
        })
    })
}

/// `plan_days` — the end of the `days`-th working day from a moment.
fn plan_days<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let calendar_id = only_one(&ctx, "plan against one working schedule at a time")?;
        let days = argument(&ctx, kwargs, "days", 0)
            .and_then(Value::as_i64)
            .ok_or_else(|| RusdooError::Validation("say how many days to plan".into()))?;
        let from = parse_datetime(
            argument(&ctx, kwargs, "day_dt", 1)
                .ok_or_else(|| RusdooError::Validation("say when to count from (day_dt)".into()))?,
            "day_dt",
        )?;
        let schedule = load::load_schedule(&ctx.registry, ctx.pool, calendar_id).await?;
        let leaves = if wants_leaves(&ctx, kwargs, 2, false) {
            let horizon = chrono::Duration::days(1400);
            let (start, end) = if days >= 0 { (from, from + horizon) } else { (from - horizon, from) };
            load::load_leaves(&ctx.registry, ctx.pool, Some(calendar_id), None, start, end).await?
        } else {
            Vec::new()
        };
        Ok(match calendar::plan_days(&schedule, &leaves, days, from) {
            Some(moment) => json!(format_datetime(moment)),
            None => json!(false),
        })
    })
}

/// `_get_unusual_days` — which days of the window this schedule does not
/// work. What a date picker greys out.
fn get_unusual_days<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let asked = question(&ctx, kwargs, "start_dt", "end_dt").await?;
        let leaves = leaves_for(&ctx, &asked, true).await?;
        let days = calendar::unusual_days(&asked.schedule, &leaves, asked.start, asked.end);
        let mut answer = Map::new();
        for (day, unusual) in days {
            answer.insert(format_date(day), json!(unusual));
        }
        Ok(Value::Object(answer))
    })
}

/// `_check_attendance_ids` — the rule Odoo writes as `@api.constrains`.
///
/// It is a method here and not a constraint because a constraint in this
/// ORM sees one record's own fields, and this rule is about the periods
/// *under* a schedule: whether any two of them clash. That gap is
/// written up in the crate's report; until it closes, a client saves the
/// periods and then asks.
fn validate_attendances<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let calendar_id = only_one(&ctx, "check one working schedule at a time")?;
        let schedule = load::load_schedule(&ctx.registry, ctx.pool, calendar_id).await?;
        calendar::check_attendances(&schedule).map_err(RusdooError::Validation)?;
        Ok(json!(true))
    })
}

/// `switch_calendar_type` — turn a weekly schedule into a fortnightly one
/// and back.
///
/// Going in, every period is duplicated into both weeks with a section
/// heading in front of each; coming back, the fortnight is thrown away
/// and the first week's periods are what remains. Odoo rebuilds from the
/// company default instead — which discards whatever the user had
/// encoded. Keeping the first week is the same shape of answer and
/// loses less.
fn switch_calendar_type<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let calendar_id = only_one(&ctx, "switch one working schedule at a time")?;
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "resource.calendar",
                &[calendar_id],
                &["two_weeks_calendar", "attendance_ids"],
            )
            .await?;
        let calendar = rows.first().ok_or_else(|| {
            RusdooError::Validation(format!("working schedule {calendar_id} does not exist"))
        })?;
        let two_weeks = calendar
            .get("two_weeks_calendar")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let attendance_ids: Vec<i64> = calendar
            .get("attendance_ids")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let attendances = load::load_attendances(&ctx.registry, ctx.pool, &attendance_ids).await?;
        let names = attendance_names(&ctx.registry, ctx.pool, &attendance_ids).await?;

        let mut commands: Vec<Value> = vec![json!([5, 0, 0])];
        if two_weeks {
            // back to one week: the first week is what the schedule
            // becomes, and the section headings go
            for (index, attendance) in attendances
                .iter()
                .filter(|a| a.display_type.is_none() && a.week_type != Some(1))
                .enumerate()
            {
                commands.push(new_period(attendance, &names, None, index as i32 + 1));
            }
        } else {
            commands.push(section("First week", 0, "0"));
            commands.push(section("Second week", 25, "1"));
            for (index, attendance) in attendances
                .iter()
                .filter(|a| a.display_type.is_none())
                .enumerate()
            {
                commands.push(new_period(attendance, &names, Some("0"), index as i32 + 1));
                commands.push(new_period(attendance, &names, Some("1"), index as i32 + 26));
            }
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "resource.calendar",
                &[calendar_id],
                vec![
                    ("two_weeks_calendar", json!(!two_weeks)),
                    ("attendance_ids", Value::Array(commands)),
                ],
            )
            .await?;
        Ok(json!(!two_weeks))
    })
}

/// The heading row that opens one of the two weeks.
fn section(name: &str, sequence: i32, week_type: &str) -> Value {
    json!([0, 0, {
        "name": name,
        "dayofweek": "0",
        "sequence": sequence,
        "hour_from": 0.0,
        "hour_to": 0.0,
        "day_period": "morning",
        "week_type": week_type,
        "display_type": "line_section",
    }])
}

/// `_copy_attendance_vals` — the same period, in the week it is going to.
fn new_period(
    attendance: &Attendance,
    names: &std::collections::HashMap<i64, String>,
    week_type: Option<&str>,
    sequence: i32,
) -> Value {
    json!([0, 0, {
        "name": names.get(&attendance.id).cloned().unwrap_or_default(),
        "dayofweek": attendance.dayofweek.to_string(),
        "hour_from": attendance.hour_from,
        "hour_to": attendance.hour_to,
        "day_period": attendance.day_period,
        "week_type": week_type,
        "sequence": sequence,
    }])
}

/// The names of the periods being copied — the one field the arithmetic
/// has no use for and a screen does.
async fn attendance_names(
    registry: &Registry,
    pool: &sqlx::PgPool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, String>, RusdooError> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = registry
        .read(pool, "resource.calendar.attendance", ids, &["name"])
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get("id")?.as_i64()?,
                row.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// `_adjust_to_calendar` — pull a start and an end onto the nearest
/// hours the resource actually works.
///
/// Odoo's example says it best: with attendances of 8–13 and 14–17, a
/// job asked to run from 9am to 6pm really runs from 8am to 5pm. A
/// boundary with no working time on its day answers `false`, because
/// there is nothing honest to move it to.
fn adjust_to_calendar<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let resource_id = only_one(&ctx, "adjust one resource at a time")?;
        let start = parse_datetime(
            argument(&ctx, kwargs, "start", 0)
                .ok_or_else(|| RusdooError::Validation("say when the job starts".into()))?,
            "start",
        )?;
        let end = parse_datetime(
            argument(&ctx, kwargs, "end", 1)
                .ok_or_else(|| RusdooError::Validation("say when the job ends".into()))?,
            "end",
        )?;
        let rows = ctx
            .registry
            .read(ctx.pool, "resource.resource", &[resource_id], &["calendar_id"])
            .await?;
        let calendar_id = rows
            .first()
            .ok_or_else(|| {
                RusdooError::Validation(format!("resource {resource_id} does not exist"))
            })?
            .get("calendar_id")
            .and_then(first_id);
        // a resource with no schedule works any hour: nothing to pull
        let Some(calendar_id) = calendar_id else {
            return Ok(json!([format_datetime(start), format_datetime(end)]));
        };
        let schedule = load::load_schedule(&ctx.registry, ctx.pool, calendar_id).await?;
        // the whole of the first day through the whole of the last, like
        // Odoo's `search_range`
        let range_start = start.date().and_hms_opt(0, 0, 0).expect("midnight");
        let range_end = (end.date() + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight");
        let leaves = load::load_leaves(
            ctx.registry.as_ref(),
            ctx.pool,
            Some(calendar_id),
            Some(resource_id),
            range_start,
            range_end,
        )
        .await?;
        let opened = calendar::closest_work_time(
            &schedule,
            &leaves,
            start,
            (range_start, range_end),
            false,
        );
        // the end is looked for from the real start onwards, so a job is
        // never pulled to close before it opens
        let closed = calendar::closest_work_time(
            &schedule,
            &leaves,
            start.max(end),
            (start, range_end),
            true,
        );
        Ok(json!([
            opened.map_or(Value::Bool(false), |it| json!(format_datetime(it))),
            closed.map_or(Value::Bool(false), |it| json!(format_datetime(it))),
        ]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depends(hours: &[(f64, f64, &str, &str)]) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert(
            "attendance_ids.hour_from".into(),
            json!(hours.iter().map(|h| h.0).collect::<Vec<f64>>()),
        );
        record.insert(
            "attendance_ids.hour_to".into(),
            json!(hours.iter().map(|h| h.1).collect::<Vec<f64>>()),
        );
        record.insert(
            "attendance_ids.day_period".into(),
            json!(hours.iter().map(|h| h.2).collect::<Vec<&str>>()),
        );
        record.insert(
            "attendance_ids.dayofweek".into(),
            json!(hours.iter().map(|h| h.3).collect::<Vec<&str>>()),
        );
        record.insert("attendance_ids.week_type".into(), json!([]));
        record.insert("attendance_ids.display_type".into(), json!([]));
        record
    }

    #[test]
    fn the_weekly_average_adds_the_worked_periods_and_skips_the_breaks() {
        let record = depends(&[
            (8.0, 12.0, "morning", "0"),
            (12.0, 13.0, "lunch", "0"),
            (13.0, 17.0, "afternoon", "0"),
        ]);
        assert_eq!(hours_per_week(&record), json!(8.0));
        assert_eq!(hours_per_day(&record), json!(8.0));
    }

    #[test]
    fn a_schedule_with_no_periods_averages_nothing_rather_than_dividing_by_zero() {
        let record = depends(&[]);
        assert_eq!(hours_per_week(&record), json!(0.0));
        assert_eq!(hours_per_day(&record), json!(0.0));
    }

    #[test]
    fn three_mornings_a_week_is_three_days_of_four_hours() {
        let record = depends(&[
            (8.0, 12.0, "morning", "0"),
            (8.0, 12.0, "morning", "1"),
            (8.0, 12.0, "morning", "2"),
        ]);
        assert_eq!(hours_per_week(&record), json!(12.0));
        assert_eq!(hours_per_day(&record), json!(4.0));
    }

    #[test]
    fn the_flexible_flag_follows_the_schedule_type() {
        let mut record = Map::new();
        record.insert("schedule_type".into(), json!("flexible"));
        assert_eq!(flexible_hours(&record), json!(true));
        record.insert("schedule_type".into(), json!("fully_fixed"));
        assert_eq!(flexible_hours(&record), json!(false));
    }

    #[test]
    fn a_part_time_week_is_a_percentage_of_the_full_one() {
        let mut record = Map::new();
        record.insert("full_time_required_hours".into(), json!(40.0));
        record.insert("hours_per_week".into(), json!(20.0));
        assert_eq!(work_time_rate(&record), json!(50.0));
        assert_eq!(is_fulltime(&record), json!(false));
        record.insert("hours_per_week".into(), json!(40.0));
        assert_eq!(work_time_rate(&record), json!(100.0));
        assert_eq!(is_fulltime(&record), json!(true));
    }

    #[test]
    fn a_schedule_with_no_full_time_reference_is_full_time() {
        let mut record = Map::new();
        record.insert("full_time_required_hours".into(), json!(0.0));
        record.insert("hours_per_week".into(), json!(12.0));
        assert_eq!(work_time_rate(&record), json!(100.0));
    }

    #[test]
    fn a_break_lasts_no_working_hours_and_no_days() {
        let mut record = Map::new();
        record.insert("day_period".into(), json!("lunch"));
        record.insert("hour_from".into(), json!(12.0));
        record.insert("hour_to".into(), json!(13.0));
        assert_eq!(duration_hours(&record), json!(0.0));
        record.insert("duration_hours".into(), json!(0.0));
        assert_eq!(duration_days(&record), json!(0.0));
    }

    #[test]
    fn a_morning_is_half_a_day_measured_against_its_own_schedule() {
        let mut record = Map::new();
        record.insert("day_period".into(), json!("morning"));
        record.insert("duration_hours".into(), json!(4.0));
        // the dependency arrives as a one-element list, like every
        // many2one hop
        record.insert("calendar_id.hours_per_day".into(), json!([8.0]));
        assert_eq!(duration_days(&record), json!(0.5));
        // the very same four hours are a whole day on a schedule whose
        // day is five hours long
        record.insert("calendar_id.hours_per_day".into(), json!([5.0]));
        assert_eq!(duration_days(&record), json!(1.0));
    }

    #[test]
    fn a_period_outside_the_day_or_running_backwards_is_refused() {
        let mut record = Map::new();
        record.insert("hour_from".into(), json!(8.0));
        record.insert("hour_to".into(), json!(17.0));
        assert!(hours_make_a_period(&record).is_ok());
        record.insert("hour_to".into(), json!(30.0));
        assert!(hours_make_a_period(&record).is_err());
        record.insert("hour_from".into(), json!(17.0));
        record.insert("hour_to".into(), json!(8.0));
        let error = hours_make_a_period(&record).expect_err("17 to 8 runs backwards");
        assert!(error.contains("backwards"), "{error}");
    }

    #[test]
    fn a_break_on_a_duration_schedule_is_refused() {
        let mut record = Map::new();
        record.insert("day_period".into(), json!("lunch"));
        record.insert("duration_based".into(), json!(true));
        assert!(no_break_on_a_duration_calendar(&record).is_err());
        record.insert("duration_based".into(), json!(false));
        assert!(no_break_on_a_duration_calendar(&record).is_ok());
    }

    #[test]
    fn time_off_that_ends_before_it_starts_is_refused() {
        let mut record = Map::new();
        record.insert("date_from".into(), json!("2025-06-04 08:00:00"));
        record.insert("date_to".into(), json!("2025-06-04 17:00:00"));
        assert!(time_off_runs_forwards(&record).is_ok());
        record.insert("date_to".into(), json!("2025-06-03 17:00:00"));
        assert!(time_off_runs_forwards(&record).is_err());
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in [
            "resource.calendar",
            "resource.calendar.attendance",
            "resource.calendar.leaves",
            "resource.resource",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // a period without a schedule is not a period, and the schedule
        // takes its week with it when it goes
        let period = reg.get("resource.calendar.attendance").unwrap();
        let calendar = period.field("calendar_id").unwrap();
        assert!(calendar.required);
        assert_eq!(calendar.ondelete, Some(OnDelete::Cascade));
        // the company gained its default schedule without losing what
        // base gave it
        let company = reg.get("res.company").unwrap();
        assert!(company.field("resource_calendar_id").is_some());
        assert!(company.field("name").unwrap().required);
        assert_eq!(company.meta.table, "res_company");
    }

    #[test]
    fn a_schedule_answers_the_questions_a_planning_module_asks() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("resource.calendar"),
            vec![
                "get_unusual_days",
                "get_work_duration_data",
                "get_work_hours_count",
                "plan_days",
                "plan_hours",
                "switch_calendar_type",
                "validate_attendances",
            ]
        );
        assert_eq!(
            methods.names_for("resource.resource"),
            vec!["adjust_to_calendar"]
        );
    }
}
