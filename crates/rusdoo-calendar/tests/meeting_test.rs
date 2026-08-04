//! Meetings against the database: who was invited, what they answered,
//! and what a repeating rule turns into.
//!
//! The rule arithmetic has its own unit tests inside the crate. What this
//! covers is the half they cannot — that the models store what those
//! functions need, and that the methods a screen calls answer over real
//! rows.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 3] = ["base", "mail", "calendar"];

async fn ask(
    case: &TransactionCase,
    model: &str,
    ids: &[i64],
    method: &str,
    kwargs: Value,
) -> Result<Value, String> {
    let methods = case.methods();
    let entry = methods
        .get(model, method)
        .unwrap_or_else(|| panic!("{model}.{method} is not registered"));
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, model, ids.to_vec());
    let kwargs: Map<String, Value> = kwargs.as_object().cloned().unwrap_or_default();
    entry
        .call(ctx, &[], &kwargs)
        .await
        .map_err(|error| error.to_string())
}

async fn a_partner(case: &TransactionCase, name: &str) -> i64 {
    case.models()
        .create(&case.pool(), "res.partner", vec![("name", json!(name))])
        .await
        .expect("the partner saves")
}

#[tokio::test]
async fn a_meeting_carries_the_people_invited_to_it_live() {
    let Some(case) = TransactionCase::open("calendar_meeting", &MODULES).await else {
        return;
    };
    let ana = a_partner(&case, "Ana").await;
    let bruno = a_partner(&case, "Bruno").await;

    let meeting = case
        .models()
        .create(
            &case.pool(),
            "calendar.event",
            vec![
                ("name", json!("Revisão do trimestre")),
                ("start", json!("2026-08-10 14:00:00")),
                ("stop", json!("2026-08-10 15:30:00")),
            ],
        )
        .await
        .expect("the meeting saves");

    // inviting is a method here and a side effect of writing
    // `partner_ids` in Odoo — see the crate docs. Either way the guest
    // list and the place to answer from are made together, because a
    // guest who cannot answer is not an invitation.
    for partner in [ana, bruno] {
        ask(
            &case,
            "calendar.event",
            &[meeting],
            "action_join_meeting",
            json!({"partner_id": partner}),
        )
        .await
        .expect("the invitation goes out");
    }

    // one attendee record per person, which is what holds the answer a
    // link row could not
    let attendees = case
        .models()
        .search(
            &case.pool(),
            "calendar.attendee",
            &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", meeting]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the attendees are searchable");
    assert_eq!(attendees.len(), 2, "um convidado por pessoa: {attendees:?}");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "calendar.attendee",
            &attendees,
            &["partner_id", "state"],
        )
        .await
        .expect("the attendees read");
    for row in &rows {
        assert_eq!(
            row["state"], json!("needsAction"),
            "ninguém respondeu ainda: {row:?}"
        );
    }

    // the meeting knows how long it is, without anybody writing it down
    let rows = case
        .models()
        .read(&case.pool(), "calendar.event", &[meeting], &["duration"])
        .await
        .expect("the meeting reads");
    assert_eq!(rows[0]["duration"], json!(1.5), "{:?}", rows[0]);

    case.close().await;
}

#[tokio::test]
async fn an_invitation_is_answered_and_the_answer_is_kept_live() {
    let Some(case) = TransactionCase::open("calendar_answer", &MODULES).await else {
        return;
    };
    let ana = a_partner(&case, "Ana").await;
    let meeting = case
        .models()
        .create(
            &case.pool(),
            "calendar.event",
            vec![
                ("name", json!("Kickoff")),
                ("start", json!("2026-08-11 09:00:00")),
                ("stop", json!("2026-08-11 10:00:00")),
            ],
        )
        .await
        .expect("the meeting saves");
    ask(
        &case,
        "calendar.event",
        &[meeting],
        "action_join_meeting",
        json!({"partner_id": ana}),
    )
    .await
    .expect("the invitation goes out");
    let attendee = case
        .models()
        .search(
            &case.pool(),
            "calendar.attendee",
            &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", meeting]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the attendee is there")[0];

    ask(&case, "calendar.attendee", &[attendee], "do_accept", json!({}))
        .await
        .expect("accepting works");
    let rows = case
        .models()
        .read(&case.pool(), "calendar.attendee", &[attendee], &["state"])
        .await
        .unwrap();
    assert_eq!(rows[0]["state"], json!("accepted"), "{:?}", rows[0]);

    // and somebody can change their mind
    ask(&case, "calendar.attendee", &[attendee], "do_decline", json!({}))
        .await
        .expect("declining works");
    let rows = case
        .models()
        .read(&case.pool(), "calendar.attendee", &[attendee], &["state"])
        .await
        .unwrap();
    assert_eq!(rows[0]["state"], json!("declined"), "{:?}", rows[0]);

    case.close().await;
}

