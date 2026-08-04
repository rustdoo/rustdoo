//! The working schedule against the database: the four questions every
//! planning module in Odoo asks it, asked through the ORM.
//!
//! The arithmetic has its own unit tests inside the crate. What this file
//! covers is the part they cannot: that the models store what the
//! functions need, that a schedule loads its periods and its time off,
//! and that the methods answer over a real row.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 2] = ["base", "resource"];

/// A five-day week of 8am–noon and 1pm–5pm, the schedule Odoo installs.
const WEEK: [(&str, f64, f64); 10] = [
    ("0", 8.0, 12.0),
    ("0", 13.0, 17.0),
    ("1", 8.0, 12.0),
    ("1", 13.0, 17.0),
    ("2", 8.0, 12.0),
    ("2", 13.0, 17.0),
    ("3", 8.0, 12.0),
    ("3", 13.0, 17.0),
    ("4", 8.0, 12.0),
    ("4", 13.0, 17.0),
];

async fn a_schedule(case: &TransactionCase) -> i64 {
    let periods: Vec<Value> = WEEK
        .iter()
        .map(|(day, from, to)| {
            json!([0, 0, {
                "name": format!("dia {day} {from}-{to}"),
                "dayofweek": day,
                "hour_from": from,
                "hour_to": to,
            }])
        })
        .collect();
    case.models()
        .create(
            &case.pool(),
            "resource.calendar",
            vec![
                ("name", json!("Semana padrão")),
                ("attendance_ids", json!(periods)),
            ],
        )
        .await
        .expect("the schedule saves with its periods")
}

async fn ask(
    case: &TransactionCase,
    model: &str,
    ids: &[i64],
    method: &str,
    kwargs: Value,
) -> Value {
    let methods = case.methods();
    let entry = methods
        .get(model, method)
        .unwrap_or_else(|| panic!("{model}.{method} is not registered"));
    let registry = case.registry();
    let pool = case.pool();
    let ctx = MethodCtx::new(registry, &pool, 1, model, ids.to_vec());
    let kwargs: Map<String, Value> = kwargs.as_object().cloned().unwrap_or_default();
    entry
        .call(ctx, &[], &kwargs)
        .await
        .unwrap_or_else(|error| panic!("{model}.{method}: {error}"))
}

#[tokio::test]
async fn a_schedule_answers_how_much_work_fits_between_two_moments_live() {
    let Some(case) = TransactionCase::open("resource_hours", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    // a Monday, eight in the morning, to the Friday at five: one week
    let hours = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "get_work_hours_count",
        json!({"start_dt": "2026-08-03 08:00:00", "end_dt": "2026-08-07 17:00:00"}),
    )
    .await;
    assert_eq!(hours, json!(40.0), "cinco dias de oito horas");

    // one day of it
    let hours = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "get_work_hours_count",
        json!({"start_dt": "2026-08-03 00:00:00", "end_dt": "2026-08-03 23:59:59"}),
    )
    .await;
    assert_eq!(hours, json!(8.0));

    // and a Saturday is nothing at all
    let hours = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "get_work_hours_count",
        json!({"start_dt": "2026-08-08 00:00:00", "end_dt": "2026-08-09 23:59:59"}),
    )
    .await;
    assert_eq!(hours, json!(0.0));

    case.close().await;
}

#[tokio::test]
async fn time_off_comes_out_of_the_hours_live() {
    let Some(case) = TransactionCase::open("resource_leaves", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    // a public holiday on the Tuesday
    case.models()
        .create(
            &case.pool(),
            "resource.calendar.leaves",
            vec![
                ("name", json!("Feriado")),
                ("calendar_id", json!(schedule)),
                ("date_from", json!("2026-08-04 00:00:00")),
                ("date_to", json!("2026-08-04 23:59:59")),
            ],
        )
        .await
        .expect("the day off saves");

    let hours = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "get_work_hours_count",
        json!({"start_dt": "2026-08-03 08:00:00", "end_dt": "2026-08-07 17:00:00",
               "compute_leaves": true}),
    )
    .await;
    assert_eq!(hours, json!(32.0), "a semana menos o feriado");

    // and without asking for them, the schedule answers about itself
    let hours = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "get_work_hours_count",
        json!({"start_dt": "2026-08-03 08:00:00", "end_dt": "2026-08-07 17:00:00",
               "compute_leaves": false}),
    )
    .await;
    assert_eq!(hours, json!(40.0));

    case.close().await;
}

