//! The pipeline against the database: a scrap of paper becomes an
//! opportunity, moves through the stages, and ends at one of the two
//! ends the module exists for.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Map, Value};

const MODULES: [&str; 4] = ["base", "utm", "sales_team", "crm"];

async fn create(case: &TransactionCase, model: &str, values: Vec<(&str, Value)>) -> i64 {
    case.models()
        .create(&case.pool(), model, values)
        .await
        .unwrap_or_else(|error| panic!("{model} saves: {error}"))
}

async fn read(case: &TransactionCase, id: i64, fields: &[&str]) -> Value {
    Value::Object(
        case.models()
            .read(&case.pool(), "crm.lead", &[id], fields)
            .await
            .expect("the lead reads")
            .into_iter()
            .next()
            .expect("the lead exists"),
    )
}

async fn ask(
    case: &TransactionCase,
    ids: &[i64],
    method: &str,
    kwargs: Value,
) -> Result<Value, String> {
    let methods = case.methods();
    let entry = methods
        .get("crm.lead", method)
        .unwrap_or_else(|| panic!("crm.lead.{method} is not registered"));
    let pool = case.pool();
    let ctx = MethodCtx::new(case.registry(), &pool, 1, "crm.lead", ids.to_vec());
    let kwargs: Map<String, Value> = kwargs.as_object().cloned().unwrap_or_default();
    entry
        .call(ctx, &[], &kwargs)
        .await
        .map_err(|error| error.to_string())
}

/// New / Qualified / Won, which is the shortest pipeline that has both
/// ends.
async fn a_pipeline(case: &TransactionCase) -> (i64, i64) {
    let new = create(
        case,
        "crm.stage",
        vec![("name", json!("Novo")), ("sequence", json!(1))],
    )
    .await;
    create(
        case,
        "crm.stage",
        vec![("name", json!("Qualificado")), ("sequence", json!(2))],
    )
    .await;
    let won = create(
        case,
        "crm.stage",
        vec![
            ("name", json!("Ganho")),
            ("sequence", json!(3)),
            ("is_won", json!(true)),
        ],
    )
    .await;
    (new, won)
}

#[tokio::test]
async fn a_lead_becomes_an_opportunity_and_enters_the_pipeline_live() {
    let Some(case) = TransactionCase::open("crm_convert", &MODULES).await else {
        return;
    };
    let (new, _) = a_pipeline(&case).await;

    // arrives as a scrap of paper: a name and an email, no contact record
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Interessado no plano anual")),
            ("contact_name", json!("Ana Ribeiro")),
            ("email_from", json!("ana@exemplo.com")),
        ],
    )
    .await;
    let row = read(&case, lead, &["type", "stage_id"]).await;
    assert_eq!(row["type"], json!("lead"), "{row}");

    ask(&case, &[lead], "convert_opportunity", json!({}))
        .await
        .expect("converting works");
    let row = read(&case, lead, &["type", "stage_id"]).await;
    assert_eq!(row["type"], json!("opportunity"), "{row}");
    assert_eq!(row["stage_id"][0], json!(new), "entra na primeira coluna");

    // and converting it twice is a mistake, not a no-op: the second one
    // would move a deal already being worked back to the start
    let refused = ask(&case, &[lead], "convert_opportunity", json!({})).await;
    assert!(refused.is_err(), "converteu de novo: {refused:?}");

    case.close().await;
}

#[tokio::test]
async fn winning_moves_the_deal_to_the_end_and_pins_the_probability_live() {
    let Some(case) = TransactionCase::open("crm_won", &MODULES).await else {
        return;
    };
    let (new, won) = a_pipeline(&case).await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Contrato anual")),
            ("type", json!("opportunity")),
            ("stage_id", json!(new)),
            ("expected_revenue", json!(12000.0)),
            ("probability", json!(40.0)),
        ],
    )
    .await;

    ask(&case, &[lead], "action_set_won", json!({}))
        .await
        .expect("winning works");
    let row = read(
        &case,
        lead,
        &["stage_id", "probability", "date_closed", "active"],
    )
    .await;
    assert_eq!(row["stage_id"][0], json!(won), "{row}");
    assert_eq!(row["probability"], json!(100.0), "{row}");
    assert!(!row["date_closed"].is_null(), "sem data de fechamento: {row}");
    assert_eq!(row["active"], json!(true), "um ganho não some da tela");

    case.close().await;
}

