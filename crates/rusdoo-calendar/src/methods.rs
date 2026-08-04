//! What a calendar lets somebody do: answer an invitation, invite one
//! more person, repeat a meeting, and call off a whole series.
//!
//! Odoo hangs most of this off `create` and `write`: setting `recurrency`
//! on an event makes the ORM build a `calendar.recurrence` and materialize
//! its occurrences behind the scenes. This ORM has no create/write
//! override, so the same work is a method somebody calls — which is what
//! the button in Odoo's own form ends up doing anyway, one layer down.
//! Where that changes what a caller has to do, the doc comment says so.

use crate::models::{first_id, MAX_RECURRENCE_YEARS_PARAM, NEEDS_ACTION};
use crate::rrule::{self, Rule, DEFAULT_MAX_RECURRENCE_YEARS};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use serde_json::{json, Map, Value};

/// How a series is edited: this occurrence, this one and the ones after
/// it, or the lot (`recurrence_update` on `calendar.event`).
const SELF_ONLY: &str = "self_only";
const FUTURE_EVENTS: &str = "future_events";
const ALL_EVENTS: &str = "all_events";

/// The fields an occurrence inherits from the meeting it repeats.
///
/// Odoo copies the base event wholesale (`copy_data`); this names them,
/// which is duller and says out loud that an occurrence does *not*
/// inherit the answers people gave to another one.
const COPIED_FIELDS: [&str; 10] = [
    "name",
    "description",
    "notes",
    "location",
    "allday",
    "show_as",
    "privacy",
    "user_id",
    "res_model",
    "res_id",
];

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "calendar.event",
        "action_join_meeting",
        Operation::Write,
        action_join_meeting,
    )?;
    methods.register(
        "calendar.event",
        "change_attendee_status",
        Operation::Write,
        change_attendee_status,
    )?;
    methods.register(
        "calendar.event",
        "action_set_recurrence",
        Operation::Write,
        action_set_recurrence,
    )?;
    methods.register(
        "calendar.event",
        "action_mass_archive",
        Operation::Write,
        action_mass_archive,
    )?;
    methods.register(
        "calendar.event",
        "action_mass_deletion",
        Operation::Unlink,
        action_mass_deletion,
    )?;
    // joining a call does not change the meeting: it opens a URL
    methods.register(
        "calendar.event",
        "action_join_video_call",
        Operation::Read,
        action_join_video_call,
    )?;
    methods.register(
        "calendar.recurrence",
        "apply_recurrence",
        Operation::Write,
        apply_recurrence,
    )?;
    methods.register(
        "calendar.recurrence",
        "set_rrule",
        Operation::Write,
        set_rrule,
    )?;
    for (name, func) in [
        ("do_accept", do_accept as rusdoo_orm::methods::MethodFn),
        ("do_decline", do_decline),
        ("do_tentative", do_tentative),
    ] {
        methods.register("calendar.attendee", name, Operation::Write, func)?;
    }
    methods.register(
        "calendar.filters",
        "unlink_from_partner_id",
        Operation::Unlink,
        unlink_from_partner_id,
    )?;
    methods.register(
        "res.partner",
        "get_attendee_detail",
        Operation::Read,
        get_attendee_detail,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------

fn only_one(ids: &[i64], complaint: &str) -> Result<i64, RusdooError> {
    match ids {
        [id] => Ok(*id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

/// An argument the client may send positionally or by name, which are
/// the two shapes `call_kw` carries.
fn wanted<'a>(
    rest: &'a [Value],
    kwargs: &'a Map<String, Value>,
    name: &str,
    position: usize,
) -> Option<&'a Value> {
    kwargs.get(name).or_else(|| rest.get(position))
}

fn ids_of(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

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

/// How far ahead a series with no end is materialized
/// (`calendar.max_recurrence_years`).
async fn max_recurrence_years(ctx: &MethodCtx<'_>) -> i64 {
    ctx.registry
        .param_or(
            ctx.pool,
            MAX_RECURRENCE_YEARS_PARAM,
            &DEFAULT_MAX_RECURRENCE_YEARS.to_string(),
        )
        .await
        .parse()
        .unwrap_or(DEFAULT_MAX_RECURRENCE_YEARS)
}

/// Say something on the meeting's thread, when there is a thread to say
/// it on.
///
/// `calendar.event` is a `mail.thread` in Odoo. Whether `mail` is
/// installed is not this module's business, so the message is posted when
/// the model is there and skipped when it is not — an accepted invitation
/// must not fail because a chatter is missing.
async fn note_on_event(
    ctx: &MethodCtx<'_>,
    event: i64,
    body: String,
) -> Result<(), RusdooError> {
    if ctx.registry.get("mail.message").is_none() {
        return Ok(());
    }
    ctx.registry
        .create_as(
            ctx.pool,
            ctx.uid,
            "mail.message",
            vec![
                ("model", json!("calendar.event")),
                ("res_id", json!(event)),
                ("body", json!(body)),
                ("message_type", json!("notification")),
                ("author_id", json!(ctx.uid)),
            ],
        )
        .await?;
    Ok(())
}

/// The partner behind the acting user — who "me" is on a calendar.
async fn my_partner(ctx: &MethodCtx<'_>) -> Result<Option<i64>, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, "res.users", &[ctx.uid], &["partner_id"])
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("partner_id"))
        .and_then(first_id))
}

// ---------------------------------------------------------------------
// calendar.event
// ---------------------------------------------------------------------

/// `action_join_meeting(partner_id)` — add somebody to a meeting.
///
/// Writing `partner_ids` is what creates the attendee row in Odoo, inside
/// `write`. Here the two are done together, because a partner invited
/// without an attendee row is somebody who is on the guest list and can
/// never answer.
fn action_join_meeting<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "invite people to one meeting at a time")?;
        let partner = wanted(&ctx.rest, kwargs, "partner_id", 0)
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                RusdooError::Validation("say who: pass partner_id with the contact".into())
            })?;
        let created = invite(&ctx, event, &[partner]).await?;
        Ok(json!(created))
    })
}

