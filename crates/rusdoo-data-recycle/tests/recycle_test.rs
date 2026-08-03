//! O ciclo da reciclagem: o que a varredura separa, o que ela deixa em
//! paz, e o que acontece quando alguém valida ou descarta.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

const RULE: &str = "data_recycle.model";
const RECORD: &str = "data_recycle.record";

/// O modelo varrido pelos testes: um documento que fecha numa data e
/// pode ser arquivado.
const DOC: &str = "rusdoo.test.doc";
/// Um modelo sem `active`: só dá para excluir o que ele guarda.
const LOG: &str = "rusdoo.test.log";

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn pool(url: &str, schema: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
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

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_data_recycle::extend(&mut reg).unwrap();
    reg.register(Model::new(
        meta(DOC, "rusdoo_test_doc"),
        vec![
            char("name"),
            char("state").default_value(json!("open")),
            Field::new("closed_on", FieldType::Datetime),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        meta(LOG, "rusdoo_test_log"),
        vec![char("name"), Field::new("logged_on", FieldType::Date)],
    ))
    .unwrap();
    reg
}

/// Um banco limpo com os quatro modelos que os testes usam.
async fn fixture(schema: &'static str) -> Option<(Registry, PgPool, MethodRegistry)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = pool(&url, schema);
    let reg = registry();
    for table in [
        "data_recycle_record",
        "data_recycle_model",
        "rusdoo_test_doc",
        "rusdoo_test_log",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [RULE, RECORD, DOC, LOG] {
        reg.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    let mut methods = MethodRegistry::new();
    rusdoo_data_recycle::extend_methods(&mut methods).unwrap();
    Some((reg, pool, methods))
}

/// Chama um método registrado sobre `ids`, como o despacho faria.
async fn call(
    reg: &Registry,
    pool: &PgPool,
    methods: &MethodRegistry,
    model: &str,
    name: &str,
    ids: Vec<i64>,
) -> Result<Value, RusdooError> {
    let method = methods
        .get(model, name)
        .unwrap_or_else(|| panic!("{model}.{name} precisa estar registrado"));
    let ctx = MethodCtx::new(reg, pool, 1, model, ids);
    (method.func)(ctx, &[], &Map::new()).await
}

/// Um documento fechado há `days_ago` dias.
async fn a_doc(reg: &Registry, pool: &PgPool, name: &str, state: &str, days_ago: i64) -> i64 {
    let closed = chrono::Utc::now().naive_utc() - chrono::Duration::days(days_ago);
    reg.create(
        pool,
        DOC,
        vec![
            ("name", json!(name)),
            ("state", json!(state)),
            (
                "closed_on",
                json!(closed.format("%Y-%m-%d %H:%M:%S").to_string()),
            ),
        ],
    )
    .await
    .unwrap()
}

/// A regra que os testes variam: cancelados fechados há mais de 6 meses.
async fn a_rule(reg: &Registry, pool: &PgPool, mode: &str, action: &str) -> i64 {
    reg.create(
        pool,
        RULE,
        vec![
            ("name", json!("Documentos cancelados antigos")),
            ("res_model_name", json!(DOC)),
            ("recycle_mode", json!(mode)),
            ("recycle_action", json!(action)),
            ("domain", json!(r#"[["state", "=", "cancel"]]"#)),
            ("time_field_name", json!("closed_on")),
            ("time_field_delta", json!(6)),
            ("time_field_delta_unit", json!("months")),
        ],
    )
    .await
    .unwrap()
}

/// As sugestões abertas de uma regra, mais novas primeiro.
async fn suggestions(reg: &Registry, pool: &PgPool, rule: i64) -> Vec<Map<String, Value>> {
    let ids = reg
        .search(
            pool,
            RECORD,
            &parse_domain(&json!([["recycle_model_id", "=", rule]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    if ids.is_empty() {
        return Vec::new();
    }
    reg.read(pool, RECORD, &ids, &["name", "res_id", "res_model_name"])
        .await
        .unwrap()
}

#[tokio::test]
async fn a_varredura_separa_so_o_que_a_regra_descreve_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_scan").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let rule = a_rule(&reg, &pool, "manual", "unlink").await;
    let old_cancelled = a_doc(&reg, &pool, "Velho e cancelado", "cancel", 400).await;
    a_doc(&reg, &pool, "Novo e cancelado", "cancel", 10).await;
    a_doc(&reg, &pool, "Velho e aberto", "open", 400).await;

    let answer = call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    // numa regra manual a resposta é a tela do que apareceu
    assert_eq!(answer["type"], "ir.actions.act_window", "{answer}");
    assert_eq!(answer["res_model"], RECORD);

    let found = suggestions(&reg, &pool, rule).await;
    assert_eq!(found.len(), 1, "só o velho e cancelado casa: {found:?}");
    assert_eq!(found[0]["res_id"], json!(old_cancelled));
    assert_eq!(found[0]["res_model_name"], DOC);
    // o nome do alvo é gravado na varredura, não inventado depois
    assert_eq!(found[0]["name"], "Velho e cancelado");

    // e nada foi apagado: separar não é aplicar
    let alive = reg
        .search(&pool, DOC, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(alive.len(), 3, "a regra é manual");

    // varrer de novo não duplica a sugestão
    call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    assert_eq!(suggestions(&reg, &pool, rule).await.len(), 1);
}

#[tokio::test]
async fn validar_apaga_o_alvo_e_a_sugestao_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_validate").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let rule = a_rule(&reg, &pool, "manual", "unlink").await;
    let doomed = a_doc(&reg, &pool, "Vai embora", "cancel", 400).await;
    let kept = a_doc(&reg, &pool, "Fica", "open", 400).await;
    call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    let found = suggestions(&reg, &pool, rule).await;
    let suggestion = found[0]["id"].as_i64().unwrap();

    call(
        &reg,
        &pool,
        &methods,
        RECORD,
        "action_validate",
        vec![suggestion],
    )
    .await
    .unwrap();

    let alive = reg
        .search(&pool, DOC, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(alive, vec![kept], "só o alvo da sugestão sai");
    // e a sugestão some junto: ela existia para essa decisão
    assert!(suggestions(&reg, &pool, rule).await.is_empty());
    assert!(reg
        .read(&pool, DOC, &[doomed], &["name"])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn descartar_tira_o_registro_da_fila_para_sempre_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_discard").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let rule = a_rule(&reg, &pool, "manual", "unlink").await;
    a_doc(&reg, &pool, "Esse fica", "cancel", 400).await;
    call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    let suggestion = suggestions(&reg, &pool, rule).await[0]["id"]
        .as_i64()
        .unwrap();

    call(
        &reg,
        &pool,
        &methods,
        RECORD,
        "action_discard",
        vec![suggestion],
    )
    .await
    .unwrap();
    assert!(
        suggestions(&reg, &pool, rule).await.is_empty(),
        "saiu da fila"
    );

    // a varredura seguinte não ressuscita a pergunta já respondida
    call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    assert!(suggestions(&reg, &pool, rule).await.is_empty());

    // o contador da regra também ignora o descartado
    let rows = reg
        .read(&pool, RULE, &[rule], &["records_to_recycle_count"])
        .await
        .unwrap();
    assert_eq!(rows[0]["records_to_recycle_count"], json!(0));
}

#[tokio::test]
async fn o_modo_automatico_arquiva_sem_perguntar_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_auto").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let rule = a_rule(&reg, &pool, "automatic", "archive").await;
    let old = a_doc(&reg, &pool, "Velho", "cancel", 400).await;
    let fresh = a_doc(&reg, &pool, "Novo", "cancel", 10).await;

    let answer = call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    // sem nada para revisar, a resposta é quantos foram tratados
    assert_eq!(answer, json!(1), "{answer}");

    // o velho saiu de circulação sem ser apagado
    let visible = reg
        .search(&pool, DOC, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(visible, vec![fresh]);
    let rows = reg.read(&pool, DOC, &[old], &["active"]).await.unwrap();
    assert_eq!(rows[0]["active"], json!(false), "arquivado, não apagado");
    // e a fila fica vazia: no automático não há o que revisar
    assert!(suggestions(&reg, &pool, rule).await.is_empty());
}

#[tokio::test]
async fn o_cron_roda_as_regras_ativas_e_ignora_a_que_quebra_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_cron").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let good = a_rule(&reg, &pool, "automatic", "archive").await;
    a_doc(&reg, &pool, "Velho", "cancel", 400).await;
    // uma regra que aponta um modelo que ninguém registrou
    let broken = reg
        .create(
            &pool,
            RULE,
            vec![
                ("name", json!("Regra quebrada")),
                ("res_model_name", json!("nao.existe")),
                ("domain", json!(r#"[["state", "=", "cancel"]]"#)),
            ],
        )
        .await
        .unwrap();
    // e uma desativada, que o cron não deve tocar
    let idle = a_rule(&reg, &pool, "manual", "unlink").await;
    reg.write(&pool, RULE, &[idle], vec![("active", json!(false))])
        .await
        .unwrap();

    let answer = call(&reg, &pool, &methods, RULE, "cron_recycle_records", vec![])
        .await
        .unwrap();
    assert_eq!(answer["scanned"], json!(1), "{answer}");
    assert_eq!(answer["applied"], json!(1), "{answer}");
    // a regra quebrada não impediu a boa de rodar, e não deixou fila
    assert!(suggestions(&reg, &pool, broken).await.is_empty());
    assert!(suggestions(&reg, &pool, good).await.is_empty());
    assert!(suggestions(&reg, &pool, idle).await.is_empty());
}

#[tokio::test]
async fn arquivar_o_que_nao_arquiva_e_recusado_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_archive").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    reg.create(&pool, LOG, vec![("name", json!("Uma linha de log"))])
        .await
        .unwrap();
    let rule = reg
        .create(
            &pool,
            RULE,
            vec![
                ("name", json!("Logs velhos")),
                ("res_model_name", json!(LOG)),
                ("recycle_action", json!("archive")),
                ("domain", json!(r#"[["name", "!=", ""]]"#)),
            ],
        )
        .await
        .unwrap();

    let error = call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .expect_err("o modelo não tem como ser arquivado");
    assert!(
        error.to_string().contains("mude a ação"),
        "a mensagem diz o que fazer: {error}"
    );
}

#[tokio::test]
async fn uma_regra_perigosa_ou_mal_escrita_nao_e_salva_live() {
    let Some((reg, pool, _methods)) = fixture("rusdoo_data_recycle_rules").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // sem filtro e sem idade: casaria com a tabela inteira
    let error = reg
        .create(
            &pool,
            RULE,
            vec![("name", json!("Tudo")), ("res_model_name", json!(DOC))],
        )
        .await
        .expect_err("uma regra que pega tudo");
    assert!(error.to_string().contains("todos os registros"), "{error}");

    // um filtro que não é domínio
    let error = reg
        .create(
            &pool,
            RULE,
            vec![
                ("name", json!("Torto")),
                ("res_model_name", json!(DOC)),
                ("domain", json!("state = cancel")),
            ],
        )
        .await
        .expect_err("isso não é um domínio");
    assert!(error.to_string().contains("lista JSON"), "{error}");

    // idade de zero é "tudo" escrito de outro jeito
    let error = reg
        .create(
            &pool,
            RULE,
            vec![
                ("name", json!("Idade zero")),
                ("res_model_name", json!(DOC)),
                ("time_field_name", json!("closed_on")),
                ("time_field_delta", json!(0)),
            ],
        )
        .await
        .expect_err("zero não é intervalo");
    assert!(error.to_string().contains("maior que zero"), "{error}");

    // e nenhuma delas sobreviveu à recusa
    let saved = reg
        .search(&pool, RULE, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert!(saved.is_empty(), "nada ficou pela metade");
}

#[tokio::test]
async fn um_alvo_que_sumiu_sozinho_nao_derruba_a_validacao_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_data_recycle_gone").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let rule = a_rule(&reg, &pool, "manual", "unlink").await;
    let doc = a_doc(&reg, &pool, "Some antes", "cancel", 400).await;
    call(
        &reg,
        &pool,
        &methods,
        RULE,
        "action_recycle_records",
        vec![rule],
    )
    .await
    .unwrap();
    let suggestion = suggestions(&reg, &pool, rule).await[0]["id"]
        .as_i64()
        .unwrap();
    // alguém apagou o documento entre a varredura e a validação
    reg.get(DOC).unwrap().unlink(&pool, &[doc]).await.unwrap();

    call(
        &reg,
        &pool,
        &methods,
        RECORD,
        "action_validate",
        vec![suggestion],
    )
    .await
    .expect("validar o que já sumiu é trabalho feito");
    assert!(suggestions(&reg, &pool, rule).await.is_empty());
}
