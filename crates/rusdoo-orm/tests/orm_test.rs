//! Phase 1 ORM tests. Reference behaviour: `odoo/odoo/orm/domains.py`
//! (STANDARD_CONDITION_OPERATORS, NEGATIVE_CONDITION_OPERATORS, _TRUE_LEAF).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::ddl::create_table_sql;
use rusdoo_orm::domain::{parse_domain, Domain, Operator};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::sql::where_clause;
use serde_json::json;

fn partner_model() -> Model {
    Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("ref", FieldType::Char { size: Some(64) }),
            Field::new("active", FieldType::Boolean),
            Field::new("color", FieldType::Integer),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "res.company".into(),
                },
            ),
            Field::new(
                "child_ids",
                FieldType::One2many {
                    comodel: "res.partner".into(),
                    inverse: "parent_id".into(),
                },
            ),
        ],
    )
}

// ---------- domain parsing ----------

#[test]
fn parses_implicit_and_between_terms() {
    // Arrange
    let raw = json!([["name", "=", "Gemini"], ["color", ">", 3]]);

    // Act
    let dom = parse_domain(&raw).unwrap();

    // Assert
    match dom {
        Domain::And(children) => assert_eq!(children.len(), 2),
        other => panic!("expected And, got {other:?}"),
    }
}

#[test]
fn parses_prefix_or_operator() {
    let raw = json!(["|", ["name", "=", "a"], ["name", "=", "b"]]);

    let dom = parse_domain(&raw).unwrap();

    match dom {
        Domain::Or(children) => assert_eq!(children.len(), 2),
        other => panic!("expected Or, got {other:?}"),
    }
}

#[test]
fn parses_not_operator() {
    let raw = json!(["!", ["active", "=", true]]);

    let dom = parse_domain(&raw).unwrap();

    assert!(matches!(dom, Domain::Not(_)));
}

#[test]
fn parses_true_and_false_leaves() {
    // (1, '=', 1) and (0, '=', 1) are Odoo's _TRUE_LEAF / _FALSE_LEAF
    assert!(matches!(
        parse_domain(&json!([[1, "=", 1]])).unwrap(),
        Domain::True
    ));
    assert!(matches!(
        parse_domain(&json!([[0, "=", 1]])).unwrap(),
        Domain::False
    ));
}

#[test]
fn empty_domain_matches_everything() {
    assert!(matches!(parse_domain(&json!([])).unwrap(), Domain::True));
}

#[test]
fn rejects_malformed_term() {
    assert!(parse_domain(&json!([["name", "="]])).is_err());
}

#[test]
fn rejects_unknown_operator() {
    assert!(parse_domain(&json!([["name", "===", "x"]])).is_err());
}

#[test]
fn normalizes_diamond_to_not_equal() {
    // '<>' is a legacy alias of '!=' in Odoo
    let dom = parse_domain(&json!([["name", "<>", "x"]])).unwrap();

    match dom {
        Domain::Term(t) => assert_eq!(t.op, Operator::Neq),
        other => panic!("expected Term, got {other:?}"),
    }
}

// ---------- WHERE generation ----------

fn where_sql(raw: serde_json::Value) -> (String, Vec<serde_json::Value>) {
    where_clause(&parse_domain(&raw).unwrap()).unwrap()
}

