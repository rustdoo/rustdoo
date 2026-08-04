//! The models of `odoo/addons/calendar/models/`: a meeting, the people
//! invited to it, the reminders it carries and the rule that repeats it.

use crate::rrule::{self, Rule};
use rusdoo_core::RusdooError;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{DefaultCtx, DefaultFn, Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// `CalendarAttendee.STATE_SELECTION` — the four answers to an invitation.
pub const ATTENDEE_STATES: [(&str, &str); 4] = [
    ("accepted", "Yes"),
    ("declined", "No"),
    ("tentative", "Maybe"),
    ("needsAction", "Needs Action"),
];

/// The answer an invitation starts on.
pub const NEEDS_ACTION: &str = "needsAction";

/// `calendar.block_mail` and friends live on `ir.config_parameter`; this
/// is the one the recurrence expansion reads.
pub const MAX_RECURRENCE_YEARS_PARAM: &str = "calendar.max_recurrence_years";

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

fn selection(name: &str, choices: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            choices
                .iter()
                .map(|(value, label)| ((*value).to_string(), (*label).to_string()))
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

/// The id out of a many2one value, which reads as `[id, name]`.
pub(crate) fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// A dependency gathered over an x2many comes back as one value per
/// linked record; a many2one hop as a list of one. Both read the same.
fn gathered<'a>(record: &'a Map<String, Value>, path: &str) -> &'a [Value] {
    record
        .get(path)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// Register the calendar's models.
///
/// The order is the one a reader follows: the small vocabularies first,
/// then the meeting, then what hangs off it. `calendar.event` and
/// `calendar.recurrence` point at each other, which no order can fix —
/// the foreign keys are added once every table exists.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(event_type())?;
    reg.register(alarm())?;
    reg.register(recurrence())?;
    reg.register(event())?;
    reg.register(attendee())?;
    reg.register(filters())?;
    reg.register(partner())?;
    Ok(())
}

// ---------------------------------------------------------------------
// calendar.event.type
// ---------------------------------------------------------------------

/// `calendar.event.type` — the tags a meeting is filed under.
fn event_type() -> Model {
    Model::new(
        meta("calendar.event.type", "calendar_event_type"),
        vec![
            char("name").required().translatable(),
            // Odoo draws a random colour on create; a random default is
            // not something a data file can reproduce, so a new tag is
            // uncoloured until somebody picks
            Field::new("color", FieldType::Integer).default_value(json!(0)),
        ],
    )
    // two tags with the same name are two tags nobody can tell apart,
    // and the check has to be the database's: two people typing at once
    // both pass a check made before the insert
    .sql_constrained(
        "calendar_event_type_name_uniq",
        r#"UNIQUE ("name")"#,
        "a meeting type with that name already exists",
    )
    .ordered("name, id")
}

// ---------------------------------------------------------------------
// calendar.alarm
// ---------------------------------------------------------------------

/// `duration_minutes` — the reminder's lead time in one unit.
///
/// Stored, like Odoo's: the alarm scheduler asks "which reminders fire in
/// the next ten minutes" and cannot ask that of a number that only exists
/// once a row is read.
fn duration_minutes(record: &Map<String, Value>) -> Value {
    let duration = record.get("duration").and_then(Value::as_i64).unwrap_or(0);
    let minutes = match record.get("interval").and_then(Value::as_str) {
        Some("minutes") => duration,
        Some("hours") => duration * 60,
        Some("days") => duration * 60 * 24,
        _ => 0,
    };
    json!(minutes)
}

