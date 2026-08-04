//! Being at work, against the database: the button that toggles, the
//! hours it leaves behind, and the two mistakes the model refuses.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 4] = ["base", "resource", "hr", "hr_attendance"];

async fn punch(case: &TransactionCase, employee: i64) -> Value {
    let methods = case.methods();
    let entry = methods
        .get("hr.employee", "attendance_manual")
        .expect("attendance_manual is registered");
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, "hr.employee", vec![employee]);
    entry
        .call(ctx, &[], &Map::new())
        .await
        .unwrap_or_else(|error| panic!("attendance_manual: {error}"))
}

#[tokio::test]
async fn the_button_checks_in_then_out_live() {
    let Some(case) = TransactionCase::open("attendance_toggle", &MODULES).await else {
        return;
    };
    let ana = case
        .models()
        .create(&case.pool(), "hr.employee", vec![("name", json!("Ana"))])
        .await
        .expect("the employee saves");

    let first = punch(&case, ana).await;
    assert_eq!(first["action"], json!("checked_in"), "{first}");
    let attendance = first["attendance_id"].as_i64().expect("an attendance");

    // still at work: no hours closed yet, and the record is open
    let rows = case
        .models()
        .read(
            &case.pool(),
            "hr.attendance",
            &[attendance],
            &["check_out", "worked_hours", "date"],
        )
        .await
        .expect("the attendance reads");
    assert!(rows[0]["check_out"].is_null(), "{:?}", rows[0]);
    assert!(!rows[0]["date"].is_null(), "sem dia: {:?}", rows[0]);

    let second = punch(&case, ana).await;
    assert_eq!(second["action"], json!("checked_out"), "{second}");
    assert_eq!(
        second["attendance_id"].as_i64(),
        Some(attendance),
        "fechou o registro que estava aberto, não outro"
    );

    // and a third press opens a new one rather than reopening the closed
    // one
    let third = punch(&case, ana).await;
    assert_eq!(third["action"], json!("checked_in"), "{third}");
    assert_ne!(third["attendance_id"].as_i64(), Some(attendance));

    case.close().await;
}

#[tokio::test]
async fn the_hours_are_counted_and_the_day_is_the_one_it_started_live() {
    let Some(case) = TransactionCase::open("attendance_hours", &MODULES).await else {
        return;
    };
    let bruno = case
        .models()
        .create(&case.pool(), "hr.employee", vec![("name", json!("Bruno"))])
        .await
        .expect("the employee saves");

    // a night shift, written as it happened
    let attendance = case
        .models()
        .create(
            &case.pool(),
            "hr.attendance",
            vec![
                ("employee_id", json!(bruno)),
                ("check_in", json!("2026-08-04 22:00:00")),
                ("check_out", json!("2026-08-05 06:30:00")),
            ],
        )
        .await
        .expect("the attendance saves");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "hr.attendance",
            &[attendance],
            &["worked_hours", "date"],
        )
        .await
        .expect("the attendance reads");
    assert_eq!(rows[0]["worked_hours"], json!(8.5), "{:?}", rows[0]);
    assert_eq!(rows[0]["date"], json!("2026-08-04"), "{:?}", rows[0]);

    // stored, so a report can group and sum by them
    let hours: f64 = sqlx::query_scalar(
        r#"SELECT SUM("worked_hours")::float8 FROM "hr_attendance" WHERE "employee_id" = $1"#,
    )
    .bind(bruno as i32)
    .fetch_one(&case.pool())
    .await
    .expect("the sum runs in SQL");
    assert_eq!(hours, 8.5);

    case.close().await;
}

#[tokio::test]
async fn leaving_before_arriving_is_refused_live() {
    let Some(case) = TransactionCase::open("attendance_backwards", &MODULES).await else {
        return;
    };
    let carla = case
        .models()
        .create(&case.pool(), "hr.employee", vec![("name", json!("Carla"))])
        .await
        .expect("the employee saves");

    let refused = case
        .models()
        .create(
            &case.pool(),
            "hr.attendance",
            vec![
                ("employee_id", json!(carla)),
                ("check_in", json!("2026-08-04 18:00:00")),
                ("check_out", json!("2026-08-04 09:00:00")),
            ],
        )
        .await;
    assert!(refused.is_err(), "saiu antes de entrar e foi aceito");

    case.close().await;
}
