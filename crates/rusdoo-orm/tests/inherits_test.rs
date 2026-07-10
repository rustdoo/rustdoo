//! Delegation inheritance (`_inherits`): transparent access to parent
//! fields through a required many2one link. Reference:
//! odoo/orm/model_classes.py and models.py (delegate=True).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;

fn base_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.company".into(),
            table: "res_company".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("email", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "res.company".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec![],
            inherits: vec![("res.partner".into(), "partner_id".into())],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new(
                "partner_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            )
            .required(),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "hr.employee".into(),
            table: "hr_employee".into(),
            inherit: vec![],
            inherits: vec![("res.users".into(), "user_id".into())],
        },
        vec![
            Field::new("badge", FieldType::Char { size: None }),
            Field::new(
                "user_id",
                FieldType::Many2one {
                    comodel: "res.users".into(),
                },
            )
            .required(),
        ],
    ))
    .unwrap();
    reg
}

// ---------- registration validation ----------

#[test]
fn delegation_requires_registered_parent() {
    let mut reg = Registry::new();

    let orphan = Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec![],
            inherits: vec![("res.partner".into(), "partner_id".into())],
        },
        vec![Field::new(
            "partner_id",
            FieldType::Many2one {
                comodel: "res.partner".into(),
            },
        )],
    );

    assert!(reg.register(orphan).is_err());
}

#[test]
fn delegation_requires_the_link_field() {
    let mut reg = base_registry();

    // no badge_partner_id many2one declared -> must be rejected
    let broken = Model::new(
        ModelMeta {
            name: "res.badge".into(),
            table: "res_badge".into(),
            inherit: vec![],
            inherits: vec![("res.partner".into(), "badge_partner_id".into())],
        },
        vec![Field::new("code", FieldType::Char { size: None })],
    );

    assert!(reg.register(broken).is_err());
}

// ---------- search ----------

#[test]
fn search_on_delegated_field_goes_through_the_link() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["name", "=", "Ana"]])).unwrap();

    let (sql, params) = reg
        .search_sql("res.users", &dom, &SearchOptions::default())
        .unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_users" WHERE "partner_id" IN (SELECT "id" FROM "res_partner" WHERE "name" = $1)"#
    );
    assert_eq!(params, vec![json!("Ana")]);
}

#[test]
fn search_chains_two_delegation_levels() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["email", "=", "a@x"]])).unwrap();

    let (sql, _) = reg
        .search_sql("hr.employee", &dom, &SearchOptions::default())
        .unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "hr_employee" WHERE "user_id" IN (SELECT "id" FROM "res_users" WHERE "partner_id" IN (SELECT "id" FROM "res_partner" WHERE "email" = $1))"#
    );
}

#[test]
fn dotted_path_through_delegation() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["company_id.name", "=", "Acme"]])).unwrap();

    let (sql, _) = reg
        .search_sql("res.users", &dom, &SearchOptions::default())
        .unwrap();

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_users" WHERE "partner_id" IN (SELECT "id" FROM "res_partner" WHERE "company_id" IN (SELECT "id" FROM "res_company" WHERE "name" = $1))"#
    );
}

#[test]
fn unknown_field_still_fails_fast() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["nope", "=", 1]])).unwrap();

    assert!(reg
        .search_sql("res.users", &dom, &SearchOptions::default())
        .is_err());
}

// ---------- read ----------

#[test]
fn read_sql_joins_the_parent_table() {
    let reg = base_registry();

    let (sql, params) = reg.read_sql("res.users", &[1], &["login", "name"]).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "t0"."id", "t0"."login", "t1"."name" FROM "res_users" "t0" LEFT JOIN "res_partner" "t1" ON "t0"."partner_id" = "t1"."id" WHERE "t0"."id" IN ($1)"#
    );
    assert_eq!(params, vec![json!(1)]);
}

#[test]
fn read_sql_reuses_the_join_for_sibling_fields() {
    let reg = base_registry();

    let (sql, _) = reg.read_sql("res.users", &[1], &["name", "email"]).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "t0"."id", "t1"."name", "t1"."email" FROM "res_users" "t0" LEFT JOIN "res_partner" "t1" ON "t0"."partner_id" = "t1"."id" WHERE "t0"."id" IN ($1)"#
    );
}

#[test]
fn read_sql_chains_two_delegation_levels() {
    let reg = base_registry();

    let (sql, _) = reg.read_sql("hr.employee", &[1], &["name"]).unwrap();

    assert_eq!(
        sql,
        r#"SELECT "t0"."id", "t2"."name" FROM "hr_employee" "t0" LEFT JOIN "res_users" "t1" ON "t0"."user_id" = "t1"."id" LEFT JOIN "res_partner" "t2" ON "t1"."partner_id" = "t2"."id" WHERE "t0"."id" IN ($1)"#
    );
}

#[test]
fn read_sql_rejects_unknown_field() {
    let reg = base_registry();

    assert!(reg.read_sql("res.users", &[1], &["nope"]).is_err());
}

#[test]
fn duplicate_delegation_parent_is_rejected() {
    let mut reg = base_registry();

    let broken = Model::new(
        ModelMeta {
            name: "res.double".into(),
            table: "res_double".into(),
            inherit: vec![],
            inherits: vec![
                ("res.partner".into(), "a_id".into()),
                ("res.partner".into(), "b_id".into()),
            ],
        },
        vec![
            Field::new(
                "a_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            ),
            Field::new(
                "b_id",
                FieldType::Many2one {
                    comodel: "res.partner".into(),
                },
            ),
        ],
    );

    assert!(reg.register(broken).is_err());
}

#[test]
fn duplicate_delegation_link_is_rejected() {
    let mut reg = base_registry();

    let broken = Model::new(
        ModelMeta {
            name: "res.twice".into(),
            table: "res_twice".into(),
            inherit: vec![],
            inherits: vec![
                ("res.partner".into(), "link_id".into()),
                ("res.company".into(), "link_id".into()),
            ],
        },
        vec![Field::new(
            "link_id",
            FieldType::Many2one {
                comodel: "res.partner".into(),
            },
        )],
    );

    assert!(reg.register(broken).is_err());
}
