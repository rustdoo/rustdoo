//! Registry, `_inherit` inheritance and relational traversal (dotted paths,
//! any/not any). Reference: odoo/orm/registry.py and domains.py.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.into(),
        table: table.into(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn base_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("res.partner", "res_partner"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            ),
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
    ))
    .unwrap();
    reg.register(Model::new(
        meta("res.company", "res_company"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "partner_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

fn search(reg: &Registry, model: &str, dom: serde_json::Value) -> (String, Vec<serde_json::Value>) {
    reg.search_sql(
        model,
        &parse_domain(&dom).unwrap(),
        &SearchOptions::default(),
    )
    .unwrap()
}

// ---------- registry basics ----------

#[test]
fn registry_returns_registered_model() {
    let reg = base_registry();

    assert!(reg.get("res.partner").is_some());
    assert!(reg.get("res.missing").is_none());
}

#[test]
fn duplicate_registration_is_rejected() {
    let mut reg = base_registry();

    let dup = Model::new(meta("res.partner", "res_partner"), vec![]);

    assert!(reg.register(dup).is_err());
}

#[test]
fn unknown_model_search_is_rejected() {
    let reg = base_registry();
    let dom = parse_domain(&json!([])).unwrap();

    assert!(reg
        .search_sql("res.nope", &dom, &SearchOptions::default())
        .is_err());
}

// ---------- _inherit ----------

#[test]
fn inherit_extends_model_in_place() {
    // _inherit with the same _name adds and overrides fields on the target
    let mut reg = base_registry();

    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec!["res.partner".into()],
            inherits: vec![],
        },
        vec![
            Field::new("ref", FieldType::Char { size: None }),
            Field::new("name", FieldType::Char { size: None }).required(),
        ],
    ))
    .unwrap();

    let partner = reg.get("res.partner").unwrap();
    assert!(partner.field("ref").is_some(), "extension adds new field");
    assert!(
        partner.field("name").unwrap().required,
        "extension overrides field"
    );
    assert!(partner.field("child_ids").is_some(), "original fields kept");
}

#[test]
fn inherit_requires_registered_parent() {
    let mut reg = Registry::new();

    let orphan = Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec!["res.partner".into()],
            inherits: vec![],
        },
        vec![],
    );

    assert!(reg.register(orphan).is_err());
}

#[test]
fn prototype_inherit_copies_fields_to_new_model() {
    // _inherit with a different _name copies parent fields; parent untouched
    let mut reg = base_registry();

    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec!["res.partner".into()],
            inherits: vec![],
        },
        vec![Field::new("login", FieldType::Char { size: None }).required()],
    ))
    .unwrap();

    let users = reg.get("res.users").unwrap();
    assert!(users.field("name").is_some());
    assert!(users.field("login").is_some());
    assert_eq!(users.meta.table, "res_users");
    assert!(reg.get("res.partner").unwrap().field("login").is_none());
}

// ---------- dotted paths ----------

#[test]
fn many2one_path_renders_semijoin() {
    let reg = base_registry();

    let (sql, params) = search(
        &reg,
        "res.partner",
        json!([["company_id.name", "=", "Acme"]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "company_id" IN (SELECT "id" FROM "res_company" WHERE "name" = $1)"#
    );
    assert_eq!(params, vec![json!("Acme")]);
}

#[test]
fn one2many_path_renders_inverse_semijoin() {
    let reg = base_registry();

    let (sql, params) = search(
        &reg,
        "res.partner",
        json!([["child_ids.name", "like", "x"]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (SELECT "parent_id" FROM "res_partner" WHERE "name" LIKE $1)"#
    );
    assert_eq!(params, vec![json!("%x%")]);
}

#[test]
fn multi_hop_path_nests_subqueries() {
    let reg = base_registry();

    let (sql, _) = search(
        &reg,
        "res.partner",
        json!([["company_id.partner_id.name", "=", "x"]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "company_id" IN (SELECT "id" FROM "res_company" WHERE "partner_id" IN (SELECT "id" FROM "res_partner" WHERE "name" = $1))"#
    );
}

#[test]
fn path_on_nonrelational_field_is_rejected() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["name.length", "=", 1]])).unwrap();

    assert!(reg
        .search_sql("res.partner", &dom, &SearchOptions::default())
        .is_err());
}

// ---------- any / not any ----------

#[test]
fn any_operator_takes_a_subdomain() {
    let reg = base_registry();

    let (sql, params) = search(
        &reg,
        "res.partner",
        json!([["company_id", "any", [["name", "=", "Acme"]]]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "company_id" IN (SELECT "id" FROM "res_company" WHERE "name" = $1)"#
    );
    assert_eq!(params, vec![json!("Acme")]);
}

#[test]
fn not_any_on_many2one_includes_null() {
    let reg = base_registry();

    let (sql, _) = search(
        &reg,
        "res.partner",
        json!([["company_id", "not any", [["name", "=", "x"]]]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE ("company_id" NOT IN (SELECT "id" FROM "res_company" WHERE "name" = $1) OR "company_id" IS NULL)"#
    );
}

#[test]
fn not_any_on_one2many_guards_null_inverse() {
    // NULL inverse rows would poison NOT IN; they must be filtered inside
    let reg = base_registry();

    let (sql, _) = search(
        &reg,
        "res.partner",
        json!([["child_ids", "not any", [["name", "=", "x"]]]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" NOT IN (SELECT "parent_id" FROM "res_partner" WHERE ("name" = $1) AND "parent_id" IS NOT NULL)"#
    );
}

#[test]
fn first_listed_parent_wins_field_conflicts() {
    // Odoo MRO: own fields beat parents; among parents, the FIRST wins
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("mixin.a", "mixin_a"),
        vec![Field::new("x", FieldType::Text)],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("mixin.b", "mixin_b"),
        vec![Field::new("x", FieldType::Integer)],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "combo".into(),
            table: "combo".into(),
            inherit: vec!["mixin.a".into(), "mixin.b".into()],
            inherits: vec![],
        },
        vec![],
    ))
    .unwrap();

    assert_eq!(
        reg.get("combo").unwrap().field("x").unwrap().ty,
        FieldType::Text
    );
}