/// Put `partners` on a meeting's guest list, and give each of them a
/// place to answer from. Answers how many were actually added.
async fn invite(
    ctx: &MethodCtx<'_>,
    event: i64,
    partners: &[i64],
) -> Result<usize, RusdooError> {
    let row = one_row(ctx, "calendar.event", event, &["partner_ids"]).await?;
    let already: Vec<i64> = row
        .get("partner_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let missing: Vec<i64> = partners
        .iter()
        .copied()
        .filter(|partner| !already.contains(partner))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let links: Vec<Value> = missing.iter().map(|id| json!([4, id, 0])).collect();
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "calendar.event",
            &[event],
            vec![("partner_ids", Value::Array(links))],
        )
        .await?;
    for partner in &missing {
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "calendar.attendee",
                vec![("event_id", json!(event)), ("partner_id", json!(partner))],
            )
            .await?;
    }
    Ok(missing.len())
}

/// `change_attendee_status(status, recurrence_update_setting)` — the
/// Yes / No / Maybe buttons.
///
/// Whose answer it is comes from who is calling and never from the
/// arguments: a client that could name the attendee would be a client
/// that accepts invitations on somebody else's behalf.
fn change_attendee_status<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "answer one meeting at a time")?;
        let status = wanted(&ctx.rest, kwargs, "status", 0)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RusdooError::Validation(
                    "say which answer: accepted, declined or tentative".into(),
                )
            })?
            .to_string();
        if !["accepted", "declined", "tentative"].contains(&status.as_str()) {
            return Err(RusdooError::Validation(format!(
                "{status:?} is not an answer to an invitation"
            )));
        }
        let scope = wanted(&ctx.rest, kwargs, "recurrence_update_setting", 1)
            .and_then(Value::as_str)
            .unwrap_or(SELF_ONLY)
            .to_string();
        let events = events_in_scope(&ctx, event, &scope).await?;
        let Some(partner) = my_partner(&ctx).await? else {
            return Err(RusdooError::Validation(
                "the acting user has no contact, so there is nobody to answer for".into(),
            ));
        };
        let mine = attendees_of(&ctx, &events, Some(partner)).await?;
        if mine.is_empty() {
            return Err(RusdooError::Validation(
                "you are not invited to this meeting".into(),
            ));
        }
        answer(&ctx, &mine, &status).await?;
        Ok(json!(mine.len()))
    })
}

