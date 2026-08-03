//! A nomenclatura com o banco no meio: as regras gravadas, a ordem em
//! que elas valem, e o que a gravação recusa.

use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Cada teste no seu schema: a suíte roda em paralelo contra o mesmo
/// banco.
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

async fn fixture(schema: &'static str) -> Option<(Registry, PgPool, MethodRegistry)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = pool(&url, schema);
    let mut registry = Registry::new();
    rusdoo_barcodes::extend(&mut registry).unwrap();
    for table in ["barcode_rule", "barcode_nomenclature"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in ["barcode.nomenclature", "barcode.rule"] {
        registry
            .get(model)
            .unwrap()
            .init_table(&pool)
            .await
            .unwrap();
    }
    let mut methods = MethodRegistry::new();
    rusdoo_barcodes::extend_methods(&mut methods).unwrap();
    Some((registry, pool, methods))
}

/// Chama `parse_barcode` como o despacho chamaria: pela tabela de
/// métodos, com o id da nomenclatura em `self`.
async fn read_code(
    registry: &Registry,
    pool: &PgPool,
    methods: &MethodRegistry,
    nomenclature: i64,
    barcode: &str,
) -> Value {
    let method = methods
        .get("barcode.nomenclature", "parse_barcode")
        .expect("o método está registrado");
    assert_eq!(method.operation, Operation::Read, "bipar não muda nada");
    // o código lido é argumento do método, e argumento vive em `rest`:
    // `args[0]` é o conjunto de registros
    let ctx = MethodCtx::new(registry, pool, 1, "barcode.nomenclature", vec![nomenclature])
        .with_rest(vec![json!(barcode)]);
    (method.func)(ctx, &[json!(barcode)], &Map::new())
        .await
        .expect("a leitura respondeu")
}

async fn a_nomenclature(registry: &Registry, pool: &PgPool, name: &str) -> i64 {
    registry
        .create(pool, "barcode.nomenclature", vec![("name", json!(name))])
        .await
        .unwrap()
}

async fn a_rule(
    registry: &Registry,
    pool: &PgPool,
    nomenclature: i64,
    encoding: &str,
    pattern: &str,
    sequence: i64,
) -> Result<i64, rusdoo_core::RusdooError> {
    registry
        .create(
            pool,
            "barcode.rule",
            vec![
                ("name", json!(format!("regra {pattern}"))),
                ("barcode_nomenclature_id", json!(nomenclature)),
                ("encoding", json!(encoding)),
                ("pattern", json!(pattern)),
                ("sequence", json!(sequence)),
            ],
        )
        .await
}

#[tokio::test]
async fn a_scanned_code_comes_back_classified_live() {
    let Some((registry, pool, methods)) = fixture(rusdoo_testing::schema_for("rusdoo_barcodes_parse")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let nomenclature = a_nomenclature(&registry, &pool, "Padrão").await;
    a_rule(
        &registry,
        &pool,
        nomenclature,
        "ean13",
        "1........{NND}.",
        10,
    )
    .await
    .unwrap();

    let parsed = read_code(&registry, &pool, &methods, nomenclature, "1020034051259").await;
    assert_eq!(parsed["type"], "product");
    assert_eq!(parsed["encoding"], "ean13");
    // a balança escreveu 12,5 no código; o que está no produto é o base
    assert_eq!(parsed["value"], json!(12.5));
    assert_eq!(parsed["base_code"], "1020034050009");

    // um código que nenhuma regra reconhece volta como erro, não como
    // produto nenhum: quem bipou precisa saber que não deu
    let parsed = read_code(&registry, &pool, &methods, nomenclature, "9999999999994").await;
    assert_eq!(parsed["type"], "error");
    assert_eq!(parsed["code"], "9999999999994");
}

#[tokio::test]
async fn the_rule_with_the_lower_sequence_wins_live() {
    let Some((registry, pool, methods)) = fixture(rusdoo_testing::schema_for("rusdoo_barcodes_sequence")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let nomenclature = a_nomenclature(&registry, &pool, "Duas regras").await;
    // as duas casam 2212345610259; a ordem de criação é a inversa da
    // ordem em que elas valem, de propósito
    let wide = a_rule(
        &registry,
        &pool,
        nomenclature,
        "ean13",
        ".....{NNNDDDD}.",
        3,
    )
    .await
    .unwrap();
    a_rule(
        &registry,
        &pool,
        nomenclature,
        "ean13",
        "22......{NNDD}.",
        2,
    )
    .await
    .unwrap();

    let parsed = read_code(&registry, &pool, &methods, nomenclature, "2212345610259").await;
    assert_eq!(parsed["value"], json!(10.25), "venceu a de sequence 2");
    assert_eq!(parsed["base_code"], "2212345600007");

    // trocando a ordem, a outra regra passa a valer para o mesmo código
    registry
        .write(&pool, "barcode.rule", &[wide], vec![("sequence", json!(1))])
        .await
        .unwrap();
    let parsed = read_code(&registry, &pool, &methods, nomenclature, "2212345610259").await;
    assert_eq!(parsed["value"], json!(456.1025));
    assert_eq!(parsed["base_code"], "2212300000002");
}

#[tokio::test]
async fn a_rule_the_server_could_not_apply_is_never_written_live() {
    let Some((registry, pool, _methods)) = fixture(rusdoo_testing::schema_for("rusdoo_barcodes_pattern")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let nomenclature = a_nomenclature(&registry, &pool, "Recusas").await;

    for (pattern, reason) in [
        ("......{}..", "chaves vazias"),
        ("......{DN}", "N seguido de D"),
        ("....{NN}{DD}", "mais de um par"),
        ("*", "'.*'"),
        ("**>>>{ND}", "expressão regular"),
    ] {
        let error = a_rule(&registry, &pool, nomenclature, "ean8", pattern, 10)
            .await
            .expect_err(&format!("{pattern} devia ser recusado"));
        assert!(
            error.to_string().contains(reason),
            "o motivo chega a quem escreveu a regra: {error}"
        );
    }
    // e nenhuma delas ficou no banco
    let count: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "barcode_rule""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "uma regra recusada não sobrevive");

    // a regra boa passa, e editar o padrão para um quebrado também é
    // recusado — a constraint vale na gravação, não só na criação
    let good = a_rule(&registry, &pool, nomenclature, "ean8", "..>>>{ND}", 10)
        .await
        .unwrap();
    let error = registry
        .write(
            &pool,
            "barcode.rule",
            &[good],
            vec![("pattern", json!("*"))],
        )
        .await
        .expect_err("editar para um padrão quebrado é recusado");
    assert!(error.to_string().contains("'.*'"), "{error}");
}

#[tokio::test]
async fn an_rfid_uri_answers_with_the_product_and_the_lot_live() {
    let Some((registry, pool, methods)) = fixture(rusdoo_testing::schema_for("rusdoo_barcodes_uri")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // sem regra nenhuma: uma URI não passa por elas
    let nomenclature = a_nomenclature(&registry, &pool, "RFID").await;

    let parsed = read_code(
        &registry,
        &pool,
        &methods,
        nomenclature,
        "urn:epc:id:sgtin:9521141.012345.4711",
    )
    .await;
    let parts = parsed.as_array().expect("uma URI traz mais de uma coisa");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "product");
    assert_eq!(parts[0]["value"], "09521141123454");
    assert_eq!(parts[1]["type"], "lot");
    assert_eq!(parts[1]["value"], "4711");
}
