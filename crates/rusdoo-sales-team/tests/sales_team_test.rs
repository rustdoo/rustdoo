//! Membership as the module enforces it: joining a team, leaving the
//! previous one, and what the models refuse outright.

use rusdoo_orm::methods::MethodCtx;
use rusdoo_orm::registry::Registry;
use std::sync::Arc;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Each test in its own schema, so the suite is safe in parallel.
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

async fn fixture(url: &str, schema: &'static str) -> (Arc<Registry>, PgPool) {
    let pool = pool(url, schema);
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_sales_team::extend(&mut registry).unwrap();
    for table in [
        "crm_team_member",
        "crm_team",
        "crm_tag",
        "team_favorite_user_rel",
        "res_users",
        "res_partner",
        "res_company",
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
        "crm.team",
        "crm.team.member",
        "crm.tag",
    ] {
        registry
            .get(model)
            .unwrap()
            .init_table(&pool)
            .await
            .unwrap();
    }
    (Arc::new(registry), pool)
}

/// Call a registered method the way the dispatch does.
async fn call(
    registry: &Arc<Registry>,
    pool: &PgPool,
    uid: i64,
    model: &str,
    name: &str,
    ids: &[i64],
    kwargs: Value,
) -> Result<Value, rusdoo_core::RusdooError> {
    let mut methods = rusdoo_orm::methods::MethodRegistry::new();
    rusdoo_sales_team::extend_methods(&mut methods).unwrap();
    let method = methods.get(model, name).expect("a registered method");
    let kwargs: Map<String, Value> = match kwargs {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let ctx = MethodCtx::new(Arc::clone(registry), pool, uid, model, ids.to_vec());
    method.call(ctx, &[], &kwargs).await
}

async fn a_user(registry: &Arc<Registry>, pool: &PgPool, login: &str, name: &str) -> i64 {
    registry
        .create(
            pool,
            "res.users",
            vec![
                ("login", json!(login)),
                ("name", json!(name)),
                ("email", json!(format!("{login}@exemplo.com"))),
            ],
        )
        .await
        .unwrap()
}

async fn a_team(registry: &Arc<Registry>, pool: &PgPool, name: &str) -> i64 {
    registry
        .create(pool, "crm.team", vec![("name", json!(name))])
        .await
        .unwrap()
}

async fn favourites_of(registry: &Arc<Registry>, pool: &PgPool, team: i64) -> Vec<i64> {
    let rows = registry
        .read(pool, "crm.team", &[team], &["favorite_user_ids"])
        .await
        .unwrap();
    rows[0]["favorite_user_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

async fn member_count(registry: &Arc<Registry>, pool: &PgPool, team: i64) -> i64 {
    registry
        .read(pool, "crm.team", &[team], &["member_count"])
        .await
        .unwrap()[0]["member_count"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn a_salesperson_belongs_to_one_team_at_a_time_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_membership")).await;
    let north = a_team(&registry, &pool, "Norte").await;
    let south = a_team(&registry, &pool, "Sul").await;
    let ana = a_user(&registry, &pool, "ana", "Ana").await;

    call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[north],
        json!({"user_id": ana}),
    )
    .await
    .expect("Ana joins the north team");
    assert_eq!(member_count(&registry, &pool, north).await, 1);

    // joining the other team is leaving this one: two pipelines for one
    // salesperson is a report nobody can read
    call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[south],
        json!({"user_id": ana}),
    )
    .await
    .expect("Ana moves to the south team");
    assert_eq!(member_count(&registry, &pool, north).await, 0);
    assert_eq!(member_count(&registry, &pool, south).await, 1);

    // and the membership that was left behind is gone, not orphaned
    let leftovers = registry
        .search(
            &pool,
            "crm.team.member",
            &rusdoo_orm::domain::parse_domain(&json!([["user_id", "=", ana]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(leftovers.len(), 1, "one person, one membership");
}

/// Exclusivity is the default, not a law: whoever turned Odoo's
/// Odoo pediu por uma pessoa em duas equipes.
#[tokio::test]
async fn the_membership_multi_parameter_lets_a_salesperson_serve_two_teams_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_multi")).await;
    registry
        .get("ir.config_parameter")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    registry
        .set_param(&pool, "sales_team.membership_multi", "True")
        .await
        .unwrap();

    let north = a_team(&registry, &pool, "Norte").await;
    let south = a_team(&registry, &pool, "Sul").await;
    let ana = a_user(&registry, &pool, "ana", "Ana").await;

    for team in [north, south] {
        call(
            &registry,
            &pool,
            1,
            "crm.team",
            "action_add_member",
            &[team],
            json!({"user_id": ana}),
        )
        .await
        .expect("com o parâmetro ligado, entrar numa equipe não é sair da outra");
    }
    assert_eq!(member_count(&registry, &pool, north).await, 1);
    assert_eq!(member_count(&registry, &pool, south).await, 1);

    // e entrar duas vezes na mesma equipe continua sendo recusado
    let error = call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[north],
        json!({"user_id": ana}),
    )
    .await
    .expect_err("duas vezes na mesma equipe não");
    assert!(error.to_string().contains("is already in team"), "{error}");
}

#[tokio::test]
async fn adding_the_same_salesperson_twice_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_duplicate")).await;
    let team = a_team(&registry, &pool, "Norte").await;
    let beto = a_user(&registry, &pool, "beto", "Beto").await;

    call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[team],
        json!({"user_id": beto}),
    )
    .await
    .unwrap();
    let error = call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[team],
        json!({"user_id": beto}),
    )
    .await
    .expect_err("he is already there");
    assert!(
        error.to_string().contains("is already in team Norte"),
        "the message names the person and the team: {error}"
    );
    assert_eq!(member_count(&registry, &pool, team).await, 1);
}

#[tokio::test]
async fn removing_a_salesperson_who_is_not_in_the_team_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_removal")).await;
    let north = a_team(&registry, &pool, "Norte").await;
    let south = a_team(&registry, &pool, "Sul").await;
    let ana = a_user(&registry, &pool, "ana", "Ana").await;
    call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[north],
        json!({"user_id": ana}),
    )
    .await
    .unwrap();

    let error = call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_remove_member",
        &[south],
        json!({"user_id": ana}),
    )
    .await
    .expect_err("she is in the other team");
    assert!(
        error.to_string().contains("is not in team Sul"),
        "{error}"
    );
    assert_eq!(
        member_count(&registry, &pool, north).await,
        1,
        "nothing moved"
    );

    call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_remove_member",
        &[north],
        json!({"user_id": ana}),
    )
    .await
    .expect("she does leave the team she is in");
    assert_eq!(member_count(&registry, &pool, north).await, 0);
}

