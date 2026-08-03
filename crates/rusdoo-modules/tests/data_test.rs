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
    let Ok(_url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_testing::pool_in("rusdoo_data_test_load_records_applies_refs_and_resp").unwrap();
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
    // many2one reads as [id, display_name] (name_get)
    assert_eq!(rows[0]["company_id"], json!([company_id, "Acme Renovada"]));
}

#[tokio::test]
async fn unresolved_ref_is_a_clear_error() {
    let Ok(_url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_testing::pool_in("rusdoo_data_test_unresolved_ref_is_a_clear_error").unwrap();
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

// ---------- review regressions ----------

#[test]
fn root_noupdate_attribute_is_honored() {
    // <odoo noupdate="1"> is THE idiomatic pattern (162 files in base)
    let src = r#"<odoo noupdate="1"><record id="x" model="m">
        <field name="n">1</field></record></odoo>"#;

    assert!(parse_xml_data(src).unwrap()[0].noupdate);
}

#[test]
fn nested_data_inherits_the_enclosing_noupdate() {
    let src = r#"<odoo noupdate="1"><data><record id="x" model="m"/></data></odoo>"#;

    assert!(parse_xml_data(src).unwrap()[0].noupdate);
}

#[test]
fn typed_text_fields_become_numbers() {
    let src = r#"<odoo><record id="x" model="m">
        <field name="sequence" type="int">30</field>
        <field name="rate" type="float">1.5</field>
    </record></odoo>"#;

    let record = &parse_xml_data(src).unwrap()[0];

    assert_eq!(record.fields[0].1, FieldValue::Eval(json!(30)));
    assert_eq!(record.fields[1].1, FieldValue::Eval(json!(1.5)));
}

#[test]
fn text_content_is_verbatim_like_odoo() {
    let src = r#"<odoo><record id="x" model="m"><field name="d">  x  </field></record></odoo>"#;

    let record = &parse_xml_data(src).unwrap()[0];

    assert_eq!(record.fields[0].1, FieldValue::Text("  x  ".into()));
}

#[tokio::test]
async fn failed_load_rolls_back_the_whole_file() {
    let Ok(_url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_testing::pool_in("rusdoo_data_test_failed_load_rolls_back_the_whole_f").unwrap();
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.rbco".into(),
            table: "rusdoo_test_rbco".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "boss_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.rbco".into(),
                },
            ),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_rbco""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.rbco")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // first record is fine, second has an unresolvable ref
    let xml = r#"<odoo>
        <record id="good" model="rusdoo.test.rbco"><field name="name">Boa</field></record>
        <record id="bad" model="rusdoo.test.rbco"><field name="boss_id" ref="ghost.nope"/></record>
    </odoo>"#;
    let records = parse_xml_data(xml).unwrap();
    let mut xml_ids = XmlIds::new();

    let err = load_records(&pool, &reg, "demo", &records, &mut xml_ids)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("ghost.nope"), "erro inesperado: {err}");
    // the whole file rolled back: no rows, no published external ids
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "rusdoo_test_rbco""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(xml_ids.get("demo.good").is_none());
}

#[test]
fn extending_existing_model_via_ir_model_is_rejected() {
    use rusdoo_modules::installer::apply_model_definitions;
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.partner".into(),
            table: "res_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    let records = parse_xml_data(
        r#"<odoo><record id="m" model="ir.model">
            <field name="model">res.partner</field></record></odoo>"#,
    )
    .unwrap();
    assert!(apply_model_definitions(&mut reg, &records).is_err());
}

#[test]
fn reserved_field_names_are_rejected() {
    use rusdoo_modules::installer::apply_model_definitions;
    let mut reg = Registry::new();
    let records = parse_xml_data(
        r#"<odoo>
        <record id="m" model="ir.model"><field name="model">x_demo.thing</field></record>
        <record id="f" model="ir.model.fields">
            <field name="model_id" ref="m"/>
            <field name="name">id</field>
            <field name="ttype">integer</field>
        </record></odoo>"#,
    )
    .unwrap();
    assert!(apply_model_definitions(&mut reg, &records).is_err());
}

#[test]
fn field_markup_captured_as_text() {
    // a <field> containing child markup keeps the inner XML as a string
    let src = r#"<odoo><record id="v" model="ir.ui.view">
        <field name="name">my view</field>
        <field name="arch" type="xml">
            <form><field name="partner_id"/><group>Oi</group></form>
        </field>
    </record></odoo>"#;

    let record = &parse_xml_data(src).unwrap()[0];

    assert_eq!(record.fields[0].1, FieldValue::Text("my view".into()));
    let arch = match &record.fields[1].1 {
        FieldValue::Text(t) => t,
        other => panic!("expected text arch, got {other:?}"),
    };
    assert!(arch.starts_with("<form>"));
    assert!(arch.contains(r#"<field name="partner_id"/>"#));
    assert!(arch.ends_with("</form>"));
}

#[test]
fn markup_close_tag_with_whitespace() {
    // </field > (legal XML) must not leak a stray '<' into the markup
    let src = "<odoo><record id=\"v\" model=\"ir.ui.view\">\
        <field name=\"arch\" type=\"xml\"><form><b/></form></field ></record></odoo>";

    let record = &parse_xml_data(src).unwrap()[0];

    match &record.fields[0].1 {
        FieldValue::Text(t) => assert_eq!(t, "<form><b/></form>"),
        other => panic!("expected clean markup, got {other:?}"),
    }
}

#[test]
fn a_view_patch_arch_survives_the_loader() {
    // a view inheritance's arch is `<data>` with a real `<field>`
    // inside: neither belongs to the record, and both were already
    // lidos como se fossem
    let src = r#"<odoo><record id="v" model="ir.ui.view">
        <field name="inherit_id" ref="account.view_move_form"/>
        <field name="arch" type="xml"><data><xpath expr="//button[@name='action_draft']" position="after"><button name="x"/></xpath><field name="invoice_origin" position="after"><field name="debit_origin_id"/></field></data></field>
    </record></odoo>"#;

    let record = &parse_xml_data(src).unwrap()[0];

    assert_eq!(record.fields.len(), 2, "dois campos, não cinco");
    assert_eq!(
        record.fields[0].1,
        FieldValue::Ref("account.view_move_form".into())
    );
    let arch = match &record.fields[1].1 {
        FieldValue::Text(t) => t,
        other => panic!("expected text arch, got {other:?}"),
    };
    assert!(arch.starts_with("<data>"), "{arch}");
    assert!(arch.ends_with("</data>"), "{arch}");
    assert!(arch.contains(r#"<field name="debit_origin_id"/>"#), "{arch}");
}

#[test]
fn access_csv_on_unknown_model_errors() {
    use rusdoo_modules::installer::{apply_access_records, XmlIds};
    use rusdoo_orm::access::AccessControl;

    let reg = Registry::new(); // no models registered
    let rec = rusdoo_modules::data::DataRecord {
        xml_id: Some("acc".into()),
        model: "ir.model.access".into(),
        fields: vec![("model".into(), FieldValue::Text("ghost.model".into()))],
        noupdate: false,
    };
    let mut ac = AccessControl::new();

    let err = apply_access_records(&mut ac, &reg, &[rec], "m", &XmlIds::new()).unwrap_err();

    assert!(err.to_string().contains("unknown model"));
}
