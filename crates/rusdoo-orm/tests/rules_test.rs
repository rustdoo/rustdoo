//! Record rules (`ir.rule`): how global and group rules combine, and how
//! a rule domain speaks about the acting user.

use rusdoo_orm::access::Operation;
use rusdoo_orm::domain::Domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_orm::rules::{RecordRules, Rule};
use serde_json::json;

const ANA: i64 = 7;
const SALES: i64 = 3;
const SUPPORT: i64 = 4;

fn rule(domain: serde_json::Value, groups: Vec<i64>, operations: Vec<Operation>) -> Rule {
    Rule {
        model: "res.partner".into(),
        domain,
        groups,
        operations,
    }
}

/// Render a rule domain to SQL, so the assertions read as what the
/// database will actually be asked.
fn sql(rules: &RecordRules, op: Operation, groups: &[i64]) -> Option<String> {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("user_id", FieldType::Integer),
            Field::new("team_id", FieldType::Integer),
        ],
    ))
    .unwrap();
    let domain = rules
        .domain_for("res.partner", op, ANA, groups, false)
        .unwrap()?;
    let (sql, params) = reg
        .search_sql(
            "res.partner",
            &domain,
            &rusdoo_orm::crud::SearchOptions {
                active_test: false,
                ..Default::default()
            },
        )
        .unwrap();
    // inline the parameters so the assertion shows the whole condition
    let mut rendered = sql;
    for (index, value) in params.iter().enumerate() {
        rendered = rendered.replace(&format!("${}", index + 1), &value.to_string());
    }
    Some(rendered)
}

#[test]
fn no_rule_means_unrestricted() {
    let rules = RecordRules::new();
    assert!(rules
        .domain_for("res.partner", Operation::Read, ANA, &[SALES], false)
        .unwrap()
        .is_none());
    assert!(!rules.covers("res.partner"));
}

#[test]
fn the_superuser_bypasses_every_rule() {
    let mut rules = RecordRules::new();
    rules.add(rule(
        json!([["user_id", "=", "user.id"]]),
        vec![],
        vec![Operation::Read],
    ));
    assert!(rules
        .domain_for("res.partner", Operation::Read, 1, &[], true)
        .unwrap()
        .is_none());
}

#[test]
fn a_rule_domain_names_the_acting_user() {
    let mut rules = RecordRules::new();
    rules.add(rule(
        json!([["user_id", "=", "user.id"]]),
        vec![],
        vec![Operation::Read, Operation::Write],
    ));
    let rendered = sql(&rules, Operation::Read, &[]).unwrap();
    assert!(rendered.contains(r#""user_id" = 7"#), "{rendered}");
    // ...and only for the operations it covers
    assert!(rules
        .domain_for("res.partner", Operation::Unlink, ANA, &[], false)
        .unwrap()
        .is_none());
}

#[test]
fn global_rules_narrow_and_group_rules_widen() {
    let mut rules = RecordRules::new();
    // global: only active teams, for everyone
    rules.add(rule(
        json!([["team_id", "!=", false]]),
        vec![],
        vec![Operation::Read],
    ));
    // group rules: each one opens up a slice
    rules.add(rule(
        json!([["user_id", "=", "user.id"]]),
        vec![SALES],
        vec![Operation::Read],
    ));
    rules.add(rule(
        json!([["team_id", "=", 42]]),
        vec![SUPPORT],
        vec![Operation::Read],
    ));

    // in sales only: global AND the sales rule
    let rendered = sql(&rules, Operation::Read, &[SALES]).unwrap();
    assert!(rendered.contains(r#""user_id" = 7"#), "{rendered}");
    assert!(
        !rendered.contains("42"),
        "another group's rule must not apply: {rendered}"
    );

    // in both groups: the two group rules are OR-ed, so membership widens
    let rendered = sql(&rules, Operation::Read, &[SALES, SUPPORT]).unwrap();
    assert!(
        rendered.contains(" OR "),
        "group rules must be OR-ed: {rendered}"
    );
    assert!(
        rendered.contains(r#""user_id" = 7"#) && rendered.contains("42"),
        "{rendered}"
    );

    // in neither: only the global rule constrains
    let rendered = sql(&rules, Operation::Read, &[]).unwrap();
    assert!(!rendered.contains(" OR "), "{rendered}");
    assert!(rendered.contains(r#""team_id" IS NOT NULL"#), "{rendered}");
}

#[test]
fn user_id_is_substituted_only_in_the_value_slot() {
    let mut rules = RecordRules::new();
    // a dotted path on a field named `user` is a legitimate domain: the
    // placeholder must not rewrite the field name into an id
    rules.add(Rule {
        model: "res.partner".into(),
        domain: json!([["user.id", "=", "user.id"]]),
        groups: vec![],
        operations: vec![Operation::Read],
    });
    let domain = rules
        .domain_for("res.partner", Operation::Read, ANA, &[], false)
        .unwrap()
        .unwrap();
    let Domain::Term(term) = &domain else {
        panic!("expected a single condition, got {domain:?}");
    };
    assert_eq!(term.field, "user.id", "the field name is untouched");
    assert_eq!(term.value, json!(ANA), "only the value names the user");
}

#[test]
fn a_malformed_rule_domain_is_an_error_not_a_bypass() {
    let mut rules = RecordRules::new();
    rules.add(rule(
        json!([["user_id", "===", 1]]),
        vec![],
        vec![Operation::Read],
    ));
    assert!(rules
        .domain_for("res.partner", Operation::Read, ANA, &[], false)
        .is_err());
}
