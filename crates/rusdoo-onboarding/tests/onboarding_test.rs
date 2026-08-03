//! O ciclo de um onboarding contra um banco de verdade: um passo
//! marcado, o painel que só fica pronto quando não sobra passo, e o
//! progresso que é de cada empresa.

use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Cada teste no seu schema: eles criam as mesmas tabelas e rodam juntos.
fn pool(url: &str, schema: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap()
}

/// Um registro com base e onboarding, e as tabelas vazias.
async fn fixture(url: &str, schema: &'static str) -> (Registry, PgPool) {
    let pool = pool(url, schema);
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_onboarding::extend(&mut registry).unwrap();
    for table in [
        "onboarding_progress_step_rel",
        "onboarding_onboarding_step_rel",
        "onboarding_progress_step",
        "onboarding_progress",
        "onboarding_onboarding_step",
        "onboarding_onboarding",
        "res_users",
        "res_company",
        "res_partner",
        "res_country",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "res.country",
        "res.company",
        "res.partner",
        "res.users",
        "onboarding.onboarding",
        "onboarding.onboarding.step",
        "onboarding.progress",
        "onboarding.progress.step",
    ] {
        registry
            .get(model)
            .unwrap()
            .init_table(&pool)
            .await
            .unwrap();
    }
    (registry, pool)
}

/// Chama um método registrado como `uid` chamaria pelo `call_kw`.
async fn call(
    registry: &Registry,
    pool: &PgPool,
    uid: i64,
    model: &str,
    method: &str,
    ids: Vec<i64>,
) -> Result<Value, rusdoo_core::RusdooError> {
    let mut methods = MethodRegistry::new();
    rusdoo_onboarding::extend_methods(&mut methods).unwrap();
    let found = methods.get(model, method).expect("método registrado");
    assert_eq!(
        found.operation,
        Operation::Write,
        "todo botão de onboarding grava alguma coisa"
    );
    let ctx = MethodCtx::new(registry, pool, uid, model, ids);
    let args: Vec<Value> = Vec::new();
    let kwargs = Map::new();
    (found.func)(ctx, &args, &kwargs).await
}

/// Um usuário de uma empresa, para o método ter de onde tirar a empresa.
async fn a_user(registry: &Registry, pool: &PgPool, login: &str, company: i64) -> i64 {
    registry
        .create(
            pool,
            "res.users",
            vec![
                ("login", json!(login)),
                ("name", json!(login)),
                ("company_id", json!(company)),
            ],
        )
        .await
        .unwrap()
}

/// Um painel com dois passos, ambos por empresa.
async fn a_panel_of_two_steps(registry: &Registry, pool: &PgPool) -> (i64, i64, i64) {
    let first = registry
        .create(
            pool,
            "onboarding.onboarding.step",
            vec![
                ("title", json!("Cadastrar a empresa")),
                ("panel_step_open_action_name", json!("action_open_company")),
            ],
        )
        .await
        .unwrap();
    let second = registry
        .create(
            pool,
            "onboarding.onboarding.step",
            vec![
                ("title", json!("Cadastrar um produto")),
                ("panel_step_open_action_name", json!("action_open_product")),
            ],
        )
        .await
        .unwrap();
    let panel = registry
        .create(
            pool,
            "onboarding.onboarding",
            vec![
                ("name", json!("Configuração inicial")),
                ("route_name", json!("setup")),
                ("step_ids", json!([[6, 0, [first, second]]])),
            ],
        )
        .await
        .unwrap();
    (panel, first, second)
}