/// The events a `recurrence_update_setting` reaches, port of the branch
/// repeated in `change_attendee_status`, `action_mass_deletion` and
/// `action_mass_archive`.
async fn events_in_scope(
    ctx: &MethodCtx<'_>,
    event: i64,
    scope: &str,
) -> Result<Vec<i64>, RusdooError> {
    if scope == SELF_ONLY {
        return Ok(vec![event]);
    }
    let row = one_row(ctx, "calendar.event", event, &["recurrence_id", "start"]).await?;
    let Some(recurrence) = row.get("recurrence_id").and_then(first_id) else {
        // a meeting that repeats nothing has exactly one occurrence, and
        // asking for "all of them" is not an error
        return Ok(vec![event]);
    };
    let mut domain = json!([["recurrence_id", "=", recurrence]]);
    if scope == FUTURE_EVENTS {
        let start = row
            .get("start")
            .and_then(Value::as_str)
            .ok_or_else(|| RusdooError::Validation("the meeting has no start".into()))?;
        domain = json!([
            ["recurrence_id", "=", recurrence],
            ["start", ">=", start]
        ]);
    } else if scope != ALL_EVENTS {
        return Err(RusdooError::Validation(format!(
            "{scope:?} is not one of self_only, future_events or all_events"
        )));
    }
    ctx.registry
        .search(
            ctx.pool,
            "calendar.event",
            &parse_domain(&domain)?,
            &ctx.search_options(),
        )
        .await
}

/// The attendee rows of `events`, optionally only one partner's.
async fn attendees_of(
    ctx: &MethodCtx<'_>,
    events: &[i64],
    partner: Option<i64>,
) -> Result<Vec<i64>, RusdooError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let domain = match partner {
        Some(partner) => json!([
            ["event_id", "in", events],
            ["partner_id", "=", partner]
        ]),
        None => json!([["event_id", "in", events]]),
    };
    ctx.registry
        .search(
            ctx.pool,
            "calendar.attendee",
            &parse_domain(&domain)?,
            &SearchOptions::default(),
        )
        .await
}

/// Write an answer onto attendee rows, and say so on the meeting's
/// thread — port of `do_accept` / `do_decline`, which post a message and
/// `do_tentative`, which does not.
async fn answer(
    ctx: &MethodCtx<'_>,
    attendees: &[i64],
    status: &str,
) -> Result<(), RusdooError> {
    let told = match status {
        "accepted" => Some("has accepted the invitation"),
        "declined" => Some("has declined the invitation"),
        _ => None,
    };
    if let Some(told) = told {
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "calendar.attendee",
                attendees,
                &["event_id", "common_name"],
            )
            .await?;
        for row in rows {
            let Some(event) = row.get("event_id").and_then(first_id) else {
                continue;
            };
            let who = row
                .get("common_name")
                .and_then(Value::as_str)
                .unwrap_or("somebody");
            note_on_event(ctx, event, format!("{who} {told}")).await?;
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "calendar.attendee",
            attendees,
            vec![("state", json!(status))],
        )
        .await
}

/// `action_join_video_call` — open the meeting's call.
fn action_join_video_call<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "join one call at a time")?;
        let row = one_row(&ctx, "calendar.event", event, &["videocall_location"]).await?;
        let url = row
            .get("videocall_location")
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| {
                RusdooError::Validation("this meeting has no call to join".into())
            })?;
        Ok(json!({
            "type": "ir.actions.act_url",
            "url": url,
            "target": "new",
        }))
    })
}

/// `action_mass_archive(recurrence_update_setting)` — call off the
/// meetings without deleting them.
///
/// Archiving and not deleting is the default a calendar wants: the
/// invitations that went out still name these records, and somebody
/// looking for "why was that cancelled" needs to find something.
fn action_mass_archive<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "archive one series at a time")?;
        let scope = wanted(&ctx.rest, kwargs, "recurrence_update_setting", 0)
            .and_then(Value::as_str)
            .unwrap_or(SELF_ONLY)
            .to_string();
        let events = events_in_scope(&ctx, event, &scope).await?;
        if events.is_empty() {
            return Ok(json!(0));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "calendar.event",
                &events,
                vec![("active", json!(false))],
            )
            .await?;
        Ok(json!(events.len()))
    })
}

