//! Reflection: the registry describing itself into the database.
//!
//! Port of `ir.model._reflect_models` and `ir.model.fields._reflect_model`.
//! The registry is built from code — Rust here, Python there — and then
//! it writes down what it is: one `ir.model` row per model, one
//! `ir.model.fields` row per field. Half of what an ERP does is talk
//! about its own shape, and everything that does needs to read it from
//! somewhere that is not the compiler.
//!
//! It also publishes the external ids Odoo publishes, and that is the
//! part that matters most today: `base.model_res_partner`,
//! `sale.field_sale_order__partner_id`. Odoo's own data files point at
//! those — `<field name="model_id" ref="base.model_res_partner"/>` — and
//! until now this port had to guess which model such a name meant by
//! spelling every registered model's name with underscores and comparing.
//! With the rows there, the guess becomes a lookup.
//!
//! Reflection runs on **every boot**, not only on `--init`: a model is
//! code, and code is not installed into a database. A server whose binary
//! grew a model since last time has to say so, and one that lost a model
//! has to stop claiming it.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::Model;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

use crate::installer::XmlIds;

/// The module every reflected row is published under.
///
/// Odoo publishes a model's external id under the module that *declared*
/// it, which it knows because it is loading that module's Python at the
/// time. This port registers every model of the binary at boot, together,
/// with no such moment — so they are published under `base`, which is
/// where a lookup that does not care about provenance will find them, and
/// which is where `base.model_res_partner` lives in Odoo too.
const MODULE: &str = "base";

/// Odoo's wire name for a field type — the `ttype` column, which its own
/// data and every module that reads metadata compare against.
fn ttype(field: &Field) -> &'static str {
    match &field.ty {
        FieldType::Boolean => "boolean",
        FieldType::Integer => "integer",
        FieldType::Float { .. } => "float",
        FieldType::Monetary => "monetary",
        FieldType::Char { .. } => "char",
        FieldType::Text => "text",
        FieldType::Html => "html",
        FieldType::Date => "date",
        FieldType::Datetime => "datetime",
        FieldType::Binary => "binary",
        FieldType::Selection(_) => "selection",
        FieldType::Many2one { .. } => "many2one",
        FieldType::One2many { .. } => "one2many",
        FieldType::Many2many { .. } => "many2many",
        FieldType::Json => "json",
    }
}

/// What a relational field points at, and through what.
fn relation(field: &Field) -> (Option<String>, Option<String>) {
    match &field.ty {
        FieldType::Many2one { comodel } | FieldType::Many2many { comodel, .. } => {
            (Some(comodel.clone()), None)
        }
        FieldType::One2many { comodel, inverse } => {
            (Some(comodel.clone()), Some(inverse.clone()))
        }
        _ => (None, None),
    }
}

/// The external id Odoo gives a model: `model_` plus its name with the
/// dots turned into underscores.
pub fn model_external_id(model: &str) -> String {
    format!("model_{}", model.replace('.', "_"))
}

/// And a field's: `field_<model>__<field>`.
pub fn field_external_id(model: &str, field: &str) -> String {
    format!("field_{}__{field}", model.replace('.', "_"))
}

/// A readable name for a model that never declared one — the same shape
/// Odoo shows before somebody writes a description.
fn humanize(name: &str) -> String {
    name.split('.')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write the registry into `ir.model` / `ir.model.fields`, and publish
/// the external ids for both.
///
/// Idempotent by construction: rows are matched by their technical name,
/// updated when they exist and created when they do not, and rows for
/// models the registry no longer has are removed. Running it twice
/// changes nothing the second time, which is what lets it run on every
/// boot.
pub async fn reflect(
    registry: &Registry,
    pool: &PgPool,
    xml_ids: &mut XmlIds,
) -> Result<usize, RusdooError> {
    // a database that never ran an install has no tables to describe
    // itself into, and a boot must not fail for asking
    let ready: bool = sqlx::query_scalar("SELECT to_regclass('ir_model') IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap_or(false);
    if !ready {
        return Ok(0);
    }
    if registry.get("ir.model").is_none() || registry.get("ir.model.fields").is_none() {
        // a build without the reflection models describes nothing, which
        // is not a failure — it is a server that cannot answer questions
        // about itself
        return Ok(0);
    }

    let mut described: Vec<&Model> = registry
        .models()
        .filter(|model| model.meta.name != "ir.model" && model.meta.name != "ir.model.fields")
        .collect();
    // a stable order, so two boots write the same rows in the same order
    // and a diff of the table means something
    described.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));

    let known = existing_models(registry, pool).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut written = 0usize;

    for model in &described {
        let name = model.meta.name.clone();
        seen.insert(name.clone());
        let values = vec![
            ("model", json!(name)),
            ("name", json!(humanize(&name))),
            ("state", json!("base")),
            ("transient", json!(model.is_transient())),
        ];
        let id = match known.get(&name) {
            Some(id) => {
                registry.write(pool, "ir.model", &[*id], values).await?;
                *id
            }
            None => registry.create(pool, "ir.model", values).await?,
        };
        publish(
            pool,
            xml_ids,
            &model_external_id(&name),
            "ir.model",
            id,
        )
        .await?;
        written += 1;
        reflect_fields(registry, pool, xml_ids, model, id).await?;
    }

    // a model the registry no longer has stops being described: its
    // fields go with it (`ondelete=cascade` on `model_id`)
    let gone: Vec<i64> = known
        .iter()
        .filter(|(name, _)| !seen.contains(*name))
        .map(|(_, id)| *id)
        .collect();
    if !gone.is_empty() {
        registry
            .unlink_as(pool, rusdoo_core::SUPERUSER_ID, "ir.model", &gone)
            .await?;
    }
    Ok(written)
}

