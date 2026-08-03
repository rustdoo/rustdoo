//! Analytics against a real database: the plan tree as the ORM walks it,
//! the balance of an account, which plans a document is offered, and the
//! distribution the standing rules prefill.
//!
//! Every case builds its own schema, so two of them — and two runs of the
//! suite — never share a table.

use rusdoo_analytic::distribution::{parse_distribution, Distribution};
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use std::sync::Arc;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// A registry with base and analytics in it, and every table created.
///
/// `None` when there is no test database configured: the case returns and
/// the runner shows it as passed-because-skipped, with a notice.
async fn fixture(case: &str) -> Option<(Arc<Registry>, PgPool)> {
    let Some(pool) = rusdoo_testing::pool_in(&format!("rusdoo_analytic_{case}")) else {
        eprintln!("skipped: {} not set", rusdoo_testing::DATABASE_ENV);
        return None;
    };
    let mut registry = rusdoo_base::registry().expect("the base registry");
    rusdoo_analytic::extend(&mut registry).expect("the analytic models");
    registry
        .init_tables(&pool)
        .await
        .expect("creating the models' tables");
    // the superuser every call is made as, like a real boot leaves it
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true)
           ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("creating the case's superuser");
    Some((Arc::new(registry), pool))
}

/// Call a registered method the way the dispatch does.
async fn call(
    registry: &Arc<Registry>,
    pool: &PgPool,
    model: &str,
    name: &str,
    ids: &[i64],
    kwargs: Value,
) -> Result<Value, rusdoo_core::RusdooError> {
    let mut methods = MethodRegistry::new();
    rusdoo_analytic::extend_methods(&mut methods).expect("the analytic methods");
    let method = methods.get(model, name).expect("a registered method");
    let kwargs: Map<String, Value> = match kwargs {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let ctx = MethodCtx::new(Arc::clone(registry), pool, 1, model, ids.to_vec());
    method.call(ctx, &[], &kwargs).await
}

async fn a_plan(registry: &Arc<Registry>, pool: &PgPool, name: &str, parent: Option<i64>) -> i64 {
    let mut values = vec![("name", json!(name))];
    if let Some(parent) = parent {
        values.push(("parent_id", json!(parent)));
    }
    registry
        .create(pool, "account.analytic.plan", values)
        .await
        .unwrap_or_else(|error| panic!("creating plan {name}: {error}"))
}

async fn an_account(registry: &Arc<Registry>, pool: &PgPool, name: &str, plan: i64) -> i64 {
    registry
        .create(
            pool,
            "account.analytic.account",
            vec![("name", json!(name)), ("plan_id", json!(plan))],
        )
        .await
        .unwrap_or_else(|error| panic!("creating account {name}: {error}"))
}

async fn a_company(registry: &Arc<Registry>, pool: &PgPool, name: &str) -> i64 {
    registry
        .create(pool, "res.company", vec![("name", json!(name))])
        .await
        .unwrap()
}

async fn a_line(
    registry: &Arc<Registry>,
    pool: &PgPool,
    account: i64,
    company: i64,
    amount: f64,
) -> i64 {
    registry
        .create(
            pool,
            "account.analytic.line",
            vec![
                ("name", json!("posting")),
                ("account_id", json!(account)),
                ("company_id", json!(company)),
                ("amount", json!(amount)),
            ],
        )
        .await
        .unwrap()
}

async fn field_of(registry: &Arc<Registry>, pool: &PgPool, model: &str, id: i64, field: &str) -> Value {
    registry
        .read(pool, model, &[id], &[field])
        .await
        .unwrap_or_else(|error| panic!("reading {model}.{field}: {error}"))
        .first()
        .and_then(|row| row.get(field).cloned())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_subplan_knows_the_axis_it_belongs_to_live() {
    let Some((registry, pool)) = fixture("roots").await else {
        return;
    };
    // Odoo's `test_analytic_plan_account_child`: three generations
    let projects = a_plan(&registry, &pool, "Projects", None).await;
    let internal = a_plan(&registry, &pool, "Internal", Some(projects)).await;
    let research = a_plan(&registry, &pool, "Research", Some(internal)).await;

    for (plan, expected) in [
        (projects, projects),
        (internal, projects),
        (research, projects),
    ] {
        let root = field_of(&registry, &pool, "account.analytic.plan", plan, "root_id").await;
        assert_eq!(
            root.as_array().and_then(|pair| pair.first()),
            Some(&json!(expected)),
            "every plan of the tree answers the plan at its top"
        );
    }
    // and the name says the whole path, however deep it is
    let complete = field_of(
        &registry,
        &pool,
        "account.analytic.plan",
        research,
        "complete_name",
    )
    .await;
    assert_eq!(complete, json!("Projects / Internal / Research"));

    // an account filed under the third generation still belongs to the
    // axis at the top
    let account = an_account(&registry, &pool, "Chatter rewrite", research).await;
    let root_plan = field_of(
        &registry,
        &pool,
        "account.analytic.account",
        account,
        "root_plan_id",
    )
    .await;
    assert_eq!(
        root_plan.as_array().and_then(|pair| pair.first()),
        Some(&json!(projects))
    );
}

#[tokio::test]
async fn a_plan_counts_the_accounts_of_everything_beneath_it_live() {
    let Some((registry, pool)) = fixture("counts").await else {
        return;
    };
    // Odoo's `test_all_account_count_with_subplans`
    let parent = a_plan(&registry, &pool, "Parent Plan", None).await;
    let sub = a_plan(&registry, &pool, "Sub Plan", Some(parent)).await;
    let sub_sub = a_plan(&registry, &pool, "Sub Sub Plan", Some(sub)).await;
    an_account(&registry, &pool, "Account", parent).await;
    an_account(&registry, &pool, "Child Account", sub).await;
    an_account(&registry, &pool, "Grand Child Account", sub_sub).await;

    for (plan, expected) in [(parent, 3), (sub, 2), (sub_sub, 1)] {
        let counted = field_of(
            &registry,
            &pool,
            "account.analytic.plan",
            plan,
            "all_account_count",
        )
        .await;
        assert_eq!(counted, json!(expected));
    }
    // its own accounts are counted apart from the subtree's
    let own = field_of(
        &registry,
        &pool,
        "account.analytic.plan",
        parent,
        "account_count",
    )
    .await;
    assert_eq!(own, json!(1));
    let children = field_of(
        &registry,
        &pool,
        "account.analytic.plan",
        parent,
        "children_count",
    )
    .await;
    assert_eq!(children, json!(1));
}

#[tokio::test]
async fn a_plan_that_is_its_own_parent_is_refused_live() {
    let Some((registry, pool)) = fixture("cycles").await else {
        return;
    };
    let plan = a_plan(&registry, &pool, "Projects", None).await;
    let error = registry
        .write(
            &pool,
            "account.analytic.plan",
            &[plan],
            vec![("parent_id", json!(plan))],
        )
        .await
        .expect_err("a plan cannot be its own parent");
    assert!(error.to_string().contains("its own parent"), "{error}");

    // and a plan with no name is refused too, blank or not
    let error = registry
        .create(&pool, "account.analytic.plan", vec![("name", json!("   "))])
        .await
        .expect_err("a nameless axis is unfindable");
    assert!(error.to_string().contains("give it a name"), "{error}");
}

#[tokio::test]
async fn an_account_shows_what_was_posted_against_it_live() {
    let Some((registry, pool)) = fixture("balance").await else {
        return;
    };
    let plan = a_plan(&registry, &pool, "Projects", None).await;
    let account = an_account(&registry, &pool, "Website redesign", plan).await;
    let company = a_company(&registry, &pool, "Acme").await;
    a_line(&registry, &pool, account, company, 1_000.0).await;
    a_line(&registry, &pool, account, company, -250.0).await;

    let rows = registry
        .read(
            &pool,
            "account.analytic.account",
            &[account],
            &["balance", "debit", "credit", "display_name"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["credit"], json!(1_000.0));
    assert_eq!(rows[0]["debit"], json!(250.0));
    assert_eq!(rows[0]["balance"], json!(750.0));
    // the stored display name was written when the record was created
    assert_eq!(rows[0]["display_name"], json!("Website redesign"));

    // a line must name an account: one that names none is not analytic
    let error = registry
        .create(
            &pool,
            "account.analytic.line",
            vec![
                ("name", json!("orphan")),
                ("company_id", json!(company)),
                ("amount", json!(10.0)),
            ],
        )
        .await
        .expect_err("a line with no account is refused");
    assert!(
        error.to_string().to_lowercase().contains("account_id"),
        "the message names the missing field: {error}"
    );
}

#[tokio::test]
async fn only_the_axes_with_something_to_choose_are_offered_live() {
    let Some((registry, pool)) = fixture("relevant").await else {
        return;
    };
    let projects = a_plan(&registry, &pool, "Projects", None).await;
    let departments = a_plan(&registry, &pool, "Departments", None).await;
    let empty = a_plan(&registry, &pool, "Vehicles", None).await;
    an_account(&registry, &pool, "Website redesign", projects).await;
    an_account(&registry, &pool, "Support", departments).await;

    let offered = call(
        &registry,
        &pool,
        "account.analytic.plan",
        "get_relevant_plans",
        &[],
        json!({}),
    )
    .await
    .unwrap();
    let ids: Vec<i64> = offered
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|plan| plan["id"].as_i64())
        .collect();
    assert_eq!(
        ids,
        vec![projects, departments],
        "an axis with no accounts is a field the user can only leave empty: {offered}"
    );
    assert_eq!(offered[0]["applicability"], json!("optional"));

    // an axis whose default answer is "unavailable" drops out
    registry
        .write(
            &pool,
            "account.analytic.plan",
            &[departments],
            vec![("default_applicability", json!("unavailable"))],
        )
        .await
        .unwrap();
    let offered = call(
        &registry,
        &pool,
        "account.analytic.plan",
        "get_relevant_plans",
        &[],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(offered.as_array().unwrap().len(), 1, "{offered}");

    // ... unless a rule about this business domain says otherwise
    registry
        .create(
            &pool,
            "account.analytic.applicability",
            vec![
                ("analytic_plan_id", json!(departments)),
                ("business_domain", json!("general")),
                ("applicability", json!("mandatory")),
            ],
        )
        .await
        .unwrap();
    let offered = call(
        &registry,
        &pool,
        "account.analytic.plan",
        "get_relevant_plans",
        &[],
        json!({"business_domain": "general"}),
    )
    .await
    .unwrap();
    let by_id: Vec<(i64, String)> = offered
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| {
            (
                plan["id"].as_i64().unwrap(),
                plan["applicability"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        by_id,
        vec![
            (projects, "optional".to_string()),
            (departments, "mandatory".to_string())
        ],
        "the rule wins over the plan's own answer: {offered}"
    );

    // and an axis nobody offers stays on screen while the document is
    // still tagged on it
    let hidden = an_account(&registry, &pool, "Truck", empty).await;
    let offered = call(
        &registry,
        &pool,
        "account.analytic.plan",
        "get_relevant_plans",
        &[],
        json!({"existing_account_ids": [hidden]}),
    )
    .await
    .unwrap();
    assert!(
        offered
            .as_array()
            .unwrap()
            .iter()
            .any(|plan| plan["id"] == json!(empty)),
        "an axis already used must not vanish with the percentage on it: {offered}"
    );
}

#[tokio::test]
async fn the_standing_rules_prefill_a_distribution_live() {
    let Some((registry, pool)) = fixture("distribution").await else {
        return;
    };
    let projects = a_plan(&registry, &pool, "Projects", None).await;
    let departments = a_plan(&registry, &pool, "Departments", None).await;
    let redesign = an_account(&registry, &pool, "Website redesign", projects).await;
    let migration = an_account(&registry, &pool, "Migration", projects).await;
    let support = an_account(&registry, &pool, "Support", departments).await;

    let acme = registry
        .create(&pool, "res.partner", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let company = a_company(&registry, &pool, "Acme Ltd").await;

    // one rule about departments, for this partner in any company
    registry
        .create(
            &pool,
            "account.analytic.distribution.model",
            vec![
                ("partner_id", json!(acme)),
                ("analytic_distribution", json!({ support.to_string(): 100 })),
            ],
        )
        .await
        .unwrap();
    // and one about projects, which sorts first
    registry
        .create(
            &pool,
            "account.analytic.distribution.model",
            vec![
                ("partner_id", json!(acme)),
                ("company_id", json!(company)),
                ("sequence", json!(1)),
                (
                    "analytic_distribution",
                    json!({ redesign.to_string(): 100, migration.to_string(): 100 }),
                ),
            ],
        )
        .await
        .unwrap();

    // a document for nobody in particular matches no rule
    let nothing = call(
        &registry,
        &pool,
        "account.analytic.distribution.model",
        "get_distribution",
        &[],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(nothing, json!({}));

    let prefilled = call(
        &registry,
        &pool,
        "account.analytic.distribution.model",
        "get_distribution",
        &[],
        json!({"partner_id": acme, "company_id": company}),
    )
    .await
    .unwrap();
    let expected: Distribution = parse_distribution(&json!({
        format!("{redesign},{support}"): 50.0,
        format!("{migration},{support}"): 50.0,
        redesign.to_string(): 50.0,
        migration.to_string(): 50.0,
    }))
    .unwrap();
    assert_eq!(
        parse_distribution(&prefilled).unwrap(),
        expected,
        "the projects rule shares the document, and the departments rule \
         takes half of what is left: {prefilled}"
    );
}

#[tokio::test]
async fn a_distribution_nobody_can_read_never_reaches_the_column_live() {
    let Some((registry, pool)) = fixture("shape").await else {
        return;
    };
    let error = registry
        .create(
            &pool,
            "account.analytic.distribution.model",
            vec![("analytic_distribution", json!({"the marketing team": 100}))],
        )
        .await
        .expect_err("a key that is not a list of account ids is refused");
    assert!(
        error.to_string().contains("analytic account ids"),
        "{error}"
    );

    // and the refused record left nothing behind
    let survivors = registry
        .search(
            &pool,
            "account.analytic.distribution.model",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(survivors.is_empty(), "a refused record must not survive");
}

#[tokio::test]
async fn a_mandatory_axis_left_empty_refuses_the_document_live() {
    let Some((registry, pool)) = fixture("mandatory").await else {
        return;
    };
    let projects = a_plan(&registry, &pool, "Projects", None).await;
    let departments = a_plan(&registry, &pool, "Departments", None).await;
    let redesign = an_account(&registry, &pool, "Website redesign", projects).await;
    let support = an_account(&registry, &pool, "Support", departments).await;
    registry
        .write(
            &pool,
            "account.analytic.plan",
            &[departments],
            vec![("default_applicability", json!("mandatory"))],
        )
        .await
        .unwrap();

    // Odoo's `test_validate_deleted_account`: the projects axis alone is
    // not enough while departments is mandatory
    let incomplete = registry
        .create(
            &pool,
            "account.analytic.distribution.model",
            vec![(
                "analytic_distribution",
                json!({ redesign.to_string(): 100 }),
            )],
        )
        .await
        .unwrap();
    let error = call(
        &registry,
        &pool,
        "account.analytic.distribution.model",
        "validate_distribution",
        &[incomplete],
        json!({}),
    )
    .await
    .expect_err("the mandatory axis says nothing");
    assert!(error.to_string().contains("100%"), "{error}");

    // naming both axes on the same slice satisfies it
    registry
        .write(
            &pool,
            "account.analytic.distribution.model",
            &[incomplete],
            vec![(
                "analytic_distribution",
                json!({ format!("{redesign},{support}"): 100 }),
            )],
        )
        .await
        .unwrap();
    call(
        &registry,
        &pool,
        "account.analytic.distribution.model",
        "validate_distribution",
        &[incomplete],
        json!({}),
    )
    .await
    .expect("both axes are fully covered");
}