/// `action_mass_deletion(recurrence_update_setting)` — delete the series.
fn action_mass_deletion<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "delete one series at a time")?;
        let scope = wanted(&ctx.rest, kwargs, "recurrence_update_setting", 0)
            .and_then(Value::as_str)
            .unwrap_or(SELF_ONLY)
            .to_string();
        let row = one_row(&ctx, "calendar.event", event, &["recurrence_id"]).await?;
        let recurrence = row.get("recurrence_id").and_then(first_id);
        let events = events_in_scope(&ctx, event, &scope).await?;
        ctx.registry
            .unlink_as(ctx.pool, ctx.uid, "calendar.event", &events)
            .await?;
        // the rule goes with the last of its occurrences: a recurrence
        // with nothing in it is a row nobody can reach and nothing will
        // ever clean up
        if scope == ALL_EVENTS {
            if let Some(recurrence) = recurrence {
                ctx.registry
                    .unlink_as(ctx.pool, ctx.uid, "calendar.recurrence", &[recurrence])
                    .await?;
            }
        }
        Ok(json!(events.len()))
    })
}

// ---------------------------------------------------------------------
// Recurrence
// ---------------------------------------------------------------------

/// `action_set_recurrence(**rule)` — make this meeting repeat.
///
/// Port of `_apply_recurrence_values`. Odoo runs it from `write` when
/// `recurrency` is set; here it is the call, because the ORM has no write
/// override to hang it off — and the parameters have to arrive together
/// anyway, since a half-applied rule is a rule that expands into the
/// wrong days.
///
/// The weekday the meeting already falls on is filled in from its start
/// date when the caller did not say (`_get_recurrence_params`): a weekly
/// rule with no day chosen would otherwise be refused, and the day the
/// user meant is the one they are already looking at.
fn action_set_recurrence<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let event = only_one(&ctx.ids, "set the rule on one meeting at a time")?;
        let row = one_row(
            &ctx,
            "calendar.event",
            event,
            &["start", "recurrence_id", "user_id"],
        )
        .await?;
        let start = row
            .get("start")
            .and_then(Value::as_str)
            .ok_or_else(|| RusdooError::Validation("the meeting has no start".into()))?;
        let start = rrule::parse_datetime(start).map_err(RusdooError::Validation)?;

        let mut values: Map<String, Value> = kwargs.clone();
        // the wizard passes the whole rule as one object when it has one
        if let Some(Value::Object(given)) = wanted(&ctx.rest, kwargs, "rule", 0).cloned() {
            values.extend(given);
        }
        values.remove("rule");
        for (name, value) in recurrence_params_from(start) {
            values.entry(name.to_string()).or_insert(value);
        }
        // the rule is checked before anything is written: a recurrence
        // created and then refused would leave the meeting pointing at a
        // rule that expands into nothing
        Rule::from_record(&values)
            .and_then(|rule| rule.check())
            .map_err(RusdooError::Validation)?;
        let written: Vec<(&str, Value)> = values
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();

        let recurrence = match row.get("recurrence_id").and_then(first_id) {
            Some(existing) => {
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "calendar.recurrence",
                        &[existing],
                        written,
                    )
                    .await?;
                existing
            }
            None => {
                let mut creation = written;
                creation.push(("base_event_id", json!(event)));
                ctx.registry
                    .create_as(ctx.pool, ctx.uid, "calendar.recurrence", creation)
                    .await?
            }
        };
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "calendar.event",
                &[event],
                vec![
                    ("recurrency", json!(true)),
                    ("follow_recurrence", json!(true)),
                    ("recurrence_id", json!(recurrence)),
                ],
            )
            .await?;
        let events = materialize(&ctx, recurrence).await?;
        Ok(json!({"recurrence_id": recurrence, "event_ids": events}))
    })
}

/// `_get_recurrence_params` — the pieces of the rule the meeting's own
/// start date already answers.
fn recurrence_params_from(start: chrono::NaiveDateTime) -> Vec<(&'static str, Value)> {
    use chrono::Datelike;
    let weekday = start.weekday().num_days_from_monday() as usize;
    // `get_weekday_occurence`: the fourth and fifth are both "the last",
    // because a month with a fifth Monday is the exception
    let occurrence = start.day().div_ceil(7);
    let byday = if (4..=5).contains(&occurrence) {
        -1
    } else {
        i64::from(occurrence)
    };
    vec![
        (rrule::WEEKDAY_FIELDS[weekday], json!(true)),
        ("weekday", json!(rrule::WEEKDAY_CODES[weekday])),
        ("byday", json!(byday.to_string())),
        ("day", json!(start.day())),
    ]
}

/// `apply_recurrence` — create the occurrences the rule calls for, and
/// detach the ones it no longer does.
fn apply_recurrence<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let recurrence = only_one(&ctx.ids, "expand one rule at a time")?;
        let events = materialize(&ctx, recurrence).await?;
        Ok(json!(events))
    })
}