async fn panel_state(registry: &Registry, pool: &PgPool, progress: i64) -> String {
    registry
        .read(
            pool,
            "onboarding.progress",
            &[progress],
            &["onboarding_state"],
        )
        .await
        .unwrap()
        .first()
        .and_then(|row| row.get("onboarding_state"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_panel_is_done_only_when_the_last_step_is_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_onboarding_lifecycle")).await;
    let company = registry
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let uid = a_user(&registry, &pool, "ana", company).await;
    let (panel, first, second) = a_panel_of_two_steps(&registry, &pool).await;

    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding.step",
        "action_set_just_done",
        vec![first],
    )
    .await
    .unwrap();
    assert_eq!(answer, json!("just_done"));

    // o progresso nasceu ao marcar o primeiro passo, e é da empresa
    let progress = registry
        .search(
            &pool,
            "onboarding.progress",
            &rusdoo_orm::domain::parse_domain(&json!([["onboarding_id", "=", panel]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        progress.len(),
        1,
        "um progresso por empresa, não um por clique"
    );
    let progress = progress[0];
    assert_eq!(
        panel_state(&registry, &pool, progress).await,
        "not_done",
        "ainda falta um passo"
    );

    // o segundo passo fecha o painel
    call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding.step",
        "action_set_just_done",
        vec![second],
    )
    .await
    .unwrap();
    assert_eq!(panel_state(&registry, &pool, progress).await, "done");

    // ler o painel comemora uma vez e consolida os `just_done`
    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.progress",
        "action_step_states",
        vec![progress],
    )
    .await
    .unwrap();
    assert_eq!(answer["onboarding_state"], "just_done", "{answer}");
    assert_eq!(answer["steps"][first.to_string()], "just_done");

    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.progress",
        "action_step_states",
        vec![progress],
    )
    .await
    .unwrap();
    assert_eq!(
        answer["onboarding_state"], "done",
        "a segunda leitura não comemora de novo: {answer}"
    );
    assert_eq!(answer["steps"][second.to_string()], "done");
}

#[tokio::test]
async fn a_step_already_done_is_not_done_again_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_onboarding_idempotent")).await;
    let company = registry
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let uid = a_user(&registry, &pool, "bruno", company).await;
    let (_panel, first, _second) = a_panel_of_two_steps(&registry, &pool).await;

    call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding.step",
        "action_set_just_done",
        vec![first],
    )
    .await
    .unwrap();
    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding.step",
        "action_set_just_done",
        vec![first],
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        json!("was_done"),
        "clicar duas vezes não refaz nada"
    );

    // e continua havendo um registro de progresso só para o passo
    let steps = registry
        .search(
            &pool,
            "onboarding.progress.step",
            &rusdoo_orm::domain::parse_domain(&json!([["step_id", "=", first]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(steps.len(), 1);

    // marcar dois passos de uma vez não é o que o painel faz
    let error = call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding.step",
        "action_set_just_done",
        vec![first, first],
    )
    .await
    .expect_err("um clique, um passo");
    assert!(
        error.to_string().contains("um passo de cada vez"),
        "{error}"
    );
}

#[tokio::test]
async fn each_company_walks_its_own_checklist_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_onboarding_companies")).await;
    let acme = registry
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let globex = registry
        .create(&pool, "res.company", vec![("name", json!("Globex"))])
        .await
        .unwrap();
    let ana = a_user(&registry, &pool, "ana", acme).await;
    let bob = a_user(&registry, &pool, "bob", globex).await;
    let (panel, first, second) = a_panel_of_two_steps(&registry, &pool).await;

    for step in [first, second] {
        call(
            &registry,
            &pool,
            ana,
            "onboarding.onboarding.step",
            "action_set_just_done",
            vec![step],
        )
        .await
        .unwrap();
    }

    let progresses = registry
        .search(
            &pool,
            "onboarding.progress",
            &rusdoo_orm::domain::parse_domain(&json!([["onboarding_id", "=", panel]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(progresses.len(), 1, "só a Acme começou");
    assert_eq!(panel_state(&registry, &pool, progresses[0]).await, "done");

    // a Globex abre o mesmo painel e não herda nada
    let action = call(
        &registry,
        &pool,
        bob,
        "onboarding.onboarding",
        "action_open_progress",
        vec![panel],
    )
    .await
    .unwrap();
    assert_eq!(action["res_model"], "onboarding.progress");
    let globex_progress = action["res_id"].as_i64().expect("um progresso");
    assert_ne!(globex_progress, progresses[0]);
    assert_eq!(
        panel_state(&registry, &pool, globex_progress).await,
        "not_done",
        "o checklist da Globex está inteiro pela frente"
    );

    let answer = call(
        &registry,
        &pool,
        bob,
        "onboarding.progress",
        "action_step_states",
        vec![globex_progress],
    )
    .await
    .unwrap();
    assert_eq!(answer["steps"][first.to_string()], "not_done", "{answer}");
    assert_eq!(answer["steps"][second.to_string()], "not_done");
}

#[tokio::test]
async fn closing_the_panel_is_remembered_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_onboarding_closing")).await;
    let company = registry
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let uid = a_user(&registry, &pool, "carla", company).await;
    let (panel, _first, _second) = a_panel_of_two_steps(&registry, &pool).await;

    call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding",
        "action_close",
        vec![panel],
    )
    .await
    .unwrap();
    let progress = registry
        .search(
            &pool,
            "onboarding.progress",
            &rusdoo_orm::domain::parse_domain(&json!([["onboarding_id", "=", panel]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap()[0];
    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.progress",
        "action_step_states",
        vec![progress],
    )
    .await
    .unwrap();
    assert_eq!(
        answer["onboarding_state"], "closed",
        "um painel fechado não volta sozinho: {answer}"
    );

    // e o botão de mostrar/esconder traz de volta
    let visible = call(
        &registry,
        &pool,
        uid,
        "onboarding.onboarding",
        "action_toggle_visibility",
        vec![panel],
    )
    .await
    .unwrap();
    assert_eq!(visible, json!(false), "deixou de estar fechado");
    let answer = call(
        &registry,
        &pool,
        uid,
        "onboarding.progress",
        "action_step_states",
        vec![progress],
    )
    .await
    .unwrap();
    assert_eq!(answer["onboarding_state"], "not_done", "{answer}");
}

#[tokio::test]
async fn what_the_models_refuse_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_onboarding_refusals")).await;

    // uma rota com espaço não vira `/onboarding/<rota>`
    let error = registry
        .create(
            &pool,
            "onboarding.onboarding",
            vec![
                ("name", json!("Configuração")),
                ("route_name", json!("configuração inicial")),
            ],
        )
        .await
        .expect_err("uma rota de duas palavras");
    assert!(error.to_string().contains("uma palavra só"), "{error}");

    // um passo sem ação de abertura não pode ser pendurado num painel
    let orphan = registry
        .create(
            &pool,
            "onboarding.onboarding.step",
            vec![("title", json!("Passo solto"))],
        )
        .await
        .expect("fora de um painel ele é legítimo");
    let panel = registry
        .create(
            &pool,
            "onboarding.onboarding",
            vec![
                ("name", json!("Configuração")),
                ("route_name", json!("setup")),
            ],
        )
        .await
        .unwrap();
    let error = registry
        .write(
            &pool,
            "onboarding.onboarding.step",
            &[orphan],
            vec![("onboarding_ids", json!([[6, 0, [panel]]]))],
        )
        .await
        .expect_err("um botão que não abre nada");
    assert!(error.to_string().contains("ação de abertura"), "{error}");

    // e o vínculo não ficou para trás
    let rows = registry
        .read(
            &pool,
            "onboarding.onboarding.step",
            &[orphan],
            &["onboarding_ids"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["onboarding_ids"], json!([]));
}