#[test]
fn eq_generates_placeholder() {
    let (sql, params) = where_sql(json!([["name", "=", "Gemini"]]));

    assert_eq!(sql, r#""name" = $1"#);
    assert_eq!(params, vec![json!("Gemini")]);
}

#[test]
fn eq_false_means_not_set() {
    // Odoo: False value indicates the field is *not set*
    let (sql, params) = where_sql(json!([["ref", "=", false]]));

    assert_eq!(sql, r#""ref" IS NULL"#);
    assert!(params.is_empty());
}

#[test]
fn neq_includes_null_rows() {
    // Odoo negative operators also match unset records
    let (sql, params) = where_sql(json!([["name", "!=", "x"]]));

    assert_eq!(sql, r#"("name" != $1 OR "name" IS NULL)"#);
    assert_eq!(params.len(), 1);
}

#[test]
fn in_list_expands_placeholders() {
    let (sql, params) = where_sql(json!([["color", "in", [1, 2, 3]]]));

    assert_eq!(sql, r#""color" IN ($1, $2, $3)"#);
    assert_eq!(params.len(), 3);
}

#[test]
fn in_list_with_false_also_matches_null() {
    let (sql, _) = where_sql(json!([["ref", "in", ["a", false]]]));

    assert_eq!(sql, r#"("ref" IN ($1) OR "ref" IS NULL)"#);
}

#[test]
fn not_in_includes_null_rows() {
    let (sql, _) = where_sql(json!([["color", "not in", [1, 2]]]));

    assert_eq!(sql, r#"("color" NOT IN ($1, $2) OR "color" IS NULL)"#);
}

#[test]
fn like_wraps_value_and_escapes_wildcards() {
    let (sql, params) = where_sql(json!([["name", "like", "50%_off"]]));

    assert_eq!(sql, r#""name" LIKE $1"#);
    assert_eq!(params, vec![json!(r"%50\%\_off%")]);
}

#[test]
fn ilike_is_case_insensitive() {
    let (sql, _) = where_sql(json!([["name", "ilike", "gemini"]]));

    assert_eq!(sql, r#""name" ILIKE $1"#);
}

#[test]
fn eq_like_keeps_value_verbatim() {
    let (_, params) = where_sql(json!([["name", "=like", "G%"]]));

    assert_eq!(params, vec![json!("G%")]);
}

#[test]
fn or_combines_two_conditions() {
    let (sql, params) = where_sql(json!(["|", ["name", "=", "a"], ["color", "=", 2]]));

    assert_eq!(sql, r#"("name" = $1 OR "color" = $2)"#);
    assert_eq!(params.len(), 2);
}

#[test]
fn not_wraps_condition() {
    let (sql, _) = where_sql(json!(["!", ["name", "=", "a"]]));

    assert_eq!(sql, r#"NOT ("name" = $1)"#);
}

#[test]
fn rejects_unsafe_field_identifier() {
    // identifiers must match [a-z_][a-z0-9_]* — anything else is an error
    let dom = parse_domain(&json!([["name\"; --injection", "=", "a"]])).unwrap();

    assert!(where_clause(&dom).is_err());
}

#[test]
fn dotted_field_paths_not_yet_supported() {
    // joins land in a later slice; must be an explicit error, never silent
    let dom = parse_domain(&json!([["company_id.name", "=", "a"]])).unwrap();

    assert!(where_clause(&dom).is_err());
}

// ---------- DDL ----------

#[test]
fn create_table_has_id_and_magic_columns() {
    let sql = create_table_sql(&partner_model()).unwrap();

    assert!(sql.starts_with(r#"CREATE TABLE IF NOT EXISTS "res_partner""#));
    assert!(sql.contains(r#""id" SERIAL NOT NULL"#));
    for magic in ["create_uid", "create_date", "write_uid", "write_date"] {
        assert!(sql.contains(&format!(r#""{magic}""#)), "missing {magic}");
    }
    assert!(sql.contains(r#"PRIMARY KEY("id")"#));
}

#[test]
fn maps_field_types_to_postgres() {
    let sql = create_table_sql(&partner_model()).unwrap();

    assert!(sql.contains(r#""name" varchar NOT NULL"#));
    assert!(sql.contains(r#""ref" varchar(64)"#));
    assert!(sql.contains(r#""active" bool"#));
    assert!(sql.contains(r#""company_id" int4"#));
    // one2many is not stored as a column
    assert!(!sql.contains("child_ids"));
}

// ---------- CRUD builders ----------

#[test]
fn search_sql_composes_where_order_limit() {
    let model = partner_model();
    let dom = parse_domain(&json!([["active", "=", true]])).unwrap();
    let opts = SearchOptions {
        order: Some("name asc".into()),
        limit: Some(10),
        offset: Some(5),
        ..SearchOptions::default()
    };

    let (sql, params) = model.search_sql(&dom, &opts).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "active" = $1 ORDER BY "name" ASC LIMIT 10 OFFSET 5"#
    );
    assert_eq!(params, vec![json!(true)]);
}

#[test]
fn search_sql_rejects_unknown_order_field() {
    let model = partner_model();
    let dom = parse_domain(&json!([])).unwrap();
    let opts = SearchOptions {
        order: Some("evil; injection".into()),
        ..Default::default()
    };

    assert!(model.search_sql(&dom, &opts).is_err());
}

#[test]
fn read_sql_selects_requested_fields() {
    let model = partner_model();

    let (sql, params) = model.read_sql(&[1, 2], &["name", "color"]).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id", "name", "color" FROM "res_partner" WHERE "id" IN ($1, $2)"#
    );
    assert_eq!(params, vec![json!(1), json!(2)]);
}

#[test]
fn insert_sql_returns_new_id() {
    let model = partner_model();

    let (sql, params) = model
        .insert_sql(1, vec![("name", json!("Gemini")), ("color", json!(7))])
        .unwrap();

    assert_eq!(
        sql,
        r#"INSERT INTO "res_partner" ("name", "color", "create_uid", "write_uid", "create_date", "write_date") VALUES ($1, $2, $3, $3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING "id""#
    );
    // name, color, uid (create_uid and write_uid reuse the same bind)
    assert_eq!(params.len(), 3);
}

#[test]
fn insert_sql_rejects_unknown_field() {
    let model = partner_model();

    assert!(model.insert_sql(1, vec![("nope", json!(1))]).is_err());
}

#[test]
fn update_sql_sets_columns_for_ids() {
    let model = partner_model();

    let (sql, params) = model
        .update_sql(9, &[4, 5], vec![("name", json!("X"))])
        .unwrap();

    assert_eq!(
        sql,
        r#"UPDATE "res_partner" SET "name" = $1, "write_uid" = $2, "write_date" = CURRENT_TIMESTAMP WHERE "id" IN ($3, $4)"#
    );
    // name, write_uid, and the two ids
    assert_eq!(params.len(), 4);
}

#[test]
fn delete_sql_removes_ids() {
    let model = partner_model();

    let (sql, params) = model.delete_sql(&[9]).unwrap();

    assert_eq!(sql, r#"DELETE FROM "res_partner" WHERE "id" IN ($1)"#);
    assert_eq!(params, vec![json!(9)]);
}

// ---------- review follow-ups ----------

#[test]
fn parser_rejects_absurdly_nested_domains() {
    // stack-overflow guard: nesting is capped, error instead of crash
    let mut items: Vec<serde_json::Value> = vec![json!("!"); 300];
    items.push(json!(["name", "=", "x"]));

    assert!(parse_domain(&serde_json::Value::Array(items)).is_err());
}

#[test]
fn render_rejects_absurdly_nested_domains() {
    let mut dom = parse_domain(&json!([["name", "=", "x"]])).unwrap();
    for _ in 0..300 {
        dom = Domain::Not(Box::new(dom));
    }

    assert!(where_clause(&dom).is_err());
}

#[test]
fn eq_false_on_boolean_matches_falsy_or_null() {
    // with model context, "not set" also matches the type's falsy value
    let model = partner_model();
    let dom = parse_domain(&json!([["active", "=", false]])).unwrap();

    let (sql, params) = model.search_sql(&dom, &SearchOptions::default()).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE ("active" = $1 OR "active" IS NULL) ORDER BY "id" ASC"#
    );
    assert_eq!(params, vec![json!(false)]);
}

#[test]
fn neq_false_on_integer_requires_set_and_nonzero() {
    let model = partner_model();
    let dom = parse_domain(&json!([["color", "!=", false]])).unwrap();

    // active_test off: this is about how `!= False` renders on an integer,
    // and the archive filter has its own tests
    let opts = SearchOptions {
        active_test: false,
        ..SearchOptions::default()
    };
    let (sql, params) = model.search_sql(&dom, &opts).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE ("color" != $1 AND "color" IS NOT NULL) ORDER BY "id" ASC"#
    );
    assert_eq!(params, vec![json!(0)]);
}

#[test]
fn eq_with_list_value_becomes_in() {
    // Odoo rewrites '=' with a collection into 'in'
    let (sql, params) = where_sql(json!([["color", "=", [1, 2]]]));

    assert_eq!(sql, r#""color" IN ($1, $2)"#);
    assert_eq!(params.len(), 2);
}

#[test]
fn float_with_digits_maps_to_scaled_numeric() {
    let field = Field::new(
        "amount",
        FieldType::Float {
            digits: Some((16, 2)),
        },
    );

    assert_eq!(field.column_type().as_deref(), Some("numeric(16,2)"));
}

// ---------- date/datetime parameters ----------

fn dated_model() -> Model {
    Model::new(
        ModelMeta {
            name: "rusdoo.test.dated".into(),
            table: "rusdoo_test_dated".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("day", FieldType::Date),
            Field::new("moment", FieldType::Datetime),
        ],
    )
}

#[test]
fn date_values_are_cast_on_write() {
    let model = dated_model();
    let (sql, params) = model
        .insert_sql(
            1,
            vec![
                ("name", json!("x")),
                ("day", json!("2026-07-31")),
                ("moment", json!("2026-07-31 12:30:00")),
            ],
        )
        .unwrap();
    // dates travel as JSON strings: without the cast the parameter is
    // text and PostgreSQL refuses to store it in a date column
    assert!(sql.contains("$2::date"), "{sql}");
    assert!(sql.contains("$3::timestamp"), "{sql}");
    assert!(!sql.contains("$1::"), "a char column needs no cast: {sql}");
    assert_eq!(params[1], json!("2026-07-31"));

    let (sql, _) = model
        .update_sql(1, &[7], vec![("day", json!("2026-08-01"))])
        .unwrap();
    assert!(sql.contains(r#""day" = $1::date"#), "{sql}");
}

#[test]
fn date_values_are_cast_in_domains() {
    let model = dated_model();
    let domain = parse_domain(&json!([["day", ">=", "2026-01-01"]])).unwrap();
    let (sql, _) = model
        .search_sql(&domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""day" >= $1::date"#), "{sql}");

    let domain = parse_domain(&json!([["moment", "=", "2026-01-01 00:00:00"]])).unwrap();
    let (sql, _) = model
        .search_sql(&domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""moment" = $1::timestamp"#), "{sql}");

    // every value of a list is cast, and so is the negative form
    let domain = parse_domain(&json!([["day", "in", ["2026-01-01", "2026-02-01"]]])).unwrap();
    let (sql, _) = model
        .search_sql(&domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""day" IN ($1::date, $2::date)"#), "{sql}");
    let domain = parse_domain(&json!([["day", "!=", "2026-01-01"]])).unwrap();
    let (sql, _) = model
        .search_sql(&domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""day" != $1::date"#), "{sql}");

    // an unset date is still a NULL check, not a cast comparison
    let domain = parse_domain(&json!([["day", "=", false]])).unwrap();
    let (sql, params) = model
        .search_sql(&domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""day" IS NULL"#), "{sql}");
    assert!(params.is_empty());
}

// ---------- counting ----------

#[test]
fn count_sql_counts_in_the_database() {
    let model = partner_model();
    let dom = parse_domain(&json!([["color", ">", 3]])).unwrap();
    let opts = SearchOptions {
        active_test: false,
        ..SearchOptions::default()
    };
    let (sql, params) = model.count_sql(&dom, &opts).unwrap();
    // no ids travel just to be counted
    assert_eq!(
        sql,
        r#"SELECT COUNT(*) FROM "res_partner" WHERE "color" > $1"#
    );
    assert_eq!(params, vec![json!(3)]);

    // a capped count keeps the paging clauses, so it can still answer
    // "at least n" like Odoo's search_count(limit=n)
    let opts = SearchOptions {
        limit: Some(80),
        active_test: false,
        ..SearchOptions::default()
    };
    let (sql, _) = model.count_sql(&dom, &opts).unwrap();
    assert!(
        sql.starts_with("SELECT COUNT(*) FROM (SELECT \"id\""),
        "{sql}"
    );
    assert!(sql.contains("LIMIT 80"), "{sql}");

    // and the archive filter applies to a count like to any search
    let (sql, _) = model.count_sql(&dom, &SearchOptions::default()).unwrap();
    assert!(sql.contains(r#""active" = $2"#), "{sql}");
}

/// A read answers in the order its ids were asked for: a search's
/// `ORDER BY` decides the sequence, and losing it in the read would
/// scramble every sorted list the client draws.
#[tokio::test]
async fn read_answers_in_the_order_the_ids_were_given_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = rusdoo_orm::registry::Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.order".into(),
            table: "rusdoo_test_read_order".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_read_order""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.order")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for name in ["ana", "bia", "caio"] {
        ids.push(
            reg.create(
                &pool,
                "rusdoo.test.order",
                vec![("name", serde_json::json!(name))],
            )
            .await
            .unwrap(),
        );
    }
    let reversed: Vec<i64> = ids.iter().rev().copied().collect();
    let rows = reg
        .read(&pool, "rusdoo.test.order", &reversed, &["name"])
        .await
        .unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["caio", "bia", "ana"]);

    // and a descending search reaches the client descending
    let domain = rusdoo_orm::domain::parse_domain(&serde_json::json!([])).unwrap();
    let found = reg
        .search(
            &pool,
            "rusdoo.test.order",
            &domain,
            &SearchOptions {
                order: Some("name desc".into()),
                ..SearchOptions::default()
            },
        )
        .await
        .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.order", &found, &["name"])
        .await
        .unwrap();
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["caio", "bia", "ana"]);
}