/// Bring a recurrence's events in line with its rule, port of
/// `_apply_recurrence` and `_reconcile_events`.
///
/// The occurrences that already exist at the right moment are kept — with
/// whatever answers people gave them — and only the missing ones are
/// created. An event the rule no longer calls for is *detached* rather
/// than deleted: somebody moved it on purpose, and a rule change is not
/// permission to throw that away.
async fn materialize(ctx: &MethodCtx<'_>, recurrence: i64) -> Result<Vec<i64>, RusdooError> {
    let mut wanted_fields: Vec<&str> = vec![
        "base_event_id",
        "calendar_event_ids",
        "rrule_type",
        "interval",
        "end_type",
        "count",
        "until",
        "month_by",
        "day",
        "weekday",
        "byday",
    ];
    wanted_fields.extend(rrule::WEEKDAY_FIELDS);
    let rule_row = one_row(ctx, "calendar.recurrence", recurrence, &wanted_fields).await?;
    let rule = Rule::from_record(&rule_row).map_err(RusdooError::Validation)?;

    let base = rule_row
        .get("base_event_id")
        .and_then(first_id)
        .ok_or_else(|| {
            RusdooError::Validation(
                "the rule has no meeting to repeat: give it a base_event_id".into(),
            )
        })?;
    let mut base_fields: Vec<&str> = vec!["start", "stop", "partner_ids"];
    base_fields.extend(COPIED_FIELDS);
    let base_row = one_row(ctx, "calendar.event", base, &base_fields).await?;
    let start = base_row
        .get("start")
        .and_then(Value::as_str)
        .ok_or_else(|| RusdooError::Validation("the meeting has no start".into()))?;
    let stop = base_row
        .get("stop")
        .and_then(Value::as_str)
        .ok_or_else(|| RusdooError::Validation("the meeting has no end".into()))?;
    let start = rrule::parse_datetime(start).map_err(RusdooError::Validation)?;
    let stop = rrule::parse_datetime(stop).map_err(RusdooError::Validation)?;
    let length = stop - start;

    let years = max_recurrence_years(ctx).await;
    let occurrences = rule
        .occurrences(start, years)
        .map_err(RusdooError::Validation)?;

    // what the recurrence already holds, and when each of them happens
    let existing: Vec<i64> = rule_row
        .get("calendar_event_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let mut when: Vec<(i64, chrono::NaiveDateTime)> = Vec::new();
    if !existing.is_empty() {
        for row in ctx
            .registry
            .read(ctx.pool, "calendar.event", &existing, &["start"])
            .await?
        {
            let (Some(id), Some(moment)) = (
                row.get("id").and_then(Value::as_i64),
                row.get("start")
                    .and_then(Value::as_str)
                    .and_then(|text| rrule::parse_datetime(text).ok()),
            ) else {
                continue;
            };
            when.push((id, moment));
        }
    }

    let mut kept: Vec<i64> = Vec::new();
    let mut created: Vec<i64> = Vec::new();
    let partners = ids_of(base_row.get("partner_ids"));
    for moment in &occurrences {
        match when.iter().find(|(_, existing)| existing == moment) {
            Some((id, _)) => kept.push(*id),
            None if *moment == start => kept.push(base),
            None => {
                let id = create_occurrence(
                    ctx,
                    &base_row,
                    recurrence,
                    *moment,
                    *moment + length,
                    &partners,
                )
                .await?;
                created.push(id);
            }
        }
    }
    // the base event belongs to its own series even when the rule was
    // trimmed past it: losing it would leave the recurrence pointing at a
    // meeting that is no longer part of it
    if !kept.contains(&base) {
        kept.push(base);
    }
    let mut all: Vec<i64> = kept.clone();
    all.extend(created.iter().copied());

    let orphans: Vec<i64> = when
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !all.contains(id))
        .collect();
    if !orphans.is_empty() {
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "calendar.event",
                &orphans,
                vec![
                    ("recurrence_id", Value::Null),
                    // it still repeats, it just no longer follows *this*
                    // rule — which is what Odoo's `_detach_events` says
                    ("recurrency", json!(true)),
                    ("follow_recurrence", json!(false)),
                ],
            )
            .await?;
    }
    // whatever was created has to point back, and the base event too
    if !all.is_empty() {
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "calendar.event",
                &all,
                vec![
                    ("recurrence_id", json!(recurrence)),
                    ("recurrency", json!(true)),
                ],
            )
            .await?;
    }
    all.sort_unstable();
    Ok(all)
}