/// `calendar.alarm` — a reminder a meeting can carry.
fn alarm() -> Model {
    Model::new(
        meta("calendar.alarm", "calendar_alarm"),
        vec![
            char("name").required().translatable(),
            selection(
                "alarm_type",
                &[("notification", "Notification"), ("email", "Email")],
            )
            .required()
            .default_value(json!("email")),
            Field::new("duration", FieldType::Integer)
                .required()
                .default_value(json!(1)),
            selection(
                "interval",
                &[
                    ("minutes", "Minutes"),
                    ("hours", "Hours"),
                    ("days", "Days"),
                ],
            )
            .required()
            .default_value(json!("hours")),
            Field::new("duration_minutes", FieldType::Integer)
                .computed(&["interval", "duration"], duration_minutes)
                .store(),
            Field::new("body", FieldType::Text),
            Field::new("notify_responsible", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .constrained("reminder lead time", &["duration"], |record| {
        match record.get("duration").and_then(Value::as_i64) {
            Some(duration) if duration < 0 => Err(
                "a reminder cannot fire after the meeting: give it a lead time of 0 or more"
                    .to_string(),
            ),
            _ => Ok(()),
        }
    })
    .ordered("duration_minutes, id")
}

// ---------------------------------------------------------------------
// calendar.recurrence
// ---------------------------------------------------------------------

/// The rule fields a recurrence is spelled with — what the name and the
/// RRULE both watch, and what the constraint reads.
const RULE_FIELDS: [&str; 16] = [
    "rrule_type",
    "interval",
    "end_type",
    "count",
    "until",
    "mon",
    "tue",
    "wed",
    "thu",
    "fri",
    "sat",
    "sun",
    "month_by",
    "day",
    "weekday",
    "byday",
];

/// `name` — the rule in one line (`get_recurrence_name`).
///
/// A rule that cannot be expanded has no name either. Odoo names it
/// anyway and produces "Every 1 Weeks on  for 3 events", which reads as a
/// rule that lost its days rather than as one that never had any.
fn recurrence_name(record: &Map<String, Value>) -> Value {
    match Rule::from_record(record).and_then(|rule| rule.check().map(|()| rule.name())) {
        Ok(name) => json!(name),
        // a half-written row has no name yet; the constraint is what
        // refuses to save it, and it says why
        Err(_) => Value::Null,
    }
}

/// `rrule` — the same rule as iCalendar spells it.
fn recurrence_rrule(record: &Map<String, Value>) -> Value {
    match Rule::from_record(record).and_then(|rule| rule.to_rrule()) {
        Ok(text) => json!(text),
        Err(_) => Value::Null,
    }
}

/// A rule that cannot be expanded, refused at the write.
///
/// Odoo raises these from `_rrule_serialize` and `_get_rrule`, which run
/// when the events are generated — so a rule saved with no weekday is
/// saved fine and fails later, somewhere else. Refusing it here means the
/// person who typed it is the person who reads the message.
fn recurrence_is_usable(record: &Map<String, Value>) -> Result<(), String> {
    Rule::from_record(record)?.check()
}

/// `calendar.recurrence` — how a meeting repeats.
fn recurrence() -> Model {
    Model::new(
        meta("calendar.recurrence", "calendar_recurrence"),
        vec![
            char("name")
                .computed(&RULE_FIELDS, recurrence_name)
                .store(),
            // the event the series was generated from. `set null` and not
            // `cascade`: deleting one occurrence must not delete the rule
            // that made the others
            m2o("base_event_id", "calendar.event").ondelete(OnDelete::SetNull),
            Field::new(
                "calendar_event_ids",
                FieldType::One2many {
                    comodel: "calendar.event".into(),
                    inverse: "recurrence_id".into(),
                },
            ),
            // Odoo picks from the timezone list; the port has no timezone
            // model, and `res.users.tz` is a plain string here too
            char("event_tz"),
            char("rrule")
                .computed(&RULE_FIELDS, recurrence_rrule)
                .store(),
            selection(
                "rrule_type",
                &[
                    ("daily", "Days"),
                    ("weekly", "Weeks"),
                    ("monthly", "Months"),
                    ("yearly", "Years"),
                ],
            )
            .default_value(json!("weekly")),
            selection(
                "end_type",
                &[
                    ("count", "Number of repetitions"),
                    ("end_date", "End date"),
                    ("forever", "Forever"),
                ],
            )
            .default_value(json!("count")),
            Field::new("interval", FieldType::Integer).default_value(json!(1)),
            Field::new("count", FieldType::Integer).default_value(json!(1)),
            // `until = fields.Date('Repeat Until')`. The rule's own name
            // reads it, the expansion stops at it, and the model had
            // simply never declared it — which made every write to a
            // recurrence fail on a compute that depends on a field the
            // model does not have.
            Field::new("until", FieldType::Date),
            Field::new("mon", FieldType::Boolean).default_value(json!(false)),
            Field::new("tue", FieldType::Boolean).default_value(json!(false)),
            Field::new("wed", FieldType::Boolean).default_value(json!(false)),
            Field::new("thu", FieldType::Boolean).default_value(json!(false)),
            Field::new("fri", FieldType::Boolean).default_value(json!(false)),
            Field::new("sat", FieldType::Boolean).default_value(json!(false)),
            Field::new("sun", FieldType::Boolean).default_value(json!(false)),
            selection(
                "month_by",
                &[("date", "Date of month"), ("day", "Day of month")],
            )
            .default_value(json!("date")),
            Field::new("day", FieldType::Integer).default_value(json!(1)),
            selection(
                "weekday",
                &[
                    ("MON", "Monday"),
                    ("TUE", "Tuesday"),
                    ("WED", "Wednesday"),
                    ("THU", "Thursday"),
                    ("FRI", "Friday"),
                    ("SAT", "Saturday"),
                    ("SUN", "Sunday"),
                ],
            ),
            selection(
                "byday",
                &[
                    ("1", "First"),
                    ("2", "Second"),
                    ("3", "Third"),
                    ("4", "Fourth"),
                    ("-1", "Last"),
                ],
            ),
        ],
    )
    .constrained("usable recurrence", &RULE_FIELDS, recurrence_is_usable)
    // Odoo's `_month_day`, kept as a database CHECK and not only as the
    // rule above: a rule the application never sees — an import, a psql
    // session — must not be able to store a 32nd of the month
    .sql_constrained(
        "calendar_recurrence_month_day",
        "CHECK (rrule_type != 'monthly' OR month_by != 'day' \
         OR day >= 1 AND day <= 31 \
         OR weekday IN ('MON','TUE','WED','THU','FRI','SAT','SUN') \
         AND byday IN ('1','2','3','4','-1'))",
        "the day must be between 1 and 31",
    )
}

// ---------------------------------------------------------------------
// calendar.event
// ---------------------------------------------------------------------

/// The hours between two datetimes, rounded to the minute like Odoo's
/// `_get_duration`.
fn duration_hours(start: &str, stop: &str) -> Option<f64> {
    let start = rrule::parse_datetime(start).ok()?;
    let stop = rrule::parse_datetime(stop).ok()?;
    let seconds = (stop - start).num_seconds() as f64;
    Some((seconds / 3600.0 * 100.0).round() / 100.0)
}

/// The two ends of a meeting, when it has both.
fn span(record: &Map<String, Value>) -> Option<(&str, &str)> {
    let start = record.get("start").and_then(Value::as_str)?;
    let stop = record.get("stop").and_then(Value::as_str)?;
    Some((start, stop))
}

/// `duration` — how long the meeting lasts, in hours.
fn event_duration(record: &Map<String, Value>) -> Value {
    match span(record) {
        Some((start, stop)) => json!(duration_hours(start, stop).unwrap_or(0.0)),
        None => json!(0.0),
    }
}

/// The date half of a datetime, only for an all-day event.
///
/// Port of `_compute_dates`: `start_date`/`stop_date` are what a calendar
/// draws an all-day band from, and they are deliberately empty for a
/// timed event — a band drawn from a timed event would cover the whole
/// day it happens in.
fn day_of(record: &Map<String, Value>, which: &str) -> Value {
    if !record.get("allday").and_then(Value::as_bool).unwrap_or(false) {
        return Value::Null;
    }
    match record
        .get(which)
        .and_then(Value::as_str)
        .and_then(|text| rrule::parse_datetime(text).ok())
    {
        Some(stamp) => json!(stamp.date().to_string()),
        None => Value::Null,
    }
}

fn event_start_date(record: &Map<String, Value>) -> Value {
    day_of(record, "start")
}

fn event_stop_date(record: &Map<String, Value>) -> Value {
    day_of(record, "stop")
}

/// `display_time` — the meeting's when, as one readable string.
///
/// Port of `_get_display_time`. Odoo renders it in the reader's timezone
/// and the language's date format; both are read off the environment, and
/// this ORM has neither on a compute. It renders UTC and says so, which
/// is a string somebody can act on — unlike one that silently claims to
/// be local time.
fn display_time(record: &Map<String, Value>) -> Value {
    let Some((start, stop)) = span(record) else {
        return json!("");
    };
    let (Ok(start), Ok(stop)) = (rrule::parse_datetime(start), rrule::parse_datetime(stop)) else {
        return json!("");
    };
    if record.get("allday").and_then(Value::as_bool).unwrap_or(false) {
        return json!(format!("All Day, {}", start.date()));
    }
    let hours = (stop - start).num_seconds() as f64 / 3600.0;
    if hours < 24.0 {
        return json!(format!(
            "{} at ({} To {}) (UTC)",
            start.date(),
            start.format("%H:%M"),
            stop.format("%H:%M")
        ));
    }
    json!(format!(
        "{} at {} To {} at {} (UTC)",
        start.date(),
        start.format("%H:%M"),
        stop.date(),
        stop.format("%H:%M")
    ))
}

/// How many attendees answered a given way.
fn count_state(record: &Map<String, Value>, state: &str) -> i64 {
    gathered(record, "attendee_ids.state")
        .iter()
        .filter(|value| value.as_str() == Some(state))
        .count() as i64
}

/// `attendees_count` — how many people were invited.
fn attendees_count(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "partner_ids").len())
}

fn accepted_count(record: &Map<String, Value>) -> Value {
    json!(count_state(record, "accepted"))
}

fn declined_count(record: &Map<String, Value>) -> Value {
    json!(count_state(record, "declined"))
}

fn tentative_count(record: &Map<String, Value>) -> Value {
    json!(count_state(record, "tentative"))
}

/// `awaiting_count` — invited, and yet to answer.
///
/// Counted from the invitations and not from the attendee rows on
/// purpose, exactly as Odoo does: a partner added to the meeting before
/// their attendee row exists is still somebody the organizer is waiting
/// on.
fn awaiting_count(record: &Map<String, Value>) -> Value {
    let invited = gathered(record, "partner_ids").len() as i64;
    let answered = count_state(record, "accepted")
        + count_state(record, "declined")
        + count_state(record, "tentative");
    json!((invited - answered).max(0))
}

/// `is_organizer_alone` — everybody else said no.
///
/// Port of `_compute_is_organizer_alone`, including the reason its
/// docstring gives: a meeting whose only attendee is the organizer is a
/// note to self, not a meeting nobody came to, so one attendee is never
/// "alone".
fn is_organizer_alone(record: &Map<String, Value>) -> Value {
    let organizer = record.get("partner_id").and_then(first_id);
    let partners = gathered(record, "attendee_ids.partner_id");
    let states = gathered(record, "attendee_ids.state");
    if partners.len() < 2 {
        return json!(false);
    }
    let others_all_declined = partners
        .iter()
        .zip(states)
        .filter(|(partner, _)| first_id(partner) != organizer)
        .all(|(_, state)| state.as_str() == Some("declined"));
    json!(others_all_declined)
}

/// `_check_closing_date` — a meeting that ends before it starts.
fn ends_after_it_starts(record: &Map<String, Value>) -> Result<(), String> {
    let name = record.get("name").and_then(Value::as_str).unwrap_or("");
    let Some((start, stop)) = span(record) else {
        return Ok(());
    };
    let (Ok(from), Ok(to)) = (rrule::parse_datetime(start), rrule::parse_datetime(stop)) else {
        return Ok(());
    };
    if to < from {
        return Err(format!(
            "the ending date and time cannot be earlier than the starting date and time: \
             meeting “{name}” starts at {start} and ends at {stop}"
        ));
    }
    Ok(())
}

/// Writing `partner_ids` is what makes the guest list, as in Odoo:
/// `create` and `write` there put a `calendar.attendee` behind every
/// partner, because a guest with nowhere to answer from is on a list and
/// not invited.
///
/// This is the port of that override. Until the ORM had hooks it lived
/// in `action_join_meeting`, which everything inside this port called and
/// nothing outside it did — and Odoo's own client is outside it, writing
/// the field straight onto the form.
fn seat_the_guests<'a>(
    mut ctx: rusdoo_orm::hooks::HookCtx<'a>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RusdooError>> + Send + 'a>> {
    Box::pin(async move {
        if !ctx.wrote("partner_ids") {
            return Ok(());
        }
        // who is on the list now, after the write the hook is reacting to
        let events = ctx.records(&["partner_ids"]).await?;
        for event in events {
            let Some(id) = event.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let wanted: Vec<i64> = event
                .get("partner_ids")
                .and_then(Value::as_array)
                .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            // and who already has a place to answer from
            let seated_ids = ctx
                .registry
                .search_tx(
                    ctx.tx,
                    "calendar.attendee",
                    &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", id]]))?,
                    &rusdoo_orm::crud::SearchOptions::default(),
                )
                .await?;
            let seated: Vec<i64> = if seated_ids.is_empty() {
                Vec::new()
            } else {
                let conn: &mut sqlx::PgConnection = &mut *ctx.tx;
                ctx.registry
                    .read_conn(conn, "calendar.attendee", &seated_ids, &["partner_id"])
                    .await?
                    .iter()
                    .filter_map(|row| match row.get("partner_id") {
                        Some(Value::Array(pair)) => pair.first().and_then(Value::as_i64),
                        other => other.and_then(Value::as_i64),
                    })
                    .collect()
            };
            for partner in wanted.iter().filter(|p| !seated.contains(p)) {
                ctx.registry
                    .create_tx(
                        ctx.tx,
                        "calendar.attendee",
                        vec![("event_id", json!(id)), ("partner_id", json!(partner))],
                    )
                    .await?;
            }
        }
        Ok(())
    })
}

/// `calendar.event` — a meeting.
fn event() -> Model {
    Model::new(
        meta("calendar.event", "calendar_event"),
        vec![
            char("name").required(),
            Field::new("description", FieldType::Html),
            Field::new("notes", FieldType::Html),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            m2o("partner_id", "res.partner").related("user_id.partner_id"),
            char("location"),
            // Odoo computes this from an access token and the base URL;
            // both live on the HTTP layer, so here it is what somebody
            // pasted in — which is also what Odoo's `custom` source is
            char("videocall_location"),
            char("access_token"),
            selection(
                "privacy",
                &[
                    ("public", "Public"),
                    ("private", "Private"),
                    ("confidential", "Only internal users"),
                ],
            ),
            selection("show_as", &[("free", "Available"), ("busy", "Busy")])
                .required()
                .default_value(json!("busy")),
            // a meeting that was called off is archived, never deleted:
            // the invitations that went out still point at it
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new(
                "categ_ids",
                FieldType::Many2many {
                    comodel: "calendar.event.type".into(),
                    relation: "meeting_category_rel".into(),
                    column1: "event_id".into(),
                    column2: "type_id".into(),
                },
            ),
            Field::new("start", FieldType::Datetime).required(),
            Field::new("stop", FieldType::Datetime).required(),
            Field::new("allday", FieldType::Boolean).default_value(json!(false)),
            Field::new("start_date", FieldType::Date)
                .computed(&["allday", "start", "stop"], event_start_date)
                .store(),
            Field::new("stop_date", FieldType::Date)
                .computed(&["allday", "start", "stop"], event_stop_date)
                .store(),
            Field::new("duration", FieldType::Float { digits: None })
                .computed(&["start", "stop"], event_duration)
                .store(),
            char("display_time").computed(&["start", "stop", "allday"], display_time),
            // which document the meeting was scheduled from. Odoo points
            // at an `ir.model` row; there is no `ir.model` here, so this
            // is the model's name, like `mail.message` already stores it
            char("res_model"),
            Field::new("res_id", FieldType::Integer),
            Field::new(
                "attendee_ids",
                FieldType::One2many {
                    comodel: "calendar.attendee".into(),
                    inverse: "event_id".into(),
                },
            ),
            Field::new(
                "partner_ids",
                FieldType::Many2many {
                    comodel: "res.partner".into(),
                    relation: "calendar_event_res_partner_rel".into(),
                    column1: "calendar_event_id".into(),
                    column2: "res_partner_id".into(),
                },
            ),
            Field::new(
                "alarm_ids",
                FieldType::Many2many {
                    comodel: "calendar.alarm".into(),
                    relation: "calendar_alarm_calendar_event_rel".into(),
                    column1: "calendar_event_id".into(),
                    column2: "calendar_alarm_id".into(),
                },
            ),
            Field::new("recurrency", FieldType::Boolean).default_value(json!(false)),
            m2o("recurrence_id", "calendar.recurrence"),
            // false marks an occurrence somebody edited: it keeps its
            // place in the series but the rule no longer moves it
            Field::new("follow_recurrence", FieldType::Boolean).default_value(json!(false)),
            Field::new("attendees_count", FieldType::Integer)
                .computed(&["partner_ids"], attendees_count),
            Field::new("accepted_count", FieldType::Integer)
                .computed(&["attendee_ids.state"], accepted_count),
            Field::new("declined_count", FieldType::Integer)
                .computed(&["attendee_ids.state"], declined_count),
            Field::new("tentative_count", FieldType::Integer)
                .computed(&["attendee_ids.state"], tentative_count),
            Field::new("awaiting_count", FieldType::Integer)
                .computed(&["partner_ids", "attendee_ids.state"], awaiting_count),
            Field::new("is_organizer_alone", FieldType::Boolean).computed(
                &["partner_id", "attendee_ids.partner_id", "attendee_ids.state"],
                is_organizer_alone,
            ),
        ],
    )
    .constrained(
        "the meeting ends after it starts",
        &["start", "stop", "name"],
        ends_after_it_starts,
    )
    // Odoo's `_order`: the calendar opens on what is coming, and the list
    // reads newest first
    .ordered("start desc, id desc")
    .on_create("os convidados ganham lugar", seat_the_guests)
    .on_write("os convidados ganham lugar", seat_the_guests)
}

// ---------------------------------------------------------------------
// calendar.attendee
// ---------------------------------------------------------------------

/// `_default_access_token` — the secret in the link an invitation mail
/// carries, so that answering it needs no login.
///
/// A dynamic default and not a constant for the obvious reason: a token
/// every attendee shares is not a token.
fn new_access_token(
    _ctx: DefaultCtx<'_>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RusdooError>> + Send + '_>> {
    Box::pin(async move { Ok(json!(uuid::Uuid::new_v4().simple().to_string())) })
}

const ACCESS_TOKEN: DefaultFn = new_access_token;

/// `common_name` — who the attendee is, on one line.
fn common_name(record: &Map<String, Value>) -> Value {
    let named = gathered(record, "partner_id.name")
        .first()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(name) = named {
        return json!(name);
    }
    // Odoo falls back to the email, which is the only other thing an
    // attendee imported from another calendar is guaranteed to have
    match gathered(record, "partner_id.email")
        .first()
        .and_then(Value::as_str)
    {
        Some(email) => json!(email),
        None => Value::Null,
    }
}

/// `calendar.attendee` — one person's place at one meeting.
fn attendee() -> Model {
    Model::new(
        meta("calendar.attendee", "calendar_attendee"),
        vec![
            m2o("event_id", "calendar.event")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("partner_id", "res.partner")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("recurrence_id", "calendar.recurrence").related("event_id.recurrence_id"),
            char("email").related("partner_id.email"),
            char("phone").related("partner_id.phone"),
            char("common_name")
                .computed(&["partner_id.name", "partner_id.email"], common_name)
                .store(),
            char("access_token").default_from(ACCESS_TOKEN),
            selection("state", &ATTENDEE_STATES).default_value(json!(NEEDS_ACTION)),
            selection("availability", &[("free", "Available"), ("busy", "Busy")]),
        ],
    )
    // one person cannot hold two places at one meeting; the database is
    // what enforces it, because two invitations sent at once both pass a
    // check made before the insert
    .sql_constrained(
        "calendar_attendee_event_partner_uniq",
        r#"UNIQUE ("event_id", "partner_id")"#,
        "that person is already invited to this meeting",
    )
    // Odoo's `_order = 'create_date ASC'`, with the id to break ties: two
    // attendees added in the same statement share a timestamp
    .ordered("create_date, id")
}

// ---------------------------------------------------------------------
// calendar.filters
// ---------------------------------------------------------------------

/// `calendar.filters` — whose calendar a user has ticked on their screen.
fn filters() -> Model {
    Model::new(
        meta("calendar.filters", "calendar_filters"),
        vec![
            m2o("user_id", "res.users")
                .required()
                .ondelete(OnDelete::Cascade)
                .default_from(defaults::CURRENT_USER),
            m2o("partner_id", "res.partner").required(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new("partner_checked", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .sql_constrained(
        "calendar_filters_user_partner_uniq",
        r#"UNIQUE ("user_id", "partner_id")"#,
        "a user cannot have the same contact twice",
    )
}

// ---------------------------------------------------------------------
// res.partner
// ---------------------------------------------------------------------

/// `meeting_count` — how many meetings this contact is at.
fn meeting_count(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "meeting_ids").len())
}

/// `res.partner` extended: a contact's meetings.
///
/// The relation table is `calendar.event.partner_ids`' own, with the two
/// columns the other way round — which is how Odoo declares it too. Both
/// sides create the table `IF NOT EXISTS`, so whichever model is
/// initialized first wins and the other finds it already there.
fn partner() -> Model {
    Model::new(
        extends("res.partner", "res_partner"),
        vec![
            Field::new(
                "meeting_ids",
                FieldType::Many2many {
                    comodel: "calendar.event".into(),
                    relation: "calendar_event_res_partner_rel".into(),
                    column1: "res_partner_id".into(),
                    column2: "calendar_event_id".into(),
                },
            ),
            // not materialized: the count changes when an *event* writes
            // its `partner_ids`, and the recompute only follows the
            // fields of whatever is being written — a column here would
            // go stale and lie
            Field::new("meeting_count", FieldType::Integer)
                .computed(&["meeting_ids"], meeting_count),
            // when this contact last dismissed a reminder popup; the
            // scheduler will not raise the same one twice
            Field::new("calendar_last_notif_ack", FieldType::Datetime)
                .default_from(defaults::NOW),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Value) -> Map<String, Value> {
        match pairs {
            Value::Object(map) => map,
            _ => panic!("the tests pass objects"),
        }
    }

    #[test]
    fn a_reminders_lead_time_is_the_same_number_in_minutes() {
        assert_eq!(
            duration_minutes(&record(json!({"interval": "minutes", "duration": 15}))),
            json!(15)
        );
        assert_eq!(
            duration_minutes(&record(json!({"interval": "hours", "duration": 2}))),
            json!(120)
        );
        assert_eq!(
            duration_minutes(&record(json!({"interval": "days", "duration": 1}))),
            json!(1440)
        );
        // a row with no unit yet is zero, not null: the scheduler
        // compares it with a clock
        assert_eq!(duration_minutes(&Map::new()), json!(0));
    }

    #[test]
    fn a_meetings_duration_is_the_hours_between_its_ends() {
        let timed = record(json!({"start": "2026-03-04 09:00:00", "stop": "2026-03-04 10:30:00"}));
        assert_eq!(event_duration(&timed), json!(1.5));
        // rounded to the minute, so a meeting does not end at 4:19:59
        let odd = record(json!({"start": "2026-03-04 09:00:00", "stop": "2026-03-04 09:00:59"}));
        assert_eq!(event_duration(&odd), json!(0.02));
        assert_eq!(event_duration(&Map::new()), json!(0.0));
    }

    #[test]
    fn only_an_all_day_meeting_carries_the_dates_a_band_is_drawn_from() {
        let allday = record(json!({
            "allday": true,
            "start": "2026-03-04 08:00:00",
            "stop": "2026-03-06 18:00:00"
        }));
        assert_eq!(event_start_date(&allday), json!("2026-03-04"));
        assert_eq!(event_stop_date(&allday), json!("2026-03-06"));

        let timed = record(json!({
            "allday": false,
            "start": "2026-03-04 08:00:00",
            "stop": "2026-03-04 09:00:00"
        }));
        assert_eq!(event_start_date(&timed), Value::Null);
        assert_eq!(event_stop_date(&timed), Value::Null);
    }

    #[test]
    fn the_display_time_says_which_of_the_three_shapes_it_is() {
        let short = record(json!({
            "start": "2026-03-04 09:00:00",
            "stop": "2026-03-04 10:30:00",
            "allday": false
        }));
        assert_eq!(display_time(&short), json!("2026-03-04 at (09:00 To 10:30) (UTC)"));

        let long = record(json!({
            "start": "2026-03-04 09:00:00",
            "stop": "2026-03-06 18:00:00",
            "allday": false
        }));
        assert_eq!(
            display_time(&long),
            json!("2026-03-04 at 09:00 To 2026-03-06 at 18:00 (UTC)")
        );

        let allday = record(json!({
            "start": "2026-03-04 08:00:00",
            "stop": "2026-03-04 18:00:00",
            "allday": true
        }));
        assert_eq!(display_time(&allday), json!("All Day, 2026-03-04"));
    }

    #[test]
    fn the_counters_add_up_to_the_number_of_people_invited() {
        let event = record(json!({
            "partner_ids": [1, 2, 3, 4],
            "attendee_ids.state": ["accepted", "declined", "tentative"]
        }));
        assert_eq!(attendees_count(&event), json!(4));
        assert_eq!(accepted_count(&event), json!(1));
        assert_eq!(declined_count(&event), json!(1));
        assert_eq!(tentative_count(&event), json!(1));
        // the fourth person was invited and has not answered — including
        // when their attendee row is not written yet
        assert_eq!(awaiting_count(&event), json!(1));
    }

    #[test]
    fn nobody_is_waiting_on_an_empty_meeting() {
        assert_eq!(attendees_count(&Map::new()), json!(0));
        assert_eq!(awaiting_count(&Map::new()), json!(0));
    }

    #[test]
    fn the_organizer_is_alone_when_everybody_else_said_no() {
        let alone = record(json!({
            "partner_id": [7, "Ana"],
            "attendee_ids.partner_id": [[7, "Ana"], [9, "Beto"]],
            "attendee_ids.state": ["accepted", "declined"]
        }));
        assert_eq!(is_organizer_alone(&alone), json!(true));

        let company = record(json!({
            "partner_id": [7, "Ana"],
            "attendee_ids.partner_id": [[7, "Ana"], [9, "Beto"]],
            "attendee_ids.state": ["accepted", "needsAction"]
        }));
        assert_eq!(is_organizer_alone(&company), json!(false));
    }

    #[test]
    fn a_meeting_with_only_its_organizer_is_a_note_and_not_a_desertion() {
        let solo = record(json!({
            "partner_id": [7, "Ana"],
            "attendee_ids.partner_id": [[7, "Ana"]],
            "attendee_ids.state": ["accepted"]
        }));
        assert_eq!(is_organizer_alone(&solo), json!(false));
    }

    #[test]
    fn a_meeting_that_ends_before_it_starts_is_refused() {
        let backwards = record(json!({
            "name": "Retro",
            "start": "2026-03-04 10:00:00",
            "stop": "2026-03-04 09:00:00"
        }));
        let error = ends_after_it_starts(&backwards).expect_err("time does not run backwards");
        assert!(error.contains("Retro"), "the message names the meeting: {error}");

        let forwards = record(json!({
            "name": "Retro",
            "start": "2026-03-04 09:00:00",
            "stop": "2026-03-04 10:00:00"
        }));
        assert!(ends_after_it_starts(&forwards).is_ok());
        // a meeting of no length at all is a marker, not a mistake
        let instant = record(json!({
            "name": "Deploy",
            "start": "2026-03-04 09:00:00",
            "stop": "2026-03-04 09:00:00"
        }));
        assert!(ends_after_it_starts(&instant).is_ok());
    }

    #[test]
    fn an_attendee_is_named_after_their_contact_and_falls_back_to_the_email() {
        let named = record(json!({"partner_id.name": ["Ana"], "partner_id.email": ["ana@x.com"]}));
        assert_eq!(common_name(&named), json!("Ana"));
        let anonymous =
            record(json!({"partner_id.name": [""], "partner_id.email": ["ana@x.com"]}));
        assert_eq!(common_name(&anonymous), json!("ana@x.com"));
        assert_eq!(common_name(&Map::new()), Value::Null);
    }

    #[test]
    fn a_rule_the_expansion_cannot_use_is_refused_at_the_write() {
        let weekly = record(json!({"rrule_type": "weekly", "end_type": "count", "count": 3}));
        let error = recurrence_is_usable(&weekly).expect_err("no weekday was chosen");
        assert!(error.contains("at least one day"), "{error}");

        let usable = record(json!({
            "rrule_type": "weekly", "end_type": "count", "count": 3, "mon": true
        }));
        assert!(recurrence_is_usable(&usable).is_ok());
        assert_eq!(
            recurrence_name(&usable),
            json!("Every 1 Weeks on Monday for 3 events")
        );
        assert_eq!(
            recurrence_rrule(&usable),
            json!("FREQ=WEEKLY;INTERVAL=1;BYDAY=MO;COUNT=3")
        );
        // a half-written row has no name and no rule yet, and says so
        // with a null rather than with a lie
        assert_eq!(recurrence_name(&weekly), Value::Null);
        assert_eq!(recurrence_rrule(&weekly), Value::Null);
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in [
            "calendar.event",
            "calendar.attendee",
            "calendar.alarm",
            "calendar.recurrence",
            "calendar.event.type",
            "calendar.filters",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the contact keeps everything `base` gave it and gains its
        // meetings
        let partner = reg.get("res.partner").unwrap();
        assert!(partner.field("name").unwrap().required, "base's field survives");
        assert!(partner.field("meeting_ids").is_some());
        assert_eq!(partner.meta.table, "res_partner", "and the table is the same");

        // an attendee belongs to a meeting and to a person, and goes when
        // either of them does
        let attendee = reg.get("calendar.attendee").unwrap();
        assert_eq!(
            attendee.field("event_id").unwrap().ondelete,
            Some(OnDelete::Cascade)
        );
        // the rule survives the meeting it was created from
        let recurrence = reg.get("calendar.recurrence").unwrap();
        assert_eq!(
            recurrence.field("base_event_id").unwrap().ondelete,
            Some(OnDelete::SetNull)
        );
        // the lead time is a real column: the scheduler sorts by it
        assert!(reg
            .get("calendar.alarm")
            .unwrap()
            .field("duration_minutes")
            .unwrap()
            .stored);
    }
}
