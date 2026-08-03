//! A model that gains a field finds its column, on a database that
//! already has data — what makes `_inherit` from another module, and any
//! module upgrade, work outside a fresh install.

use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;

fn meta(table: &str) -> ModelMeta {
    ModelMeta {
        name: "rusdoo.test.upgrade".into(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The model as the first version of a module declared it.
fn before(table: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta(table),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg
}

/// The same model after another module extended it — in place, by
/// `_inherit`, which is how a module adds a field to a model it does
/// not own.
fn after(table: &str, required: bool) -> Registry {
    let mut reg = before(table);
    let extra = Field::new("origem", FieldType::Char { size: None });
    let extra = if required { extra.required() } else { extra };
    reg.register(Model::new(
        ModelMeta {
            inherit: vec!["rusdoo.test.upgrade".into()],
            ..meta(table)
        },
        vec![extra, Field::new("prioridade", FieldType::Integer)],
    ))
    .unwrap();
    reg
}

async fn pool() -> Option<PgPool> {
    // a schema of this run: these tests create tables directly, and
    // without it two runs touch the same ones
    rusdoo_testing::pool_in("rusdoo_schema_upgrad_pool")
}

#[tokio::test]
async fn a_field_added_later_gets_its_column_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let table = "rusdoo_test_upgrade_added";
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();

    // yesterday's schema, with a row in it
    let old = before(table);
    old.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let id = old
        .create(
            &pool,
            "rusdoo.test.upgrade",
            vec![("name", json!("registro antigo"))],
        )
        .await
        .unwrap();

    // today's schema: two more fields, same table
    let new = after(table, false);
    new.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .expect("o upgrade não recusa uma tabela que já existe");

    // the row is still there, and the new fields are readable
    let rows = new
        .read(
            &pool,
            "rusdoo.test.upgrade",
            &[id],
            &["name", "origem", "prioridade"],
        )
        .await
        .expect("os campos novos têm coluna");
    assert_eq!(rows[0]["name"], "registro antigo", "o dado sobreviveu");
    assert_eq!(rows[0]["origem"], json!(null));

    // and they can be written to
    new.write(
        &pool,
        "rusdoo.test.upgrade",
        &[id],
        vec![("origem", json!("importação")), ("prioridade", json!(3))],
    )
    .await
    .unwrap();
    let rows = new
        .read(&pool, "rusdoo.test.upgrade", &[id], &["origem", "prioridade"])
        .await
        .unwrap();
    assert_eq!(rows[0]["origem"], "importação");
    assert_eq!(rows[0]["prioridade"], json!(3));
}

#[tokio::test]
async fn a_required_field_added_over_existing_rows_is_added_anyway_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let table = "rusdoo_test_upgrade_required";
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();

    let old = before(table);
    old.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    old.create(
        &pool,
        "rusdoo.test.upgrade",
        vec![("name", json!("sem origem"))],
    )
    .await
    .unwrap();

    // the new field is required, and the rows that are already there
    // have nothing in it. Refusing the upgrade would be worse than
    // adding the column without the constraint — which is what the log
    // says happened.
    let new = after(table, true);
    new.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .expect("o upgrade continua");

    let nullable: Option<bool> = sqlx::query_scalar(
        "SELECT is_nullable = 'YES' FROM information_schema.columns
         WHERE table_name = $1 AND column_name = 'origem'",
    )
    .bind(table)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        nullable,
        Some(true),
        "a coluna entrou, sem a restrição que as linhas antigas quebrariam"
    );
}

#[tokio::test]
async fn a_required_field_on_an_empty_table_keeps_its_constraint_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let table = "rusdoo_test_upgrade_empty";
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();

    before(table)
        .get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    // nothing was created, so the constraint can still be enforced
    let new = after(table, true);
    new.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let nullable: Option<bool> = sqlx::query_scalar(
        "SELECT is_nullable = 'YES' FROM information_schema.columns
         WHERE table_name = $1 AND column_name = 'origem'",
    )
    .bind(table)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(nullable, Some(false), "numa tabela vazia a restrição vale");

    // and the database is the one refusing an incomplete record
    let error = new
        .create(
            &pool,
            "rusdoo.test.upgrade",
            vec![("name", json!("sem origem"))],
        )
        .await
        .expect_err("um registro sem o campo obrigatório é recusado");
    assert!(
        error.to_string().contains("origem"),
        "unexpected error: {error}"
    );
}

/// A field that becomes translatable converts the column that is
/// already there,
/// em vez de deixar o modelo dizendo `jsonb` e a tabela dizendo `varchar`
/// — which is a server that boots and fails on every read of that
/// field.
#[tokio::test]
async fn a_field_that_becomes_translatable_converts_its_column_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let table = "rusdoo_test_translate_upgrade";
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();

    // ontem: um char comum, com dados dentro
    let mut old = Registry::new();
    old.register(Model::new(
        meta(table),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    old.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let id = old
        .create(
            &pool,
            "rusdoo.test.upgrade",
            vec![("name", json!("Mesa de escritório"))],
        )
        .await
        .unwrap();

    // today: translatable
    let mut new = Registry::new();
    new.register(Model::new(
        meta(table),
        vec![Field::new("name", FieldType::Char { size: None }).translatable()],
    ))
    .unwrap();
    new.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .expect("a conversão acontece no boot");

    let udt: String = sqlx::query_scalar(
        "SELECT udt_name FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = 'name'",
    )
    .bind(table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(udt, "jsonb", "a coluna virou o mapa de idiomas");

    // and the text that was already there became the source value, it
    // did not disappear
    let rows = new
        .read(&pool, "rusdoo.test.upgrade", &[id], &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("Mesa de escritório"));

    // the way back too: a field that stops being translatable
    let mut back = Registry::new();
    back.register(Model::new(
        meta(table),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    back.get("rusdoo.test.upgrade")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let udt: String = sqlx::query_scalar(
        "SELECT udt_name FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = 'name'",
    )
    .bind(table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(udt, "varchar");
    let rows = back
        .read(&pool, "rusdoo.test.upgrade", &[id], &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("Mesa de escritório"), "o texto voltou inteiro");

    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();
}