#[tokio::test]
async fn a_repeating_meeting_becomes_the_meetings_it_repeats_into_live() {
    let Some(case) = TransactionCase::open("calendar_recurrence", &MODULES).await else {
        return;
    };
    let meeting = case
        .models()
        .create(
            &case.pool(),
            "calendar.event",
            vec![
                ("name", json!("Daily")),
                ("start", json!("2026-08-10 09:00:00")),
                ("stop", json!("2026-08-10 09:15:00")),
            ],
        )
        .await
        .expect("the meeting saves");

    // every weekday, five times — the rule a screen sets
    ask(
        &case,
        "calendar.event",
        &[meeting],
        "action_set_recurrence",
        json!({"rrule_type": "daily", "interval": 1, "end_type": "count", "count": 5}),
    )
    .await
    .expect("the rule is set");

    let events = case
        .models()
        .search(
            &case.pool(),
            "calendar.event",
            &rusdoo_orm::domain::parse_domain(&json!([["name", "=", "Daily"]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the occurrences are searchable");
    assert_eq!(events.len(), 5, "cinco ocorrências: {events:?}");

    // each occurrence is an ordinary meeting pointing back at the rule,
    // which is what lets somebody move one without moving the rest
    let rows = case
        .models()
        .read(
            &case.pool(),
            "calendar.event",
            &events,
            &["start", "recurrence_id"],
        )
        .await
        .expect("the occurrences read");
    let mut starts: Vec<&str> = rows
        .iter()
        .map(|row| row["start"].as_str().unwrap_or_default())
        .collect();
    starts.sort_unstable();
    assert_eq!(starts[0], "2026-08-10 09:00:00", "{starts:?}");
    assert_eq!(starts[4], "2026-08-14 09:00:00", "{starts:?}");
    for row in &rows {
        assert!(
            row["recurrence_id"][0].as_i64().is_some(),
            "uma ocorrência sem regra: {row:?}"
        );
    }

    case.close().await;
}

#[tokio::test]
async fn a_meeting_that_ends_before_it_starts_is_refused_live() {
    let Some(case) = TransactionCase::open("calendar_backwards", &MODULES).await else {
        return;
    };
    let refused = case
        .models()
        .create(
            &case.pool(),
            "calendar.event",
            vec![
                ("name", json!("De trás para a frente")),
                ("start", json!("2026-08-10 15:00:00")),
                ("stop", json!("2026-08-10 14:00:00")),
            ],
        )
        .await;
    assert!(refused.is_err(), "uma reunião invertida foi aceita");

    case.close().await;
}

/// What Odoo's own client does: it writes `partner_ids` onto the form
/// and expects the guest list to be there afterwards. It calls no method
/// — it does not know there is one.
#[tokio::test]
async fn writing_the_guest_list_seats_the_guests_live() {
    let Some(case) = TransactionCase::open("calendar_hook", &MODULES).await else {
        return;
    };
    let ana = a_partner(&case, "Ana").await;
    let bruno = a_partner(&case, "Bruno").await;

    // the create: the field is written with the record, as a form does
    let meeting = case
        .models()
        .create(
            &case.pool(),
            "calendar.event",
            vec![
                ("name", json!("Planejamento")),
                ("start", json!("2026-08-12 10:00:00")),
                ("stop", json!("2026-08-12 11:00:00")),
                ("partner_ids", json!([[6, 0, [ana, bruno]]])),
            ],
        )
        .await
        .expect("the meeting saves");

    let attendees = case
        .models()
        .search(
            &case.pool(),
            "calendar.attendee",
            &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", meeting]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the attendees are searchable");
    assert_eq!(
        attendees.len(),
        2,
        "escrever a lista não deu lugar a ninguém: {attendees:?}"
    );

    // the write: a third guest added later gets a place too
    let carla = a_partner(&case, "Carla").await;
    case.models()
        .write(
            &case.pool(),
            "calendar.event",
            &[meeting],
            vec![("partner_ids", json!([[4, carla, 0]]))],
        )
        .await
        .expect("the third guest saves");

    let attendees = case
        .models()
        .search(
            &case.pool(),
            "calendar.attendee",
            &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", meeting]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(attendees.len(), 3, "{attendees:?}");

    // and a write that touches nothing else does not seat anybody twice
    case.models()
        .write(
            &case.pool(),
            "calendar.event",
            &[meeting],
            vec![("name", json!("Planejamento anual"))],
        )
        .await
        .expect("the rename saves");
    let attendees = case
        .models()
        .search(
            &case.pool(),
            "calendar.attendee",
            &rusdoo_orm::domain::parse_domain(&json!([["event_id", "=", meeting]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(attendees.len(), 3, "convidou de novo: {attendees:?}");

    case.close().await;
}
