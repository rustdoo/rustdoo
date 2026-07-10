//! Data file loading: <record>/<field> XML, model CSVs, external ids
//! and noupdate. Reference: odoo/tools/convert.py, ir_model_data.

use rusdoo_modules::data::{parse_csv_data, parse_xml_data, FieldValue};
use rusdoo_modules::installer::{load_records, XmlIds};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use std::path::Path;

// ---------- XML ----------

#[test]
fn parses_xml_records_with_text_ref_and_eval() {
    let src = r#"<?xml version="1.0" encoding="utf-8"?>
<odoo>
    <data noupdate="1">
        <record id="partner_ana" model="res.partner">
            <field name="name">Ana</field>
            <field name="company_id" ref="base.main_company"/>
            <field name="color" eval="7"/>
            <field name="tags" eval="['a', 'b']"/>
        </record>
    </data>
    <record model="res.partner">
        <field name="name">Anon</field>
    </record>
</odoo>"#;

    let records = parse_xml_data(src).unwrap();

    assert_eq!(records.len(), 2);
    let first = &records[0];
    assert_eq!(first.xml_id.as_deref(), Some("partner_ana"));
    assert_eq!(first.model, "res.partner");
    assert!(first.noupdate);
    assert_eq!(
        first.fields[0],
        ("name".into(), FieldValue::Text("Ana".into()))
    );
    assert_eq!(
        first.fields[1],
        (
            "company_id".into(),
            FieldValue::Ref("base.main_company".into())
        )
    );
    assert_eq!(first.fields[2].1, FieldValue::Eval(json!(7)));
    assert_eq!(first.fields[3].1, FieldValue::Eval(json!(["a", "b"])));
    // records outside <data noupdate="1"> are updatable
    assert!(!records[1].noupdate);
    assert_eq!(records[1].xml_id, None);
}

#[test]
fn non_record_elements_are_skipped() {
    let src = r#"<odoo>
        <menuitem id="menu_x" name="X"/>
        <template id="tpl"><div>html</div></template>
        <record id="a" model="res.partner"><field name="name">A</field></record>
    </odoo>"#;

    let records = parse_xml_data(src).unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].xml_id.as_deref(), Some("a"));
}

#[test]
fn record_requires_a_model() {
    assert!(parse_xml_data(r#"<odoo><record id="x"/></odoo>"#).is_err());
}

// ---------- CSV ----------

#[test]
fn csv_rows_become_records_with_external_ids() {
    let src = "id,name,country_id:id\nstate_sp,\"S\u{e3}o Paulo\",base.br\n";

    let records = parse_csv_data("res.country.state", src).unwrap();

    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.model, "res.country.state");
    assert_eq!(r.xml_id.as_deref(), Some("state_sp"));
    assert_eq!(
        r.fields[0],
        ("name".into(), FieldValue::Text("S\u{e3}o Paulo".into()))
    );
    assert_eq!(
        r.fields[1],
        ("country_id".into(), FieldValue::Ref("base.br".into()))
    );
}

#[test]
fn real_base_country_states_csv_parses() {
    let path = Path::new("../../odoo/odoo/addons/base/data/res.country.state.csv");
    if !path.exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }

    let source = std::fs::read_to_string(path).unwrap();
    let records = parse_csv_data("res.country.state", &source).unwrap();

    assert!(records.len() > 400, "got {}", records.len());
    assert!(records.iter().all(|r| r.xml_id.is_some()));
}

// ---------- applying to the database ----------

#[tokio::test]
async fn load_records_applies_refs_and_respects_noupdate() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.dcompany".into(),
            table: "rusdoo_test_dcompany".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.dcontact".into(),
            table: "rusdoo_test_dcontact".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.dcompany".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for table in ["rusdoo_test_dcontact", "rusdoo_test_dcompany"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("rusdoo.test.dcompany")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.dcontact")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let xml = r#"<odoo><data noupdate="1">
        <record id="acme" model="rusdoo.test.dcompany">
            <field name="name">Acme</field>
        </record>
        <record id="ana" model="rusdoo.test.dcontact">
            <field name="name">Ana</field>
            <field name="company_id" ref="acme"/>
        </record>
    </data></odoo>"#;
    let records = parse_xml_data(xml).unwrap();
    let mut xml_ids = XmlIds::new();

    // first load creates both, resolving the intra-module ref
    let stats = load_records(&pool, &reg, "demo", &records, &mut xml_ids)
        .await
        .unwrap();
    assert_eq!((stats.created, stats.updated, stats.skipped), (2, 0, 0));

    // reload with noupdate: nothing changes
    let stats = load_records(&pool, &reg, "demo", &records, &mut xml_ids)
        .await
        .unwrap();
    assert_eq!((stats.created, stats.updated, stats.skipped), (0, 0, 2));

    // an updatable version does write
    let updatable = parse_xml_data(
        r#"<odoo>
        <record id="acme" model="rusdoo.test.dcompany">
            <field name="name">Acme Renovada</field>
        </record>
    </odoo>"#,
    )
    .unwrap();
    let stats = load_records(&pool, &reg, "demo", &updatable, &mut xml_ids)
        .await
        .unwrap();
    assert_eq!((stats.created, stats.updated, stats.skipped), (0, 1, 0));

    let company_id = xml_ids.get("demo.acme").unwrap().1;
    let rows = reg
        .read(&pool, "rusdoo.test.dcompany", &[company_id], &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("Acme Renovada"));

    // the contact points at the company created by external id
    let contact_id = xml_ids.get("demo.ana").unwrap().1;
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.dcontact",
            &[contact_id],
            &["company_id"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["company_id"], json!(company_id));
}

#[tokio::test]
async fn unresolved_ref_is_a_clear_error() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.dcontact2".into(),
            table: "rusdoo_test_dcontact2".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new(
            "company_id",
            FieldType::Many2one {
                comodel: "rusdoo.test.dcompany".into(),
            },
        )],
    ))
    .unwrap();

    let xml = r#"<odoo><record id="x" model="rusdoo.test.dcontact2">
        <field name="company_id" ref="ghost.company"/>
    </record></odoo>"#;
    let records = parse_xml_data(xml).unwrap();
    let mut xml_ids = XmlIds::new();

    let err = load_records(&pool, &reg, "demo", &records, &mut xml_ids)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("ghost.company"));
}
