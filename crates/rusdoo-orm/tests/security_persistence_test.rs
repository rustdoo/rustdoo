//! The ACL and the record rules are rows: what one boot writes, the
//! next boot reads back. A restart must not silently lose them — that
//! would lock every non-superuser out of a working database.

use rusdoo_orm::access::{AccessControl, Grant, Operation};
use rusdoo_orm::rules::{RecordRules, Rule};
use serde_json::json;

/// A pool pinned to a schema of its own, so the fixed table names
/// (`ir_model_access`, `ir_rule`) cannot collide with another test.
fn pool(url: &str, schema: &'static str) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}") as &str,
                )
                .await?;
                sqlx::Executor::execute(&mut *conn, &format!("SET search_path TO {schema}") as &str)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap()
}

async fn reset(pool: &sqlx::PgPool) {
    for table in ["ir_model_access", "ir_rule"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn grants_survive_a_restart_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = pool(&url, rusdoo_testing::schema_for("rusdoo_acl_persist_test"));
    reset(&pool).await;

    AccessControl::persist_module(
        &pool,
        "base",
        &[
            Grant {
                model: "res.partner".into(),
                group_id: 1,
                operations: vec![Operation::Read],
            },
            Grant {
                model: "res.partner".into(),
                group_id: 2,
                operations: vec![
                    Operation::Read,
                    Operation::Write,
                    Operation::Create,
                    Operation::Unlink,
                ],
            },
        ],
    )
    .await
    .unwrap();

    // a fresh process: nothing but the database
    let access = AccessControl::load(&pool).await.unwrap();
    assert!(access.check("res.partner", Operation::Read, &[1], false).is_ok());
    // group 1 got read only — the other three did not come back granted
    for op in [Operation::Write, Operation::Create, Operation::Unlink] {
        assert!(
            access.check("res.partner", op, &[1], false).is_err(),
            "{op:?} must not be granted to group 1"
        );
    }
    assert!(access
        .check("res.partner", Operation::Unlink, &[2], false)
        .is_ok());
    // and a model nobody granted stays closed
    assert!(access
        .check("res.company", Operation::Read, &[1, 2], false)
        .is_err());
}

#[tokio::test]
async fn reinstalling_a_module_replaces_only_its_own_grants_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = pool(&url, rusdoo_testing::schema_for("rusdoo_acl_replace_test"));
    reset(&pool).await;

    AccessControl::persist_module(
        &pool,
        "base",
        &[Grant {
            model: "res.partner".into(),
            group_id: 1,
            operations: vec![Operation::Read],
        }],
    )
    .await
    .unwrap();
    AccessControl::persist_module(
        &pool,
        "sale",
        &[Grant {
            model: "sale.order".into(),
            group_id: 1,
            operations: vec![Operation::Read],
        }],
    )
    .await
    .unwrap();

    // reinstalling `sale` with a narrower grant revokes its old row and
    // leaves `base` alone
    AccessControl::persist_module(&pool, "sale", &[]).await.unwrap();
    let access = AccessControl::load(&pool).await.unwrap();
    assert!(access
        .check("res.partner", Operation::Read, &[1], false)
        .is_ok());
    assert!(access
        .check("sale.order", Operation::Read, &[1], false)
        .is_err());
}

#[tokio::test]
async fn rules_survive_a_restart_with_their_domain_and_groups_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = pool(&url, rusdoo_testing::schema_for("rusdoo_rule_persist_test"));
    reset(&pool).await;

    RecordRules::persist_module(
        &pool,
        "base",
        &[
            // global: everyone only sees their own records
            Rule {
                model: "res.partner".into(),
                domain: json!([["create_uid", "=", "user.id"]]),
                groups: vec![],
                operations: vec![Operation::Read, Operation::Write],
            },
            // group rule: managers see the whole company
            Rule {
                model: "res.partner".into(),
                domain: json!([["active", "=", true]]),
                groups: vec![2],
                operations: vec![Operation::Read],
            },
        ],
    )
    .await
    .unwrap();

    let rules = RecordRules::load(&pool).await.unwrap();
    assert!(rules.covers("res.partner"));
    assert!(!rules.covers("res.company"));

    // the placeholder is still a placeholder after the round trip: it is
    // resolved per user, not frozen at write time
    let restored = rules.rows();
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].domain, json!([["create_uid", "=", "user.id"]]));
    assert!(restored[0].groups.is_empty());
    assert_eq!(restored[1].groups, vec![2]);
    assert_eq!(restored[1].operations, vec![Operation::Read]);

    // and the domain a user gets still names that user
    let domain = rules
        .domain_for("res.partner", Operation::Read, 7, &[], false)
        .unwrap();
    assert!(domain.is_some(), "a global rule constrains every user");
    // the superuser stays unconstrained
    assert!(rules
        .domain_for("res.partner", Operation::Read, 1, &[], true)
        .unwrap()
        .is_none());
}
