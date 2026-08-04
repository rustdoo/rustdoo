//! The work against the database: tasks in a project, the two dimensions
//! they live in, and the counts a manager reads.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 2] = ["base", "project"];

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

async fn ask(case: &TransactionCase, ids: &[i64], method: &str) -> Result<Value, String> {
    let methods = case.methods();
    let entry = methods
        .get("project.task", method)
        .unwrap_or_else(|| panic!("project.task.{method} is not registered"));
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, "project.task", ids.to_vec());
    entry
        .call(ctx, &[], &Map::new())
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn a_project_counts_the_work_still_on_somebodys_plate_live() {
    let Some(case) = TransactionCase::open("project_counts", &MODULES).await else {
        return;
    };
    let backlog = create(
        &case,
        "project.task.type",
        vec![("name", json!("Backlog")), ("sequence", json!(1))],
    )
    .await;
    let site = create(
        &case,
        "project.project",
        vec![
            ("name", json!("Site novo")),
            ("type_ids", json!([[6, 0, [backlog]]])),
        ],
    )
    .await;

    let open = create(
        &case,
        "project.task",
        vec![
            ("name", json!("Desenhar a home")),
            ("project_id", json!(site)),
            ("stage_id", json!(backlog)),
            ("allocated_hours", json!(8.0)),
        ],
    )
    .await;
    create(
        &case,
        "project.task",
        vec![
            ("name", json!("Comprar domínio")),
            ("project_id", json!(site)),
            ("state", json!("1_done")),
        ],
    )
    .await;
    create(
        &case,
        "project.task",
        vec![
            ("name", json!("Ideia descartada")),
            ("project_id", json!(site)),
            ("state", json!("1_canceled")),
        ],
    )
    .await;

    let row = read(
        &case,
        "project.project",
        site,
        &["task_count", "open_task_count"],
    )
    .await;
    assert_eq!(row["task_count"], json!(3), "{row}");
    assert_eq!(
        row["open_task_count"],
        json!(1),
        "concluída e cancelada saem do prato: {row}"
    );

    // the task lives in both dimensions at once
    let row = read(&case, "project.task", open, &["stage_id", "state", "is_closed"]).await;
    assert_eq!(row["stage_id"][0], json!(backlog), "{row}");
    assert_eq!(row["state"], json!("01_in_progress"), "{row}");
    assert_eq!(row["is_closed"], json!(false), "{row}");

    case.close().await;
}

#[tokio::test]
async fn done_and_cancelled_are_both_closed_and_never_the_same_thing_live() {
    let Some(case) = TransactionCase::open("project_close", &MODULES).await else {
        return;
    };
    let site = create(&case, "project.project", vec![("name", json!("Projeto"))]).await;
    let finished = create(
        &case,
        "project.task",
        vec![("name", json!("Feita")), ("project_id", json!(site))],
    )
    .await;
    let dropped = create(
        &case,
        "project.task",
        vec![("name", json!("Abandonada")), ("project_id", json!(site))],
    )
    .await;

    ask(&case, &[finished], "action_done")
        .await
        .expect("closing works");
    ask(&case, &[dropped], "action_cancel")
        .await
        .expect("cancelling works");

    let done = read(&case, "project.task", finished, &["state", "is_closed", "date_end"]).await;
    assert_eq!(done["state"], json!("1_done"), "{done}");
    assert_eq!(done["is_closed"], json!(true), "{done}");
    assert!(!done["date_end"].is_null(), "sem data de encerramento: {done}");

    let cancelled = read(&case, "project.task", dropped, &["state", "is_closed"]).await;
    assert_eq!(cancelled["state"], json!("1_canceled"), "{cancelled}");
    assert_eq!(cancelled["is_closed"], json!(true), "{cancelled}");

    // both are closed, and a report can still tell them apart — which is
    // the whole reason Odoo keeps two words for it
    assert_ne!(done["state"], cancelled["state"]);

    case.close().await;
}

#[tokio::test]
async fn deleting_a_project_takes_its_tasks_with_it_live() {
    let Some(case) = TransactionCase::open("project_unlink", &MODULES).await else {
        return;
    };
    let site = create(&case, "project.project", vec![("name", json!("Some"))]).await;
    let task = create(
        &case,
        "project.task",
        vec![("name", json!("Vai junto")), ("project_id", json!(site))],
    )
    .await;

    case.models()
        .unlink_as(&case.pool(), 1, "project.project", &[site])
        .await
        .expect("the project is deleted");

    let left = case
        .models()
        .read(&case.pool(), "project.task", &[task], &["name"])
        .await
        .expect("the read runs");
    assert!(left.is_empty(), "a tarefa ficou órfã: {left:?}");

    case.close().await;
}

#[tokio::test]
async fn an_estimate_below_zero_is_refused_live() {
    let Some(case) = TransactionCase::open("project_hours", &MODULES).await else {
        return;
    };
    let refused = case
        .models()
        .create(
            &case.pool(),
            "project.task",
            vec![
                ("name", json!("Negativa")),
                ("allocated_hours", json!(-2.0)),
            ],
        )
        .await;
    assert!(refused.is_err(), "estimativa negativa aceita");

    case.close().await;
}