#[tokio::test]
async fn losing_needs_a_reason_and_keeps_it_live() {
    let Some(case) = TransactionCase::open("crm_lost", &MODULES).await else {
        return;
    };
    let (new, _) = a_pipeline(&case).await;
    let reason = create(
        &case,
        "crm.lost.reason",
        vec![("name", json!("Preço"))],
    )
    .await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Proposta recusada")),
            ("type", json!("opportunity")),
            ("stage_id", json!(new)),
        ],
    )
    .await;

    // "we lost" with no why is a row nobody can learn from, so it is
    // refused rather than accepted with a null
    let refused = ask(&case, &[lead], "action_set_lost", json!({})).await;
    assert!(refused.is_err(), "perdeu sem motivo: {refused:?}");
    // and a reason that does not exist is not a reason
    let refused = ask(
        &case,
        &[lead],
        "action_set_lost",
        json!({"lost_reason_id": 987654}),
    )
    .await;
    assert!(refused.is_err(), "motivo inexistente aceito: {refused:?}");

    ask(
        &case,
        &[lead],
        "action_set_lost",
        json!({"lost_reason_id": reason}),
    )
    .await
    .expect("losing with a reason works");

    // archived, not deleted: what was lost and why is next quarter's
    // planning material
    let rows = case
        .models()
        .read(
            &case.pool(),
            "crm.lead",
            &[lead],
            &["probability", "lost_reason_id", "active"],
        )
        .await
        .expect("the lead still reads");
    let row = &rows[0];
    assert_eq!(row["probability"], json!(0.0), "{row:?}");
    assert_eq!(row["lost_reason_id"][0], json!(reason), "{row:?}");
    assert_eq!(row["active"], json!(false), "{row:?}");

    // and it is out of the pipeline the salesperson looks at
    let open = case
        .models()
        .search(
            &case.pool(),
            "crm.lead",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    assert!(open.is_empty(), "o perdido continua na lista: {open:?}");

    case.close().await;
}

#[tokio::test]
async fn a_won_deal_that_was_lost_comes_back_live() {
    let Some(case) = TransactionCase::open("crm_reopen", &MODULES).await else {
        return;
    };
    let (new, won) = a_pipeline(&case).await;
    let reason = create(&case, "crm.lost.reason", vec![("name", json!("Prazo"))]).await;
    let lead = create(
        &case,
        "crm.lead",
        vec![
            ("name", json!("Voltou atrás")),
            ("type", json!("opportunity")),
            ("stage_id", json!(new)),
        ],
    )
    .await;

    ask(
        &case,
        &[lead],
        "action_set_lost",
        json!({"lost_reason_id": reason}),
    )
    .await
    .expect("losing works");
    ask(&case, &[lead], "action_set_won", json!({}))
        .await
        .expect("winning after losing works");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "crm.lead",
            &[lead],
            &["active", "stage_id", "probability", "lost_reason_id"],
        )
        .await
        .expect("the lead reads");
    let row = &rows[0];
    assert_eq!(row["active"], json!(true), "ganhar traz de volta: {row:?}");
    assert_eq!(row["stage_id"][0], json!(won), "{row:?}");
    assert_eq!(row["probability"], json!(100.0), "{row:?}");
    assert!(
        row["lost_reason_id"].is_null(),
        "o motivo da perda ficou pendurado num ganho: {row:?}"
    );

    case.close().await;
}

#[tokio::test]
async fn the_numbers_a_report_sums_are_checked_live() {
    let Some(case) = TransactionCase::open("crm_numbers", &MODULES).await else {
        return;
    };
    let refused = case
        .models()
        .create(
            &case.pool(),
            "crm.lead",
            vec![
                ("name", json!("Impossível")),
                ("probability", json!(140.0)),
            ],
        )
        .await;
    assert!(refused.is_err(), "probabilidade de 140% aceita");

    let refused = case
        .models()
        .create(
            &case.pool(),
            "crm.lead",
            vec![
                ("name", json!("Negativo")),
                ("expected_revenue", json!(-1.0)),
            ],
        )
        .await;
    assert!(refused.is_err(), "receita negativa aceita");

    case.close().await;
}
