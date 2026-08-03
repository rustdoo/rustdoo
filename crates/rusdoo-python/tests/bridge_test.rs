//! The bridge, end to end: a model written in ordinary Odoo Python, and
//! records of it created and read through the Rust ORM.
//!
//! This is the proof issue #10 asks for before anything larger is built
//! on it. If a `models.py` cannot declare a model the Rust core serves,
//! the whole approach is wrong and it is better to find out here.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::registry::Registry;
use rusdoo_python::load_python_models;
use serde_json::json;

/// A model as an addon would really write it.
const ADDON: &str = r#"
from odoo import models, fields


class Instrument(models.Model):
    _name = "music.instrument"
    _description = "An instrument the shop sells"
    _order = "name, id"

    name = fields.Char("Name", required=True, translate=True)
    reference = fields.Char("Internal Reference", size=32)
    price = fields.Float("Price", digits=(10, 2))
    in_stock = fields.Boolean("In Stock", default=True)
    family = fields.Selection(
        [("string", "String"), ("wind", "Wind"), ("percussion", "Percussion")],
        "Family",
    )
    notes = fields.Text("Notes")
"#;

#[test]
fn a_model_written_in_python_lands_in_the_rust_registry() {
    let mut registry = Registry::new();
    let loaded = load_python_models(&mut registry, "music", ADDON).expect("the addon loads");

    assert_eq!(loaded, vec!["music.instrument"]);
    let model = registry.get("music.instrument").expect("the model is there");
    assert_eq!(model.meta.table, "music_instrument");
    assert_eq!(model.order(), "name, id", "_order crossed");

    // the fields crossed with their types and their flags
    let name = model.field("name").expect("name");
    assert!(name.required, "required=True crossed");
    assert!(name.translate, "translate=True crossed");
    let reference = model.field("reference").expect("reference");
    assert!(
        matches!(
            reference.ty,
            rusdoo_orm::fields::FieldType::Char { size: Some(32) }
        ),
        "size crossed: {:?}",
        reference.ty
    );
    let price = model.field("price").expect("price");
    assert!(
        matches!(
            price.ty,
            rusdoo_orm::fields::FieldType::Float {
                digits: Some((10, 2))
            }
        ),
        "digits crossed: {:?}",
        price.ty
    );
    assert_eq!(
        model.field("in_stock").and_then(|f| f.default.clone()),
        Some(json!(true)),
        "a constant default crossed"
    );
    let family = model.field("family").expect("family");
    match &family.ty {
        rusdoo_orm::fields::FieldType::Selection(options) => {
            assert_eq!(options.len(), 3);
            assert_eq!(options[0], ("string".into(), "String".into()));
        }
        other => panic!("selection did not cross: {other:?}"),
    }
}

#[test]
fn a_relation_between_two_python_models_crosses() {
    let mut registry = Registry::new();
    load_python_models(
        &mut registry,
        "band",
        r#"
from odoo import models, fields


class Band(models.Model):
    _name = "music.band"
    name = fields.Char(required=True)
    member_ids = fields.One2many("music.member", "band_id", "Members")


class Member(models.Model):
    _name = "music.member"
    name = fields.Char(required=True)
    band_id = fields.Many2one("music.band", "Band", required=True)
"#,
    )
    .expect("the addon loads");

    let band = registry.get("music.band").unwrap();
    match &band.field("member_ids").unwrap().ty {
        rusdoo_orm::fields::FieldType::One2many { comodel, inverse } => {
            assert_eq!(comodel, "music.member");
            assert_eq!(inverse, "band_id");
        }
        other => panic!("one2many did not cross: {other:?}"),
    }
    let member = registry.get("music.member").unwrap();
    assert!(matches!(
        &member.field("band_id").unwrap().ty,
        rusdoo_orm::fields::FieldType::Many2one { comodel } if comodel == "music.band"
    ));
}

