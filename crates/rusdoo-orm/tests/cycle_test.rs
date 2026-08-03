//! A self-referencing model whose data forms a circle.
//!
//! Found by the reviewer of the `analytic` port, and it is the
//! framework's fault, not the addon's: the hop counter that bounds a
//! related or computed read restarted at every hop, so it never reached
//! its ceiling. Two records pointing at each other made the read recurse
//! until the stack ran out, and a stack overflow is not an error a
//! server can answer — the process aborts.
//!
//! Any model with a self-referencing many2one is exposed: an analytic
//! plan's parent, a partner's parent company, a menu's parent. Making
//! the cycle takes two ordinary writes that no constraint refuses.

use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.into(),
        table: table.into(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// A model shaped like `account.analytic.plan`: a parent of its own
/// kind, and a related field that walks it.
fn registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("x.node", "rusdoo_test_cycle_node"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "x.node".into(),
                },
            ),
            Field::new("parent_name", FieldType::Char { size: None }).related("parent_id.name"),
            // the shape that recursed, and the shape an analytic plan
            // really has: a related field that reads *itself* on the
            // parent, so it walks the chain to its top. On a chain that
            // ends it stops there; on a circle it never does.
            Field::new("root_name", FieldType::Char { size: None })
                .related("parent_id.root_name"),
        ],
    ))
    .unwrap();
    reg
}

async fn fixture(case: &str) -> Option<(Registry, PgPool)> {
    let pool = rusdoo_testing::pool_in(case)?;
    let reg = registry();
    reg.init_tables(&pool).await.expect("the table is made");
    Some((reg, pool))
}

#[tokio::test]
async fn a_chain_that_ends_is_read_to_its_end_live() {
    let Some((reg, pool)) = fixture("rusdoo_cycle_chain").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // grandparent <- parent <- child, no circle
    let grandparent = reg
        .create(&pool, "x.node", vec![("name", json!("Root"))])
        .await
        .unwrap();
    let parent = reg
        .create(
            &pool,
            "x.node",
            vec![("name", json!("Middle")), ("parent_id", json!(grandparent))],
        )
        .await
        .unwrap();
    let child = reg
        .create(
            &pool,
            "x.node",
            vec![("name", json!("Leaf")), ("parent_id", json!(parent))],
        )
        .await
        .unwrap();

    let rows = reg
        .read(&pool, "x.node", &[child], &["name", "parent_name", "root_name"])
        .await
        .expect("a finite chain reads");
    assert_eq!(rows[0]["name"], json!("Leaf"));
    assert_eq!(rows[0]["parent_name"], json!("Middle"));
    // the recursive related walked up and stopped at the top, where
    // there is no parent left
    assert_eq!(rows[0]["root_name"], json!(null));
}

/// The one that used to abort the process.
#[tokio::test]
async fn a_circle_answers_an_error_instead_of_taking_the_server_down_live() {
    let Some((reg, pool)) = fixture("rusdoo_cycle_circle").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let a = reg
        .create(&pool, "x.node", vec![("name", json!("A"))])
        .await
        .unwrap();
    let b = reg
        .create(
            &pool,
            "x.node",
            vec![("name", json!("B")), ("parent_id", json!(a))],
        )
        .await
        .unwrap();
    // the second write closes the circle, and nothing refuses it: a
    // one-hop "not its own parent" check does not see a two-record loop
    reg.write(&pool, "x.node", &[a], vec![("parent_id", json!(b))])
        .await
        .expect("the write itself is allowed");

    let error = reg
        .read(&pool, "x.node", &[a], &["root_name"])
        .await
        .expect_err("reading through the circle is refused, not fatal");
    let message = error.to_string();
    assert!(
        message.contains("hops deep") || message.contains("exceeds"),
        "the refusal says what happened: {message}"
    );

    // and the record is still readable for what does not go round
    let rows = reg
        .read(&pool, "x.node", &[a], &["name"])
        .await
        .expect("a plain column is unaffected");
    assert_eq!(rows[0]["name"], json!("A"));
}

/// A record pointing at itself — the case a one-hop check does catch,
/// kept here so the bound is proven for it too.
#[tokio::test]
async fn a_record_that_is_its_own_parent_is_refused_the_same_way_live() {
    let Some((reg, pool)) = fixture("rusdoo_cycle_self").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let one = reg
        .create(&pool, "x.node", vec![("name", json!("Ouroboros"))])
        .await
        .unwrap();
    reg.write(&pool, "x.node", &[one], vec![("parent_id", json!(one))])
        .await
        .unwrap();

    let error = reg
        .read(&pool, "x.node", &[one], &["root_name"])
        .await
        .expect_err("a self-reference is a circle of one");
    assert!(
        error.to_string().contains("hops deep") || error.to_string().contains("exceeds"),
        "{error}"
    );
}