#[tokio::test]
async fn planning_says_when_the_work_will_be_done_live() {
    let Some(case) = TransactionCase::open("resource_plan", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    // twelve hours from Monday morning is Tuesday at noon: eight on the
    // Monday, four on the Tuesday morning
    let done = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "plan_hours",
        json!({"hours": 12.0, "day_dt": "2026-08-03 08:00:00"}),
    )
    .await;
    assert_eq!(done, json!("2026-08-04 12:00:00"));

    // work that starts on a Friday afternoon runs into the next week,
    // over a weekend nobody works
    let done = ask(
        &case,
        "resource.calendar",
        &[schedule],
        "plan_hours",
        json!({"hours": 8.0, "day_dt": "2026-08-07 13:00:00"}),
    )
    .await;
    assert_eq!(done, json!("2026-08-10 12:00:00"));

    case.close().await;
}

#[tokio::test]
async fn the_schedule_reads_its_own_totals_live() {
    let Some(case) = TransactionCase::open("resource_totals", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    let rows = case
        .models()
        .read(
            &case.pool(),
            "resource.calendar",
            &[schedule],
            &["hours_per_day", "hours_per_week", "is_fulltime", "work_time_rate"],
        )
        .await
        .expect("the schedule reads");
    let row = &rows[0];
    assert_eq!(row["hours_per_day"], json!(8.0), "{row:?}");
    assert_eq!(row["hours_per_week"], json!(40.0), "{row:?}");
    assert_eq!(row["is_fulltime"], json!(true), "{row:?}");
    assert_eq!(row["work_time_rate"], json!(100.0), "{row:?}");

    case.close().await;
}

#[tokio::test]
async fn a_resource_works_by_the_schedule_it_points_at_live() {
    let Some(case) = TransactionCase::open("resource_resource", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    let developer = case
        .models()
        .create(
            &case.pool(),
            "resource.resource",
            vec![
                ("name", json!("Ana")),
                ("calendar_id", json!(schedule)),
                ("resource_type", json!("user")),
            ],
        )
        .await
        .expect("the resource saves");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "resource.resource",
            &[developer],
            &["name", "calendar_id", "tz"],
        )
        .await
        .expect("the resource reads");
    assert_eq!(rows[0]["calendar_id"][0], json!(schedule), "{:?}", rows[0]);

    // a resource with no schedule is not an error but a statement:
    // fully flexible, in the words of the field's own help text
    // (`resource/models/resource_resource.py`)
    let flexible = case
        .models()
        .create(
            &case.pool(),
            "resource.resource",
            vec![("name", json!("Sem agenda"))],
        )
        .await
        .expect("a fully flexible resource is a resource");
    let rows = case
        .models()
        .read(&case.pool(), "resource.resource", &[flexible], &["calendar_id"])
        .await
        .expect("the resource reads");
    assert_eq!(rows[0]["calendar_id"], json!(null), "{:?}", rows[0]);

    case.close().await;
}

#[tokio::test]
async fn a_period_that_ends_before_it_starts_is_refused_live() {
    let Some(case) = TransactionCase::open("resource_period", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    let refused = case
        .models()
        .create(
            &case.pool(),
            "resource.calendar.attendance",
            vec![
                ("name", json!("de trás para a frente")),
                ("calendar_id", json!(schedule)),
                ("dayofweek", json!("0")),
                ("hour_from", json!(17.0)),
                ("hour_to", json!(9.0)),
            ],
        )
        .await;
    assert!(refused.is_err(), "um período invertido foi aceito");

    case.close().await;
}

#[tokio::test]
async fn a_new_resource_works_the_companys_hours_live() {
    let Some(case) = TransactionCase::open("resource_default", &MODULES).await else {
        return;
    };
    let schedule = a_schedule(&case).await;

    // the company keeps a default schedule, and the acting user belongs
    // to that company — which is the whole of Odoo's
    // `default=lambda self: self.env.company.resource_calendar_id`
    let company = case
        .models()
        .create(
            &case.pool(),
            "res.company",
            vec![
                ("name", json!("Fábrica")),
                ("resource_calendar_id", json!(schedule)),
            ],
        )
        .await
        .expect("the company saves");
    case.models()
        .write(
            &case.pool(),
            "res.users",
            &[1],
            vec![("company_id", json!(company))],
        )
        .await
        .expect("the superuser joins the company");

    let hired = case
        .models()
        .create(
            &case.pool(),
            "resource.resource",
            vec![("name", json!("Bruno"))],
        )
        .await
        .expect("the resource saves");
    let rows = case
        .models()
        .read(&case.pool(), "resource.resource", &[hired], &["calendar_id"])
        .await
        .expect("the resource reads");
    assert_eq!(rows[0]["calendar_id"][0], json!(schedule), "{:?}", rows[0]);

    case.close().await;
}
