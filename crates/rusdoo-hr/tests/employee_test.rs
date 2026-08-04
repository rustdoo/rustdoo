//! The organisation against the database: an employee who is also a
//! resource, a department that knows its place in the tree, and a job
//! that counts who holds it.

use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 3] = ["base", "resource", "hr"];

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
async fn hiring_somebody_creates_the_resource_they_are_live() {
    let Some(case) = TransactionCase::open("hr_employee", &MODULES).await else {
        return;
    };

    // created the way every caller does it: as an employee, saying
    // nothing about resources
    let ana = create(
        &case,
        "hr.employee",
        vec![
            ("name", json!("Ana Ribeiro")),
            ("job_title", json!("Analista de vendas")),
            ("work_email", json!("ana@exemplo.com")),
        ],
    )
    .await;

    let row = read(&case, "hr.employee", ana, &["name", "job_title", "resource_id"]).await;
    assert_eq!(row["name"], json!("Ana Ribeiro"));
    let resource = row["resource_id"][0]
        .as_i64()
        .expect("the employee is a resource");

    // and the resource is a record of its own, holding the name — which
    // is how a planning module finds her without knowing about hr
    let row = read(&case, "resource.resource", resource, &["name", "resource_type"]).await;
    assert_eq!(row["name"], json!("Ana Ribeiro"));
    assert_eq!(row["resource_type"], json!("user"));

    // renaming the employee renames the resource: one value, not two
    // that drift apart
    case.models()
        .write(
            &case.pool(),
            "hr.employee",
            &[ana],
            vec![("name", json!("Ana Ribeiro Costa"))],
        )
        .await
        .expect("the rename saves");
    let row = read(&case, "resource.resource", resource, &["name"]).await;
    assert_eq!(row["name"], json!("Ana Ribeiro Costa"));

    case.close().await;
}

#[tokio::test]
async fn an_employee_works_the_hours_of_their_resource_live() {
    let Some(case) = TransactionCase::open("hr_hours", &MODULES).await else {
        return;
    };

    let schedule = create(
        &case,
        "resource.calendar",
        vec![
            ("name", json!("Meio período")),
            (
                "attendance_ids",
                json!([
                    [0, 0, {"name": "seg", "dayofweek": "0", "hour_from": 8.0, "hour_to": 12.0}],
                    [0, 0, {"name": "ter", "dayofweek": "1", "hour_from": 8.0, "hour_to": 12.0}],
                ]),
            ),
        ],
    )
    .await;

    let bruno = create(
        &case,
        "hr.employee",
        vec![("name", json!("Bruno")), ("calendar_id", json!(schedule))],
    )
    .await;

    // the working hours are the resource's, written and read through the
    // employee like any other delegated field
    let row = read(&case, "hr.employee", bruno, &["calendar_id", "tz"]).await;
    assert_eq!(row["calendar_id"][0], json!(schedule), "{row}");
    assert_eq!(row["calendar_id"][1], json!("Meio período"), "{row}");
    assert_eq!(row["tz"], json!("UTC"));

    // and the schedule counts him among what it schedules
    let row = read(&case, "resource.calendar", schedule, &["resource_ids"]).await;
    assert_eq!(
        row["resource_ids"].as_array().map(Vec::len),
        Some(1),
        "{row}"
    );

    case.close().await;
}

#[tokio::test]
async fn a_department_knows_its_place_and_its_people_live() {
    let Some(case) = TransactionCase::open("hr_department", &MODULES).await else {
        return;
    };

    let board = create(&case, "hr.department", vec![("name", json!("Diretoria"))]).await;
    let sales = create(
        &case,
        "hr.department",
        vec![("name", json!("Vendas")), ("parent_id", json!(board))],
    )
    .await;
    let field = create(
        &case,
        "hr.department",
        vec![("name", json!("Campo")), ("parent_id", json!(sales))],
    )
    .await;

    // the name a department is known by, once there is more than one
    // level of them
    let row = read(&case, "hr.department", field, &["complete_name"]).await;
    assert_eq!(row["complete_name"], json!("Diretoria / Vendas / Campo"));

    // the people in it are counted, and only the ones in it
    for name in ["Ana", "Bruno"] {
        create(
            &case,
            "hr.employee",
            vec![("name", json!(name)), ("department_id", json!(sales))],
        )
        .await;
    }
    create(
        &case,
        "hr.employee",
        vec![("name", json!("Carla")), ("department_id", json!(field))],
    )
    .await;
    let row = read(&case, "hr.department", sales, &["total_employee"]).await;
    assert_eq!(row["total_employee"], json!(2), "{row}");

    // a department that reports to itself is refused
    let refused = case
        .models()
        .write(
            &case.pool(),
            "hr.department",
            &[sales],
            vec![("parent_id", json!(sales))],
        )
        .await;
    assert!(refused.is_err(), "um departamento chefiando a si mesmo");

    case.close().await;
}

#[tokio::test]
async fn a_job_counts_who_holds_it_and_who_is_still_wanted_live() {
    let Some(case) = TransactionCase::open("hr_job", &MODULES).await else {
        return;
    };

    let job = create(
        &case,
        "hr.job",
        vec![
            ("name", json!("Vendedor")),
            ("no_of_recruitment", json!(2)),
        ],
    )
    .await;
    create(
        &case,
        "hr.employee",
        vec![("name", json!("Ana")), ("job_id", json!(job))],
    )
    .await;

    let row = read(
        &case,
        "hr.job",
        job,
        &["no_of_employee", "expected_employees"],
    )
    .await;
    assert_eq!(row["no_of_employee"], json!(1), "{row}");
    assert_eq!(row["expected_employees"], json!(3), "{row}");

    case.close().await;
}

#[tokio::test]
async fn the_list_is_ordered_by_a_name_the_employee_does_not_own_live() {
    let Some(case) = TransactionCase::open("hr_order", &MODULES).await else {
        return;
    };

    for name in ["Carla", "Ana", "Bruno"] {
        create(&case, "hr.employee", vec![("name", json!(name))]).await;
    }

    // `_order = 'name'`, and the name is the resource's — the search
    // reaches it through the link
    let found = case
        .models()
        .search(
            &case.pool(),
            "hr.employee",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    let rows = case
        .models()
        .read(&case.pool(), "hr.employee", &found, &["name"])
        .await
        .expect("the employees read");
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, ["Ana", "Bruno", "Carla"]);

    // and a search on that name filters by it, through the same link
    let found = case
        .models()
        .search(
            &case.pool(),
            "hr.employee",
            &rusdoo_orm::domain::parse_domain(&json!([["name", "like", "Bru"]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the filtered search runs");
    assert_eq!(found.len(), 1, "{found:?}");

    case.close().await;
}