/// Record an external id both in memory and in `ir_model_data`, so the
/// next boot — and every `ref=` in every data file — finds it.
///
/// `noupdate` is false on purpose: these rows describe code, and code is
/// what changes between boots. A reflected id that stopped matching its
/// row would be worse than no id at all.
async fn publish(
    pool: &PgPool,
    xml_ids: &mut XmlIds,
    name: &str,
    model: &str,
    id: i64,
) -> Result<(), RusdooError> {
    xml_ids.insert(format!("{MODULE}.{name}"), model.to_string(), id);
    sqlx::query(
        r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id", "noupdate")
           VALUES ($1, $2, $3, $4, false)
           ON CONFLICT ("module", "name") DO UPDATE
           SET "res_id" = EXCLUDED."res_id", "model" = EXCLUDED."model""#,
    )
    .bind(MODULE)
    .bind(name)
    .bind(model)
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| RusdooError::Database(error.to_string()))?;
    Ok(())
}

/// `model name -> row id`, for the models already described.
async fn existing_models(
    registry: &Registry,
    pool: &PgPool,
) -> Result<HashMap<String, i64>, RusdooError> {
    let ids = registry
        .search(pool, "ir.model", &parse_domain(&json!([]))?, &SearchOptions::default())
        .await?;
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = registry.read(pool, "ir.model", &ids, &["model"]).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get("model")?.as_str()?.to_string(),
                row.get("id")?.as_i64()?,
            ))
        })
        .collect())
}

/// The fields of one model, described and published.
async fn reflect_fields(
    registry: &Registry,
    pool: &PgPool,
    xml_ids: &mut XmlIds,
    model: &Model,
    model_id: i64,
) -> Result<(), RusdooError> {
    let ids = registry
        .search(
            pool,
            "ir.model.fields",
            &parse_domain(&json!([["model", "=", model.meta.name]]))?,
            &SearchOptions::default(),
        )
        .await?;
    let known: HashMap<String, i64> = if ids.is_empty() {
        HashMap::new()
    } else {
        registry
            .read(pool, "ir.model.fields", &ids, &["name"])
            .await?
            .iter()
            .filter_map(|row| Some((row.get("name")?.as_str()?.to_string(), row.get("id")?.as_i64()?)))
            .collect()
    };

    let mut seen: HashSet<String> = HashSet::new();
    for field in model.fields() {
        seen.insert(field.name.clone());
        let (relation, relation_field) = relation(field);
        let values = vec![
            ("name", json!(field.name)),
            ("model", json!(model.meta.name)),
            ("model_id", json!(model_id)),
            ("field_description", json!(humanize(&field.name))),
            ("ttype", json!(ttype(field))),
            ("relation", json!(relation)),
            ("relation_field", json!(relation_field)),
            ("required", json!(field.required)),
            ("readonly", json!(field.readonly)),
            ("store", json!(field.stored)),
            ("translate", json!(field.translate)),
            ("related", json!(field.related)),
            ("is_computed", json!(field.compute.is_some())),
            ("state", json!("base")),
        ];
        let id = match known.get(&field.name) {
            Some(id) => {
                registry
                    .write(pool, "ir.model.fields", &[*id], values)
                    .await?;
                *id
            }
            None => registry.create(pool, "ir.model.fields", values).await?,
        };
        publish(
            pool,
            xml_ids,
            &field_external_id(&model.meta.name, &field.name),
            "ir.model.fields",
            id,
        )
        .await?;
    }

    let gone: Vec<i64> = known
        .iter()
        .filter(|(name, _)| !seen.contains(*name))
        .map(|(_, id)| *id)
        .collect();
    if !gone.is_empty() {
        registry
            .unlink_as(pool, rusdoo_core::SUPERUSER_ID, "ir.model.fields", &gone)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_external_ids_are_the_ones_odoos_data_points_at() {
        assert_eq!(model_external_id("res.partner"), "model_res_partner");
        assert_eq!(model_external_id("res.partner.bank"), "model_res_partner_bank");
        assert_eq!(
            field_external_id("sale.order", "partner_id"),
            "field_sale_order__partner_id"
        );
    }

    #[test]
    fn a_relational_field_says_what_it_points_at_and_through_what() {
        let m2o = Field::new(
            "partner_id",
            FieldType::Many2one {
                comodel: "res.partner".into(),
            },
        );
        assert_eq!(ttype(&m2o), "many2one");
        assert_eq!(relation(&m2o), (Some("res.partner".into()), None));

        let o2m = Field::new(
            "order_line",
            FieldType::One2many {
                comodel: "sale.order.line".into(),
                inverse: "order_id".into(),
            },
        );
        assert_eq!(ttype(&o2m), "one2many");
        assert_eq!(
            relation(&o2m),
            (Some("sale.order.line".into()), Some("order_id".into()))
        );

        // and a scalar points at nothing
        let plain = Field::new("name", FieldType::Char { size: None });
        assert_eq!(ttype(&plain), "char");
        assert_eq!(relation(&plain), (None, None));
    }

    #[test]
    fn a_model_with_no_description_still_reads_like_something() {
        assert_eq!(humanize("res.partner"), "Res Partner");
        assert_eq!(humanize("sale.order.line"), "Sale Order Line");
    }
}
