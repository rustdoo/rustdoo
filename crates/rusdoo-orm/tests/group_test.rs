//! Grouped reads (`_read_group`): SQL shape, validation of the
//! client-supplied specs, and live GROUP BY results.

use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::group::{Aggregate, GroupBy, GroupOptions};
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

fn sales_registry(table: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("res.country", "rusdoo_test_grp_country"),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("rusdoo.test.sale", table),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("amount", FieldType::Integer),
            Field::new("signed", FieldType::Boolean),
            Field::new("day", FieldType::Date),
            Field::new("moment", FieldType::Datetime),
            Field::new(
                "country_id",
                FieldType::Many2one {
                    comodel: "res.country".into(),
                },
            ),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "res.country".into(),
                    relation: "rusdoo_test_grp_rel".into(),
                    column1: "sale_id".into(),
                    column2: "tag_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

fn parse(reg: &Registry, groupby: &[&str], aggregates: &[&str]) -> (Vec<GroupBy>, Vec<Aggregate>) {
    let model = reg.get("rusdoo.test.sale").unwrap();
    (
        groupby
            .iter()
            .map(|spec| GroupBy::parse(reg, model, spec).unwrap())
            .collect(),
        aggregates
            .iter()
            .map(|spec| Aggregate::parse(reg, model, spec).unwrap())
            .collect(),
    )
}

#[test]
fn groups_by_column_with_count_and_aggregate() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let (groupby, aggregates) = parse(&reg, &["country_id"], &["__count", "amount:sum"]);
    let query = reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([["amount", ">", 0]])).unwrap(),
            &groupby,
            &aggregates,
            &GroupOptions::default(),
        )
        .unwrap();

    assert!(
        query.sql.contains(r#"to_jsonb("country_id") AS "g0""#),
        "{}",
        query.sql
    );
    assert!(query.sql.contains(r#"to_jsonb(count(*)) AS "a0""#));
    assert!(query.sql.contains(r#"to_jsonb(sum("amount")) AS "a1""#));
    assert!(query.sql.contains(r#"GROUP BY "country_id""#));
    // no explicit order: the groupby values sort the groups
    assert!(query.sql.contains(r#"ORDER BY "country_id" ASC"#));
    assert_eq!(query.params, vec![json!(0)]);
    let specs: Vec<&str> = query.columns.iter().map(|c| c.spec.as_str()).collect();
    assert_eq!(specs, vec!["country_id", "__count", "amount:sum"]);
}

#[test]
fn date_groupby_buckets_by_granularity() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let (groupby, aggregates) = parse(&reg, &["day:week"], &["__count"]);
    let query = reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &groupby,
            &aggregates,
            &GroupOptions::default(),
        )
        .unwrap();
    assert!(
        query
            .sql
            .contains(r#"to_char(date_trunc('week', "day"), 'YYYY-MM-DD')"#),
        "{}",
        query.sql
    );

    // a datetime keeps the time part, like the read path's wire format
    let (groupby, aggregates) = parse(&reg, &["moment:day"], &["__count"]);
    let query = reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &groupby,
            &aggregates,
            &GroupOptions::default(),
        )
        .unwrap();
    assert!(query
        .sql
        .contains(r#"to_char(date_trunc('day', "moment"), 'YYYY-MM-DD HH24:MI:SS')"#));
}

#[test]
fn date_groupby_without_granularity_defaults_to_month() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let model = reg.get("rusdoo.test.sale").unwrap();
    let group = GroupBy::parse(&reg, model, "day").unwrap();
    let query = reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &[group],
            &[Aggregate::Count],
            &GroupOptions::default(),
        )
        .unwrap();
    assert!(query.sql.contains("date_trunc('month'"), "{}", query.sql);
}

#[test]
fn order_limit_and_offset_apply_to_groups() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let (groupby, aggregates) = parse(&reg, &["country_id"], &["__count"]);
    let query = reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &groupby,
            &aggregates,
            &GroupOptions {
                order: Some("__count desc, country_id asc".into()),
                limit: Some(5),
                offset: Some(10),
            },
        )
        .unwrap();
    assert!(
        query
            .sql
            .contains(r#"ORDER BY count(*) DESC, "country_id" ASC LIMIT 5 OFFSET 10"#),
        "{}",
        query.sql
    );
}

#[test]
fn malformed_specs_are_refused() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let model = reg.get("rusdoo.test.sale").unwrap();

    assert!(GroupBy::parse(&reg, model, "nope").is_err(), "unknown field");
    assert!(
        GroupBy::parse(&reg, model, "day:century").is_err(),
        "unknown granularity"
    );
    assert!(
        GroupBy::parse(&reg, model, "amount:month").is_err(),
        "granularity on a non-date field"
    );
    assert!(
        GroupBy::parse(&reg, model, "country_id.name").is_err(),
        "grouping through a related field is not supported yet"
    );
    assert!(
        GroupBy::parse(&reg, model, "tag_ids").is_err(),
        "a many2many has no column to group on"
    );

    assert!(
        Aggregate::parse(&reg, model, "amount").is_err(),
        "an aggregate needs a function"
    );
    assert!(
        Aggregate::parse(&reg, model, "amount:median").is_err(),
        "unknown aggregate function"
    );
    assert!(
        Aggregate::parse(&reg, model, "nope:sum").is_err(),
        "unknown field"
    );
    assert!(
        Aggregate::parse(&reg, model, "tag_ids:sum").is_err(),
        "x2many has no column to aggregate"
    );
    // the closed function set is what keeps SQL out of the spec
    assert!(Aggregate::parse(&reg, model, "amount:sum) FROM x --").is_err());
}

#[test]
fn ordering_by_an_unselected_spec_is_refused() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    let (groupby, aggregates) = parse(&reg, &["country_id"], &["__count"]);
    for order in ["name asc", "amount:sum desc", "country_id sideways"] {
        let outcome = reg.read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &groupby,
            &aggregates,
            &GroupOptions {
                order: Some(order.into()),
                ..GroupOptions::default()
            },
        );
        assert!(outcome.is_err(), "order {order:?} must be refused");
    }
}

#[test]
fn grouping_needs_at_least_one_groupby() {
    let reg = sales_registry("rusdoo_test_grp_sale");
    assert!(reg
        .read_group_sql(
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &[],
            &[Aggregate::Count],
            &GroupOptions::default(),
        )
        .is_err());
}

async fn test_pool() -> Option<PgPool> {
    // a schema of this run: these tests create tables directly, and
    // without it two runs touch the same ones
    rusdoo_testing::pool_in("rusdoo_group_test_test_pool")
}

#[tokio::test]
async fn read_group_returns_groups_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = sales_registry("rusdoo_test_grp_sale");
    for t in [
        "rusdoo_test_grp_rel",
        "rusdoo_test_grp_sale",
        "rusdoo_test_grp_country",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.country", "rusdoo.test.sale"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let br = reg
        .create(&pool, "res.country", vec![("name", json!("Brasil"))])
        .await
        .unwrap();
    let pt = reg
        .create(&pool, "res.country", vec![("name", json!("Portugal"))])
        .await
        .unwrap();
    for (name, amount, country, day) in [
        ("a", 10, Some(br), "2026-01-05"),
        ("b", 20, Some(br), "2026-01-20"),
        ("c", 5, Some(pt), "2026-02-03"),
        ("d", 7, None, "2026-02-11"),
    ] {
        reg.create(
            &pool,
            "rusdoo.test.sale",
            vec![
                ("name", json!(name)),
                ("amount", json!(amount)),
                ("country_id", country.map_or(json!(null), |c| json!(c))),
                ("day", json!(day)),
            ],
        )
        .await
        .unwrap();
    }

    // count and sum per country, biggest group first
    let model = reg.get("rusdoo.test.sale").unwrap();
    let groups = reg
        .read_group(
            &pool,
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &[GroupBy::parse(&reg, model, "country_id").unwrap()],
            &[
                Aggregate::Count,
                Aggregate::parse(&reg, model, "amount:sum").unwrap(),
                Aggregate::parse(&reg, model, "amount:max").unwrap(),
            ],
            &GroupOptions {
                order: Some("__count desc".into()),
                ..GroupOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(groups.len(), 3, "brasil, portugal and the empty group");
    assert_eq!(groups[0]["country_id"], json!(br));
    assert_eq!(groups[0]["__count"], json!(2));
    assert_eq!(groups[0]["amount:sum"], json!(30));
    assert_eq!(groups[0]["amount:max"], json!(20));
    // a record without a country groups under a null value, like Odoo's
    // False group — it is a group, not a dropped row
    let empty = groups
        .iter()
        .find(|g| g["country_id"].is_null())
        .expect("the empty group is present");
    assert_eq!(empty["__count"], json!(1));
    assert_eq!(empty["amount:sum"], json!(7));

    // the domain filters the rows before grouping
    let groups = reg
        .read_group(
            &pool,
            "rusdoo.test.sale",
            &parse_domain(&json!([["amount", ">=", 10]])).unwrap(),
            &[GroupBy::parse(&reg, model, "country_id").unwrap()],
            &[Aggregate::Count],
            &GroupOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["country_id"], json!(br));
    assert_eq!(groups[0]["__count"], json!(2));

    // dates bucket by granularity, in the read path's wire format
    let groups = reg
        .read_group(
            &pool,
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &[GroupBy::parse(&reg, model, "day:month").unwrap()],
            &[Aggregate::Count],
            &GroupOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["day:month"], json!("2026-01-01"));
    assert_eq!(groups[0]["__count"], json!(2));
    assert_eq!(groups[1]["day:month"], json!("2026-02-01"));
    assert_eq!(groups[1]["__count"], json!(2));

    // limit/offset page over the groups themselves
    let groups = reg
        .read_group(
            &pool,
            "rusdoo.test.sale",
            &parse_domain(&json!([])).unwrap(),
            &[GroupBy::parse(&reg, model, "name").unwrap()],
            &[Aggregate::Count],
            &GroupOptions {
                limit: Some(2),
                offset: Some(1),
                ..GroupOptions::default()
            },
        )
        .await
        .unwrap();
    let names: Vec<&str> = groups.iter().map(|g| g["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["b", "c"]);
}