#[test]
fn a_wizard_is_transient_and_an_inherit_extends() {
    let mut registry = Registry::new();
    load_python_models(
        &mut registry,
        "shop",
        r#"
from odoo import models, fields


class Instrument(models.Model):
    _name = "music.instrument"
    name = fields.Char(required=True)


class Discount(models.TransientModel):
    _name = "music.discount"
    percent = fields.Integer("Percent")


class InstrumentExtra(models.Model):
    _inherit = "music.instrument"
    warranty_months = fields.Integer("Warranty (months)")
"#,
    )
    .expect("the addon loads");

    assert!(
        registry.get("music.discount").unwrap().is_transient(),
        "a TransientModel crossed as one"
    );
    // the `_inherit` added a field to the model that was already there,
    // exactly as a Rust module's `_inherit` does
    let instrument = registry.get("music.instrument").unwrap();
    assert!(instrument.field("name").is_some(), "the original field stayed");
    assert!(
        instrument.field("warranty_months").is_some(),
        "the _inherit's field arrived"
    );
}

#[test]
fn a_broken_addon_says_where_it_broke() {
    let mut registry = Registry::new();
    let error = load_python_models(
        &mut registry,
        "broken",
        "from odoo import models\n\nclass X(models.Model)\n    _name = 'x'\n",
    )
    .expect_err("a syntax error is refused");
    let message = error.to_string();
    assert!(
        message.contains("SyntaxError"),
        "the Python error survives: {message}"
    );

    // and a field type this port has no column for is refused by name,
    // rather than installing a model with a hole in it
    let error = load_python_models(
        &mut registry,
        "unsupported",
        r#"
from odoo import models, fields


class X(models.Model):
    _name = "x.y"
    thing = fields.Field("Thing")
"#,
    )
    .expect_err("an unsupported field type is refused");
    assert!(
        error.to_string().contains("not supported yet"),
        "unexpected: {error}"
    );
}

/// The point of the whole exercise: records of a Python-declared model,
/// created and read through the Rust ORM with nothing Python-shaped left
/// in the path.
#[tokio::test]
async fn records_of_a_python_model_go_through_the_rust_orm_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_py_bridge") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut registry = Registry::new();
    load_python_models(&mut registry, "music", ADDON).expect("the addon loads");

    registry.init_tables(&pool).await.expect("the table is made");

    let id = registry
        .create(
            &pool,
            "music.instrument",
            vec![
                ("name", json!("Cello")),
                ("reference", json!("CEL-001")),
                ("price", json!(4200.50)),
                ("family", json!("string")),
            ],
        )
        .await
        .expect("a record is created");

    let rows = registry
        .read(
            &pool,
            "music.instrument",
            &[id],
            &["name", "reference", "price", "family", "in_stock"],
        )
        .await
        .expect("and read back");
    assert_eq!(rows[0]["name"], json!("Cello"));
    assert_eq!(rows[0]["reference"], json!("CEL-001"));
    assert_eq!(rows[0]["price"], json!(4200.50));
    assert_eq!(rows[0]["family"], json!("string"));
    assert_eq!(rows[0]["in_stock"], json!(true), "the default was applied");

    // required is the database's, not a promise made in Python
    let error = registry
        .create(&pool, "music.instrument", vec![("reference", json!("X"))])
        .await
        .expect_err("a nameless instrument is refused");
    assert!(error.to_string().contains("name"), "unexpected: {error}");

    // and the model's `_order` is what a search with no order gets
    for name in ["Viola", "Bass"] {
        registry
            .create(&pool, "music.instrument", vec![("name", json!(name))])
            .await
            .unwrap();
    }
    let found = registry
        .search(
            &pool,
            "music.instrument",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    let names: Vec<String> = registry
        .read(&pool, "music.instrument", &found, &["name"])
        .await
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["Bass", "Cello", "Viola"], "_order held");

    sqlx::query("DROP TABLE IF EXISTS music_instrument")
        .execute(&pool)
        .await
        .unwrap();
}

