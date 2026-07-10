//! Many2many traversal (relation table) and hierarchical operators
//! child_of/parent_of (recursive CTE). Reference: odoo/orm/domains.py,
//! fields_relational.py.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::ddl::create_relation_table_sql;
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
            Field::new(
                "main_category_id",
                FieldType::Many2one {
                    comodel: "res.partner.category".into(),
                },
            ),
            Field::new(
                "category_id",
                FieldType::Many2many {
                    comodel: "res.partner.category".into(),
                    relation: "res_partner_category_rel".into(),
                    column1: "partner_id".into(),
                    column2: "category_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("res.partner.category", "res_partner_category"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "res.partner.category".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("res.company", "res_company"),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg
}

fn search(reg: &Registry, dom: serde_json::Value) -> (String, Vec<serde_json::Value>) {
    reg.search_sql(
        "res.partner",
        &parse_domain(&dom).unwrap(),
        &SearchOptions::default(),
    )
    .unwrap()
}

// ---------- many2many ----------

#[test]
fn m2m_relation_table_ddl() {
    let reg = base_registry();
    let field = reg
        .get("res.partner")
        .unwrap()
        .field("category_id")
        .unwrap();

    let sql = create_relation_table_sql(field).unwrap().unwrap();

    assert_eq!(
        sql,
        r#"CREATE TABLE IF NOT EXISTS "res_partner_category_rel" ("partner_id" int4 NOT NULL, "category_id" int4 NOT NULL, PRIMARY KEY("partner_id", "category_id"))"#
    );
}

#[test]
fn m2m_any_goes_through_relation_table() {
    let reg = base_registry();

    let (sql, params) = search(
        &reg,
        json!([["category_id", "any", [["name", "=", "vip"]]]]),
    );

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (SELECT "partner_id" FROM "res_partner_category_rel" WHERE "category_id" IN (SELECT "id" FROM "res_partner_category" WHERE "name" = $1))"#
    );
    assert_eq!(params, vec![json!("vip")]);
}

#[test]
fn m2m_dotted_path_goes_through_relation_table() {
    let reg = base_registry();

    let (sql, _) = search(&reg, json!([["category_id.name", "=", "vip"]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (SELECT "partner_id" FROM "res_partner_category_rel" WHERE "category_id" IN (SELECT "id" FROM "res_partner_category" WHERE "name" = $1))"#
    );
}

#[test]
fn m2m_in_ids_matches_linked_records() {
    let reg = base_registry();

    let (sql, params) = search(&reg, json!([["category_id", "in", [1, 2]]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (SELECT "partner_id" FROM "res_partner_category_rel" WHERE "category_id" IN ($1, $2))"#
    );
    assert_eq!(params.len(), 2);
}

#[test]
fn m2m_not_in_ids_excludes_linked_records() {
    let reg = base_registry();

    let (sql, _) = search(&reg, json!([["category_id", "not in", [1]]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" NOT IN (SELECT "partner_id" FROM "res_partner_category_rel" WHERE "category_id" IN ($1))"#
    );
}

#[test]
fn o2m_in_ids_goes_through_inverse() {
    let reg = base_registry();

    let (sql, _) = search(&reg, json!([["child_ids", "in", [5]]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (SELECT "parent_id" FROM "res_partner" WHERE "id" IN ($1))"#
    );
}

// ---------- child_of / parent_of ----------

#[test]
fn child_of_on_id_uses_recursive_cte() {
    let reg = base_registry();

    let (sql, params) = search(&reg, json!([["id", "child_of", 1]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (WITH RECURSIVE __rusdoo_tree AS (SELECT "id" FROM "res_partner" WHERE "id" IN ($1) UNION SELECT __t."id" FROM "res_partner" __t JOIN __rusdoo_tree __r ON __t."parent_id" = __r."id") SELECT "id" FROM __rusdoo_tree)"#
    );
    assert_eq!(params, vec![json!(1)]);
}

#[test]
fn child_of_on_self_referential_m2o_remaps_to_id() {
    // Odoo: child_of on parent_id (same model) == child_of on id
    let reg = base_registry();

    let (sql, _) = search(&reg, json!([["parent_id", "child_of", 1]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (WITH RECURSIVE __rusdoo_tree AS (SELECT "id" FROM "res_partner" WHERE "id" IN ($1) UNION SELECT __t."id" FROM "res_partner" __t JOIN __rusdoo_tree __r ON __t."parent_id" = __r."id") SELECT "id" FROM __rusdoo_tree)"#
    );
}

#[test]
fn child_of_on_foreign_m2o_keeps_the_column() {
    let reg = base_registry();

    let (sql, _) = search(&reg, json!([["main_category_id", "child_of", 3]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "main_category_id" IN (WITH RECURSIVE __rusdoo_tree AS (SELECT "id" FROM "res_partner_category" WHERE "id" IN ($1) UNION SELECT __t."id" FROM "res_partner_category" __t JOIN __rusdoo_tree __r ON __t."parent_id" = __r."id") SELECT "id" FROM __rusdoo_tree)"#
    );
}

#[test]
fn child_of_with_empty_id_list_matches_nothing() {
    let reg = base_registry();

    let (sql, params) = search(&reg, json!([["id", "child_of", []]]));

    assert_eq!(sql, r#"SELECT "id" FROM "res_partner" WHERE FALSE"#);
    assert!(params.is_empty());
}

#[test]
fn unknown_field_fails_fast_with_model_context() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["nope", "=", 1]])).unwrap();

    assert!(reg
        .search_sql("res.partner", &dom, &SearchOptions::default())
        .is_err());
}

#[test]
fn parent_of_on_id_walks_upwards() {
    let reg = base_registry();

    let (sql, params) = search(&reg, json!([["id", "parent_of", 7]]));

    assert_eq!(
        sql,
        r#"SELECT "id" FROM "res_partner" WHERE "id" IN (WITH RECURSIVE __rusdoo_tree AS (SELECT "id", "parent_id" FROM "res_partner" WHERE "id" IN ($1) UNION SELECT __t."id", __t."parent_id" FROM "res_partner" __t JOIN __rusdoo_tree __r ON __t."id" = __r."parent_id") SELECT "id" FROM __rusdoo_tree)"#
    );
    assert_eq!(params, vec![json!(7)]);
}

#[test]
fn child_of_accepts_id_lists() {
    let reg = base_registry();

    let (_, params) = search(&reg, json!([["id", "child_of", [1, 2]]]));

    assert_eq!(params, vec![json!(1), json!(2)]);
}

#[test]
fn child_of_requires_parent_field_on_target() {
    // res.company has no parent_id: must be an explicit error
    let reg = base_registry();
    let dom = parse_domain(&json!([["company_id", "child_of", 1]])).unwrap();

    assert!(reg
        .search_sql("res.partner", &dom, &SearchOptions::default())
        .is_err());
}

#[test]
fn child_of_rejects_non_numeric_values() {
    let reg = base_registry();
    let dom = parse_domain(&json!([["id", "child_of", "Agrolait"]])).unwrap();

    assert!(reg
        .search_sql("res.partner", &dom, &SearchOptions::default())
        .is_err());
}