#[tokio::test]
async fn a_membership_carries_the_salespersons_details_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_details")).await;
    let team = a_team(&registry, &pool, "Norte").await;
    let ana = a_user(&registry, &pool, "ana", "Ana").await;
    let membership = call(
        &registry,
        &pool,
        1,
        "crm.team",
        "action_add_member",
        &[team],
        json!({"user_id": ana}),
    )
    .await
    .unwrap()
    .as_i64()
    .expect("the new membership id");

    // the member list shows who it is without a second lookup: the
    // fields mirror the user
    let rows = registry
        .read(&pool, "crm.team.member", &[membership], &["name", "email"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], "Ana");
    assert_eq!(rows[0]["email"], "ana@exemplo.com");
}

#[tokio::test]
async fn a_favourite_team_is_a_link_the_caller_toggles_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_favourite")).await;
    let team = a_team(&registry, &pool, "Norte").await;
    let ana = a_user(&registry, &pool, "ana", "Ana").await;
    let beto = a_user(&registry, &pool, "beto", "Beto").await;

    let pinned = call(
        &registry,
        &pool,
        ana,
        "crm.team",
        "action_toggle_favorite",
        &[team],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(pinned, json!(true));
    call(
        &registry,
        &pool,
        beto,
        "crm.team",
        "action_toggle_favorite",
        &[team],
        json!({}),
    )
    .await
    .unwrap();
    let mut current = favourites_of(&registry, &pool, team).await;
    current.sort_unstable();
    let mut expected = vec![ana, beto];
    expected.sort_unstable();
    assert_eq!(current, expected);

    // pressing it again unpins it — and only for who pressed
    let pinned = call(
        &registry,
        &pool,
        ana,
        "crm.team",
        "action_toggle_favorite",
        &[team],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(pinned, json!(false));
    assert_eq!(favourites_of(&registry, &pool, team).await, vec![beto]);
}

#[tokio::test]
async fn a_team_without_a_name_or_with_an_impossible_colour_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (registry, pool) = fixture(&url, rusdoo_testing::schema_for("rusdoo_sales_team_constraints")).await;

    // a blank name passes NOT NULL and still leaves an unusable card
    let error = registry
        .create(&pool, "crm.team", vec![("name", json!("   "))])
        .await
        .expect_err("a nameless team is refused");
    assert!(error.to_string().contains("give the team or the tag a name"), "{error}");

    let error = registry
        .create(
            &pool,
            "crm.tag",
            vec![("name", json!("Services")), ("color", json!(57))],
        )
        .await
        .expect_err("57 is not on the board");
    assert!(error.to_string().contains("from 0 to 11"), "{error}");

    // and nothing was left behind by the refused creates
    let teams = registry
        .search(
            &pool,
            "crm.team",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(teams.is_empty(), "a refused record must not survive");

    // renaming a team to blank is refused too, not only creating one
    let team = a_team(&registry, &pool, "Norte").await;
    let error = registry
        .write(&pool, "crm.team", &[team], vec![("name", json!(""))])
        .await
        .expect_err("the rule applies to a write as well");
    assert!(error.to_string().contains("give the team or the tag a name"), "{error}");
}
