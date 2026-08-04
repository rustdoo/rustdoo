//! The machines against the database: a request opened on a piece of
//! equipment, the count of what is still open, and the two mistakes the
//! model refuses.

use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 4] = ["base", "resource", "hr", "maintenance"];

async fn create(case: &TransactionCase, model: &str, values: Vec<(&str, Value)>) -> i64 {
    case.models()
        .create(&case.pool(), model, values)
        .await
        .unwrap_or_else(|error| panic!("{model} saves: {error}"))
}

async fn read(case: &TransactionCase, model: &str, id: i64, fields: &[&str]) -> Value {
    Value::Object(
        case.models()
            .read(&case.pool(), model, &[id], fields)
            .await
            .unwrap_or_else(|error| panic!("{model} reads: {error}"))
            .into_iter()
            .next()
            .expect("the record exists"),
    )
}

#[tokio::test]
async fn a_press_that_broke_is_a_request_on_a_machine_live() {
    let Some(case) = TransactionCase::open("maintenance_request", &MODULES).await else {
        return;
    };
    let new = create(
        &case,
        "maintenance.stage",
        vec![("name", json!("Novo")), ("sequence", json!(1))],
    )
    .await;
    let done = create(
        &case,
        "maintenance.stage",
        vec![
            ("name", json!("Resolvido")),
            ("sequence", json!(9)),
            ("done", json!(true)),
        ],
    )
    .await;
    let category = create(
        &case,
        "maintenance.equipment.category",
        vec![("name", json!("Prensas"))],
    )
    .await;
    let press = create(
        &case,
        "maintenance.equipment",
        vec![
            ("name", json!("Prensa 1")),
            ("category_id", json!(category)),
            ("serial_no", json!("PR-0001")),
        ],
    )
    .await;

    // two requests: one still open, one already resolved
    let broken = create(
        &case,
        "maintenance.request",
        vec![
            ("name", json!("Não liga")),
            ("equipment_id", json!(press)),
            ("stage_id", json!(new)),
        ],
    )
    .await;
    create(
        &case,
        "maintenance.request",
        vec![
            ("name", json!("Troca de óleo")),
            ("equipment_id", json!(press)),
            ("maintenance_type", json!("preventive")),
            ("stage_id", json!(done)),
            ("close_date", json!("2026-08-04")),
        ],
    )
    .await;

    let row = read(
        &case,
        "maintenance.equipment",
        press,
        &["maintenance_count", "maintenance_open_count"],
    )
    .await;
    assert_eq!(row["maintenance_count"], json!(2), "{row}");
    assert_eq!(
        row["maintenance_open_count"],
        json!(1),
        "só o que não está numa etapa concluída: {row}"
    );

    // the request knows whether it is done, through the stage it is in
    let row = read(
        &case,
        "maintenance.request",
        broken,
        &["stage_done", "maintenance_type"],
    )
    .await;
    assert_eq!(row["stage_done"], json!(false), "{row}");
    assert_eq!(row["maintenance_type"], json!("corrective"), "{row}");

    // and the category counts its machines
    let row = read(
        &case,
        "maintenance.equipment.category",
        category,
        &["equipment_count"],
    )
    .await;
    assert_eq!(row["equipment_count"], json!(1), "{row}");

    case.close().await;
}

#[tokio::test]
async fn a_repair_closed_before_it_was_asked_for_is_refused_live() {
    let Some(case) = TransactionCase::open("maintenance_dates", &MODULES).await else {
        return;
    };
    let refused = case
        .models()
        .create(
            &case.pool(),
            "maintenance.request",
            vec![
                ("name", json!("Datas trocadas")),
                ("request_date", json!("2026-08-10")),
                ("close_date", json!("2026-08-04")),
            ],
        )
        .await;
    assert!(refused.is_err(), "fechou antes de abrir e foi aceito");

    case.close().await;
}

#[tokio::test]
async fn a_category_with_machines_in_it_is_not_deleted_live() {
    let Some(case) = TransactionCase::open("maintenance_category", &MODULES).await else {
        return;
    };
    let category = create(
        &case,
        "maintenance.equipment.category",
        vec![("name", json!("Veículos"))],
    )
    .await;
    create(
        &case,
        "maintenance.equipment",
        vec![("name", json!("Van")), ("category_id", json!(category))],
    )
    .await;

    // `restrict`: apagar a categoria deixaria máquinas apontando para o
    // nada, e é o banco que segura isso
    let refused = case
        .models()
        .unlink_as(&case.pool(), 1, "maintenance.equipment.category", &[category])
        .await;
    assert!(refused.is_err(), "apagou a categoria com máquinas nela");

    case.close().await;
}