/// One occurrence of a repeating meeting.
///
/// It copies what the meeting *is* and none of what happened to it: the
/// answers, the reminders somebody dismissed and the notes belong to the
/// occurrence they were given on. The guest list is copied, because a
/// series everybody has to be re-invited to every week is not a series.
async fn create_occurrence(
    ctx: &MethodCtx<'_>,
    base: &Map<String, Value>,
    recurrence: i64,
    start: chrono::NaiveDateTime,
    stop: chrono::NaiveDateTime,
    partners: &[i64],
) -> Result<i64, RusdooError> {
    let mut values: Vec<(&str, Value)> = vec![
        ("start", json!(rrule::format_datetime(start))),
        ("stop", json!(rrule::format_datetime(stop))),
        ("recurrence_id", json!(recurrence)),
        ("recurrency", json!(true)),
        ("follow_recurrence", json!(true)),
    ];
    for name in COPIED_FIELDS {
        let Some(value) = base.get(name) else {
            continue;
        };
        // a many2one comes back as `[id, name]`; what a create takes is
        // the id
        let value = match name {
            "user_id" => match first_id(value) {
                Some(id) => json!(id),
                None => continue,
            },
            _ => value.clone(),
        };
        if value.is_null() {
            continue;
        }
        values.push((name, value));
    }
    if !partners.is_empty() {
        let links: Vec<Value> = partners.iter().map(|id| json!([4, id, 0])).collect();
        values.push(("partner_ids", Value::Array(links)));
    }
    let event = ctx
        .registry
        .create_as(ctx.pool, ctx.uid, "calendar.event", values)
        .await?;
    for partner in partners {
        ctx.registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "calendar.attendee",
                vec![("event_id", json!(event)), ("partner_id", json!(partner))],
            )
            .await?;
    }
    Ok(event)
}

/// `set_rrule(rrule)` — spell the rule out from an iCalendar string.
///
/// Port of `_inverse_rrule`. It exists for the same reason Odoo's does:
/// an event that came from another calendar carries its rule as one
/// string, and there has to be a way in that is not "fill in fourteen
/// fields by hand".
fn set_rrule<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let recurrence = only_one(&ctx.ids, "set the rule of one recurrence at a time")?;
        let text = wanted(&ctx.rest, kwargs, "rrule", 0)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RusdooError::Validation("say which rule: pass rrule with the RRULE line".into())
            })?;
        let rule = Rule::from_rrule(text).map_err(RusdooError::Validation)?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "calendar.recurrence",
                &[recurrence],
                rule.to_values(),
            )
            .await?;
        Ok(json!(rule.name()))
    })
}

// ---------------------------------------------------------------------
// calendar.attendee
// ---------------------------------------------------------------------

fn answer_method<'a>(ctx: MethodCtx<'a>, status: &'static str) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "answer at least one invitation".into(),
            ));
        }
        let ids = ctx.ids.clone();
        answer(&ctx, &ids, status).await?;
        Ok(json!(true))
    })
}

/// `do_accept` — yes.
fn do_accept<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    answer_method(ctx, "accepted")
}

/// `do_decline` — no.
fn do_decline<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    answer_method(ctx, "declined")
}

/// `do_tentative` — maybe. It says nothing on the thread, like Odoo's:
/// "maybe" is not news.
fn do_tentative<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    answer_method(ctx, "tentative")
}

// ---------------------------------------------------------------------
// calendar.filters
// ---------------------------------------------------------------------

/// `unlink_from_partner_id(partner_id)` — forget a contact's calendar,
/// for everybody who had it ticked.
fn unlink_from_partner_id<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let partner = wanted(&ctx.rest, kwargs, "partner_id", 0)
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                RusdooError::Validation("say whose: pass partner_id with the contact".into())
            })?;
        let ids = ctx
            .registry
            .search(
                ctx.pool,
                "calendar.filters",
                &parse_domain(&json!([["partner_id", "=", partner]]))?,
                &SearchOptions::default(),
            )
            .await?;
        if ids.is_empty() {
            return Ok(json!(0));
        }
        let gone = ctx
            .registry
            .unlink_as(ctx.pool, ctx.uid, "calendar.filters", &ids)
            .await?;
        Ok(json!(gone))
    })
}

