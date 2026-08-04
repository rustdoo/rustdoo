//! What a model does because it was written: the create and write hooks,
//! and the cycle they are not allowed to become.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::hooks::{HookCtx, MAX_HOOK_DEPTH};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.into(),
        table: table.into(),
        inherit: vec![],
        inherits: vec![],
    }
}

type Answer<'a> = Pin<Box<dyn Future<Output = Result<(), RusdooError>> + Send + 'a>>;

/// A meeting's hook, in miniature: writing the guest list creates a row
/// per guest, so a client that only knows the field still gets them.
fn make_the_guests<'a>(ctx: HookCtx<'a>) -> Answer<'a> {
    Box::pin(async move {
        if !ctx.wrote("guests") {
            return Ok(());
        }
        let names: Vec<String> = ctx
            .value("guests")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        let ids = ctx.ids.to_vec();
        for id in ids {
            for name in &names {
                ctx.registry
                    .create_tx(
                        ctx.tx,
                        "rusdoo.test.guest",
                        vec![("name", json!(name)), ("meeting_id", json!(id))],
                    )
                    .await?;
            }
        }
        Ok(())
    })
}

/// A hook that reads the record it is reacting to — including what the
/// write just did.
fn stamp_the_state<'a>(mut ctx: HookCtx<'a>) -> Answer<'a> {
    Box::pin(async move {
        if !ctx.wrote("state") {
            return Ok(());
        }
        let rows = ctx.records(&["state"]).await?;
        let state = rows
            .first()
            .and_then(|row| row.get("state"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ids = ctx.ids.to_vec();
        ctx.registry
            .write_tx(
                ctx.tx,
                "rusdoo.test.meeting",
                &ids,
                vec![("last_state", json!(state))],
            )
            .await?;
        Ok(())
    })
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(
        Model::new(
            meta("rusdoo.test.meeting", "rusdoo_test_hook_meeting"),
            vec![
                Field::new("name", FieldType::Char { size: None }).required(),
                Field::new("guests", FieldType::Char { size: None }),
                Field::new("state", FieldType::Char { size: None }),
                Field::new("last_state", FieldType::Char { size: None }),
                Field::new(
                    "guest_ids",
                    FieldType::One2many {
                        comodel: "rusdoo.test.guest".into(),
                        inverse: "meeting_id".into(),
                    },
                ),
            ],
        )
        .on_create("the guest list", make_the_guests)
        .on_write("the guest list", make_the_guests)
        .on_write("stamp the state", stamp_the_state),
    )
    .unwrap();
    reg.register(Model::new(
        meta("rusdoo.test.guest", "rusdoo_test_hook_guest"),
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new(
                "meeting_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.meeting".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

async fn guests_of(reg: &Registry, pool: &sqlx::PgPool, meeting: i64) -> Vec<String> {
    let ids = reg
        .search(
            pool,
            "rusdoo.test.guest",
            &parse_domain(&json!([["meeting_id", "=", meeting]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    let mut names: Vec<String> = reg
        .read(pool, "rusdoo.test.guest", &ids, &["name"])
        .await
        .expect("the guests read")
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn writing_a_field_makes_the_records_it_implies_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_hooks") else {
        return;
    };
    let reg = registry();
    for table in ["rusdoo_test_hook_guest", "rusdoo_test_hook_meeting"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.init_tables(&pool).await.unwrap();

    // the create: a client that only knows the field still gets the rows
    let meeting = reg
        .create(
            &pool,
            "rusdoo.test.meeting",
            vec![
                ("name", json!("Revisão")),
                ("guests", json!("Ana, Bruno")),
            ],
        )
        .await
        .expect("the meeting saves");
    assert_eq!(guests_of(&reg, &pool, meeting).await, ["Ana", "Bruno"]);

    // the write: the same hook, and it runs only for the field it cares
    // about
    reg.write(
        &pool,
        "rusdoo.test.meeting",
        &[meeting],
        vec![("name", json!("Revisão do trimestre"))],
    )
    .await
    .expect("the rename saves");
    assert_eq!(
        guests_of(&reg, &pool, meeting).await,
        ["Ana", "Bruno"],
        "um write que não tocou a lista não convidou ninguém de novo"
    );

    reg.write(
        &pool,
        "rusdoo.test.meeting",
        &[meeting],
        vec![("guests", json!("Carla"))],
    )
    .await
    .expect("the guest write saves");
    assert_eq!(guests_of(&reg, &pool, meeting).await, ["Ana", "Bruno", "Carla"]);

    // a hook that reads what the write just did, and writes back to its
    // own model without setting itself off forever
    reg.write(
        &pool,
        "rusdoo.test.meeting",
        &[meeting],
        vec![("state", json!("confirmada"))],
    )
    .await
    .expect("the state saves");
    let rows = reg
        .read(&pool, "rusdoo.test.meeting", &[meeting], &["last_state"])
        .await
        .unwrap();
    assert_eq!(rows[0]["last_state"], json!("confirmada"), "{:?}", rows[0]);

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_hooks CASCADE")
        .execute(&pool)
        .await
        .ok();
}

/// A hook that fails takes the write with it: half a reaction is worse
/// than none.
#[tokio::test]
async fn a_hook_that_refuses_rolls_the_whole_write_back_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_hooks_refuse") else {
        return;
    };
    fn refuse<'a>(_ctx: HookCtx<'a>) -> Answer<'a> {
        Box::pin(async move { Err(RusdooError::Validation("não hoje".into())) })
    }
    let mut reg = Registry::new();
    reg.register(
        Model::new(
            meta("rusdoo.test.refused", "rusdoo_test_hook_refused"),
            vec![Field::new("name", FieldType::Char { size: None }).required()],
        )
        .on_create("recusa", refuse),
    )
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_hook_refused" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    reg.init_tables(&pool).await.unwrap();

    let refused = reg
        .create(
            &pool,
            "rusdoo.test.refused",
            vec![("name", json!("não vai existir"))],
        )
        .await;
    let error = refused.expect_err("o hook recusou e o create passou");
    assert!(error.to_string().contains("não hoje"), "{error}");

    // and the row is not there: the hook ran inside the same transaction
    let left = reg
        .search(
            &pool,
            "rusdoo.test.refused",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(left.is_empty(), "sobrou linha de um create recusado: {left:?}");

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_hooks_refuse CASCADE")
        .execute(&pool)
        .await
        .ok();
}

/// A hook setting off the hook that set it off is a cycle, and the error
/// names the model — which is the only way anybody finds it.
#[tokio::test]
async fn a_cycle_is_refused_by_name_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_hooks_cycle") else {
        return;
    };
    fn write_again<'a>(ctx: HookCtx<'a>) -> Answer<'a> {
        Box::pin(async move {
            let ids = ctx.ids.to_vec();
            // writes the very field it reacts to: the loop somebody
            // writes by accident
            ctx.registry
                .write_tx(ctx.tx, "rusdoo.test.loop", &ids, vec![("spin", json!("de novo"))])
                .await?;
            Ok(())
        })
    }
    let mut reg = Registry::new();
    reg.register(
        Model::new(
            meta("rusdoo.test.loop", "rusdoo_test_hook_loop"),
            vec![
                Field::new("name", FieldType::Char { size: None }).required(),
                Field::new("spin", FieldType::Char { size: None }),
            ],
        )
        .on_write("gira", write_again),
    )
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_hook_loop" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    reg.init_tables(&pool).await.unwrap();
    let id = reg
        .create(&pool, "rusdoo.test.loop", vec![("name", json!("gira"))])
        .await
        .unwrap();

    let error = reg
        .write(&pool, "rusdoo.test.loop", &[id], vec![("spin", json!("começa"))])
        .await
        .expect_err("o ciclo passou");
    let message = error.to_string();
    assert!(message.contains("rusdoo.test.loop"), "{message}");
    assert!(
        message.contains(&MAX_HOOK_DEPTH.to_string()),
        "a mensagem diz o limite: {message}"
    );

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_hooks_cycle CASCADE")
        .execute(&pool)
        .await
        .ok();
}