/// The half that makes a bridge a bridge: Python holding records and
/// asking the database about them, through `self.env` and a recordset.
#[tokio::test(flavor = "multi_thread")]
async fn python_reads_and_writes_records_through_a_recordset_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_py_records") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut registry = Registry::new();
    load_python_models(
        &mut registry,
        "shop",
        r#"
from odoo import models, fields


class Band(models.Model):
    _name = "music.band"
    _order = "name, id"
    name = fields.Char(required=True)
    member_ids = fields.One2many("music.member", "band_id")


class Member(models.Model):
    _name = "music.member"
    _order = "name, id"
    name = fields.Char(required=True)
    instrument = fields.Char()
    band_id = fields.Many2one("music.band", required=True)
"#,
    )
    .expect("the addon loads");
    registry.init_tables(&pool).await.expect("the tables are made");

    let registry = std::sync::Arc::new(registry);
    // everything below runs as uid 1, the way a call from the client
    // would run as whoever is logged in
    let answer: String = rusdoo_python::with_environment(registry.clone(), pool.clone(), 1, || {
        rusdoo_python::run_python(
            "script",
            r#"
from odoo import api

env = api.Environment()

# create, through the ORM in Rust
band = env["music.band"].create({"name": "Trio"})
for who, what in [("Ana", "cello"), ("Bia", "violin"), ("Caio", "viola")]:
    env["music.member"].create({"name": who, "instrument": what, "band_id": band.id})

# read a field off a single record
one = env["music.member"].search([["name", "=", "Ana"]])
first_instrument = one.instrument

# the whole set, in the model's own order
everyone = env["music.member"].search([])
names = everyone.mapped("name")

# filtered and sorted, as an addon writes them
strings = everyone.filtered(lambda r: r.instrument != "violin").mapped("name")
backwards = everyone.sorted(key=lambda r: r.name, reverse=True).mapped("name")

# writing goes back to the database, and the cache does not lie about it
one.write({"instrument": "double bass"})
after_write = one.instrument

# a dotted mapped walks the relation
band_names = everyone.mapped("band_id.name")

# and unlink really removes
env["music.member"].search([["name", "=", "Caio"]]).unlink()
left = env["music.member"].search_count([])

result = {
    "first_instrument": first_instrument,
    "names": names,
    "strings": strings,
    "backwards": backwards,
    "after_write": after_write,
    "band_names": band_names,
    "left": left,
    "band_repr": repr(band),
    "uid": env.uid,
}
"#,
            |_py, module| {
                use pyo3::prelude::PyAnyMethods;
                Ok(module.as_any().getattr("result")?.to_string())
            },
        )
        .expect("the script runs")
    });

    // the shape python built, checked against what the database holds
    assert!(answer.contains("'first_instrument': 'cello'"), "{answer}");
    assert!(
        answer.contains("'names': ['Ana', 'Bia', 'Caio']"),
        "the model's _order held for a python search: {answer}"
    );
    assert!(
        answer.contains("'strings': ['Ana', 'Caio']"),
        "filtered ran the predicate against real fields: {answer}"
    );
    assert!(
        answer.contains("'backwards': ['Caio', 'Bia', 'Ana']"),
        "{answer}"
    );
    assert!(
        answer.contains("'after_write': 'double bass'"),
        "the write reached the database and the cache was dropped: {answer}"
    );
    assert!(
        answer.contains("'band_names': ['Trio', 'Trio', 'Trio']"),
        "a dotted mapped walked the many2one: {answer}"
    );
    assert!(answer.contains("'left': 2"), "unlink removed one: {answer}");
    assert!(answer.contains("'uid': 1"), "{answer}");

    // and the database agrees, read from Rust
    let left = registry
        .search(
            &pool,
            "music.member",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(left.len(), 2);

    for table in ["music_member", "music_band"] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {table} CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
    }
}