// ---------------------------------------------------------------------
// res.partner
// ---------------------------------------------------------------------

/// `get_attendee_detail(meeting_ids)` — who is coming, and what they
/// said, for the avatars a calendar draws on a meeting.
///
/// Port of `res_partner.get_attendee_detail`. One call for a screenful of
/// meetings, which is the point: the alternative is a request per avatar.
fn get_attendee_detail<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let meetings = ids_of(wanted(&ctx.rest, kwargs, "meeting_ids", 0));
        if meetings.is_empty() || ctx.ids.is_empty() {
            return Ok(json!([]));
        }
        let attendees = attendees_of(&ctx, &meetings, None).await?;
        if attendees.is_empty() {
            return Ok(json!([]));
        }
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "calendar.attendee",
                &attendees,
                &["event_id", "partner_id", "state", "common_name"],
            )
            .await?;
        // which contact organizes each meeting, so an avatar can be
        // marked as the organizer's without a lookup per row
        let organizers = ctx
            .registry
            .read(ctx.pool, "calendar.event", &meetings, &["partner_id"])
            .await?;
        let details: Vec<Value> = rows
            .iter()
            .filter_map(|row| {
                let partner = row.get("partner_id").and_then(first_id)?;
                // only the contacts the caller asked about
                if !ctx.ids.contains(&partner) {
                    return None;
                }
                let event = row.get("event_id").and_then(first_id)?;
                let organizer = organizers
                    .iter()
                    .find(|meeting| meeting.get("id").and_then(Value::as_i64) == Some(event))
                    .and_then(|meeting| meeting.get("partner_id"))
                    .and_then(first_id);
                Some(json!({
                    "id": partner,
                    "name": row.get("common_name").cloned().unwrap_or(Value::Null),
                    "status": row.get("state").cloned().unwrap_or(json!(NEEDS_ACTION)),
                    "event_id": event,
                    "attendee_id": row.get("id").cloned().unwrap_or(Value::Null),
                    "is_organizer": i32::from(organizer == Some(partner)),
                }))
            })
            .collect();
        Ok(json!(details))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrule::parse_datetime;

    #[test]
    fn the_rule_takes_the_weekday_the_meeting_already_falls_on() {
        // 2026-03-11 is the second Wednesday of March
        let start = parse_datetime("2026-03-11 09:00:00").unwrap();
        let params = recurrence_params_from(start);
        assert!(params.contains(&("wed", json!(true))));
        assert!(params.contains(&("weekday", json!("WED"))));
        assert!(params.contains(&("byday", json!("2"))));
        assert!(params.contains(&("day", json!(11))));
    }

    #[test]
    fn the_fourth_and_the_fifth_of_a_month_both_mean_the_last() {
        // Odoo's `get_weekday_occurence`: a month with a fifth Monday is
        // the exception, so "the fourth" and "the fifth" both become -1 —
        // otherwise a rule would skip the months that have only four
        for day in ["2026-03-25", "2026-03-30"] {
            let start = parse_datetime(&format!("{day} 09:00:00")).unwrap();
            let params = recurrence_params_from(start);
            assert!(
                params.contains(&("byday", json!("-1"))),
                "{day} falls in the last week of its month"
            );
        }
        let start = parse_datetime("2026-03-18 09:00:00").unwrap();
        assert!(recurrence_params_from(start).contains(&("byday", json!("3"))));
    }

    #[test]
    fn every_method_the_screens_call_is_registered() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("calendar.event"),
            vec![
                "action_join_meeting",
                "action_join_video_call",
                "action_mass_archive",
                "action_mass_deletion",
                "action_set_recurrence",
                "change_attendee_status",
            ]
        );
        assert_eq!(
            methods.names_for("calendar.attendee"),
            vec!["do_accept", "do_decline", "do_tentative"]
        );
        assert_eq!(
            methods.names_for("calendar.recurrence"),
            vec!["apply_recurrence", "set_rrule"]
        );
        // deleting a series is checked as a delete, not as a write
        assert_eq!(
            methods
                .get("calendar.event", "action_mass_deletion")
                .unwrap()
                .operation,
            Operation::Unlink
        );
        // and joining a call touches nothing
        assert_eq!(
            methods
                .get("calendar.event", "action_join_video_call")
                .unwrap()
                .operation,
            Operation::Read
        );
    }
}
