//! Applying parsed data records to the database, port of the record
//! creation in `odoo/tools/convert.py` + the external-id bookkeeping
//! of `ir.model.data` (in-memory until the base models are ported).

use crate::data::{parse_csv_data, parse_xml_data, DataRecord, FieldValue};
use crate::graph::dependency_order;
use crate::loader::discover_addons;
use crate::manifest::Manifest;
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashMap;
use std::path::Path;

/// External id -> (model, database id). The Rust side of `ir.model.data`.
#[derive(Debug, Default)]
pub struct XmlIds {
    map: HashMap<String, (String, i64)>,
}

/// Persistence table for external ids, the Rust `ir.model.data`.
const IR_MODEL_DATA_DDL: &str = r#"CREATE TABLE IF NOT EXISTS "ir_model_data" ("id" SERIAL NOT NULL, "module" varchar NOT NULL, "name" varchar NOT NULL, "model" varchar NOT NULL, "res_id" int4 NOT NULL, "noupdate" bool NOT NULL DEFAULT false, PRIMARY KEY("id"), UNIQUE("module", "name"))"#;

fn db_err(e: sqlx::Error) -> RusdooError {
    RusdooError::Database(e.to_string())
}

impl XmlIds {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every persisted external id (creating the table on first
    /// use), so a fresh process resumes where the last boot stopped.
    pub async fn load(pool: &PgPool) -> Result<Self, RusdooError> {
        sqlx::query(IR_MODEL_DATA_DDL)
            .execute(pool)
            .await
            .map_err(db_err)?;
        let rows = sqlx::query_as::<_, (String, String, String, i32)>(
            r#"SELECT "module", "name", "model", "res_id" FROM "ir_model_data""#,
        )
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
        let map = rows
            .into_iter()
            .map(|(module, name, model, res_id)| {
                (format!("{module}.{name}"), (model, i64::from(res_id)))
            })
            .collect();
        Ok(XmlIds { map })
    }

    pub fn get(&self, qualified: &str) -> Option<&(String, i64)> {
        self.map.get(qualified)
    }

    fn insert(&mut self, qualified: String, model: String, id: i64) {
        self.map.insert(qualified, (model, id));
    }
}

/// `ref="x"` inside module `demo` means `demo.x`.
fn qualify(module: &str, xml_id: &str) -> String {
    if xml_id.contains('.') {
        xml_id.to_string()
    } else {
        format!("{module}.{xml_id}")
    }
}

#[derive(Debug, Default)]
pub struct LoadStats {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
}

/// Apply records in file order, atomically: the whole call runs in one
/// transaction (Odoo rolls a failing data file back the same way), and
/// external ids are only published to `xml_ids` after commit. Create
/// when the external id is new, update when it exists (unless
/// noupdate), resolve `ref`s through the accumulated external ids.
///
/// Known divergence: id-less records inside noupdate scopes are always
/// created; Odoo skips them in update mode, which does not exist here
/// yet (no persisted ir.model.data / install state).
pub async fn load_records(
    pool: &PgPool,
    registry: &Registry,
    module: &str,
    records: &[DataRecord],
    xml_ids: &mut XmlIds,
) -> Result<LoadStats, RusdooError> {
    let mut tx: Transaction<'static, Postgres> = pool.begin().await.map_err(db_err)?;
    sqlx::query(IR_MODEL_DATA_DDL)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    let mut staged: HashMap<String, (String, i64)> = HashMap::new();
    let mut staged_noupdate: HashMap<String, bool> = HashMap::new();
    let mut stats = LoadStats::default();

    for record in records {
        let mut pairs: Vec<(&str, Value)> = Vec::new();
        for (name, field_value) in &record.fields {
            let value = match field_value {
                FieldValue::Text(text) => Value::String(text.clone()),
                FieldValue::Eval(value) => value.clone(),
                FieldValue::Ref(xml_id) => {
                    let key = qualify(module, xml_id);
                    let (_, id) =
                        staged
                            .get(&key)
                            .or_else(|| xml_ids.get(&key))
                            .ok_or_else(|| {
                                RusdooError::Validation(format!(
                                    "unresolved external id {key} (field {name:?} on {})",
                                    record.model
                                ))
                            })?;
                    Value::from(*id)
                }
            };
            pairs.push((name.as_str(), value));
        }

        match &record.xml_id {
            None => {
                registry.create_tx(&mut tx, &record.model, pairs).await?;
                stats.created += 1;
            }
            Some(xml_id) => {
                let key = qualify(module, xml_id);
                let existing = staged.get(&key).or_else(|| xml_ids.get(&key)).cloned();
                match existing {
                    Some(_) if record.noupdate => stats.skipped += 1,
                    Some((_, existing_id)) => {
                        registry
                            .write_tx(&mut tx, &record.model, &[existing_id], pairs)
                            .await?;
                        stats.updated += 1;
                    }
                    None => {
                        // a foreign-module id that does not exist is a bug
                        // in the data file, not a record to create
                        if !key.starts_with(&format!("{module}.")) {
                            return Err(RusdooError::Validation(format!(
                                "cannot update missing record {key} (referenced from module {module})"
                            )));
                        }
                        let id = registry.create_tx(&mut tx, &record.model, pairs).await?;
                        staged_noupdate.insert(key.clone(), record.noupdate);
                        staged.insert(key, (record.model.clone(), id));
                        stats.created += 1;
                    }
                }
            }
        }
    }

    // persist the new external ids inside the same transaction
    for (key, (model, id)) in &staged {
        let (module_part, name_part) = key
            .split_once('.')
            .ok_or_else(|| RusdooError::Validation(format!("unqualified external id: {key}")))?;
        let noupdate = staged_noupdate.get(key).copied().unwrap_or(false);
        sqlx::query(
            r#"INSERT INTO "ir_model_data" ("module", "name", "model", "res_id", "noupdate")
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT ("module", "name") DO UPDATE
               SET "res_id" = EXCLUDED."res_id", "model" = EXCLUDED."model""#,
        )
        .bind(module_part)
        .bind(name_part)
        .bind(model)
        .bind(*id)
        .bind(noupdate)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    for (key, entry) in staged {
        xml_ids.insert(key, entry.0, entry.1);
    }
    Ok(stats)
}

/// Per-module load statistics of one boot.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub modules: Vec<(String, LoadStats)>,
}

/// The integrated boot, port of `odoo/modules/loading.py`:
/// initialize the schema for every registered model, discover the
/// addons, and load each installable module's data files in
/// dependency order.
pub async fn install_modules(
    pool: &PgPool,
    registry: &mut Registry,
    addons_paths: &[&Path],
    xml_ids: &mut XmlIds,
) -> Result<InstallReport, RusdooError> {
    for model in registry.models() {
        model.init_table(pool).await?;
    }
    let manifests = discover_addons(addons_paths)?;
    let order = dependency_order(&manifests)?;
    let by_name: HashMap<&str, &Manifest> =
        manifests.iter().map(|m| (m.name.as_str(), m)).collect();

    let mut report = InstallReport::default();
    for name in &order {
        let manifest = by_name[name.as_str()];
        if !manifest.installable {
            continue;
        }
        let mut totals = LoadStats::default();
        for data_file in &manifest.data {
            let file_path = manifest.path.join(data_file);
            let source = std::fs::read_to_string(&file_path).map_err(|e| {
                RusdooError::Validation(format!("cannot read {}: {e}", file_path.display()))
            })?;
            let records = if data_file.ends_with(".xml") {
                parse_xml_data(&source)
                    .map_err(|e| RusdooError::Validation(format!("{}: {e}", file_path.display())))?
            } else if data_file.ends_with(".csv") {
                let model = Path::new(data_file)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "csv data file without a model name: {data_file}"
                        ))
                    })?;
                parse_csv_data(model, &source)?
            } else {
                tracing::warn!("skipping unsupported data file {}", file_path.display());
                continue;
            };
            // ir.model / ir.model.fields records define models in the
            // registry before any data touches them
            let (records, new_models) = apply_model_definitions(registry, &records)?;
            for model_name in &new_models {
                registry
                    .get(model_name)
                    .expect("registered just above")
                    .init_table(pool)
                    .await?;
            }
            let stats = load_records(pool, registry, name, &records, xml_ids).await?;
            totals.created += stats.created;
            totals.updated += stats.updated;
            totals.skipped += stats.skipped;
        }
        tracing::info!(
            "module {name}: {} created, {} updated, {} skipped",
            totals.created,
            totals.updated,
            totals.skipped
        );
        report.modules.push((name.clone(), totals));
    }
    Ok(report)
}

fn record_field<'a>(record: &'a DataRecord, name: &str) -> Option<&'a FieldValue> {
    record
        .fields
        .iter()
        .find(|(field_name, _)| field_name == name)
        .map(|(_, value)| value)
}

fn text_of(value: Option<&FieldValue>) -> Option<String> {
    match value {
        Some(FieldValue::Text(text)) => Some(text.clone()),
        Some(FieldValue::Eval(Value::String(text))) => Some(text.clone()),
        _ => None,
    }
}

fn bool_of(value: Option<&FieldValue>) -> bool {
    matches!(value, Some(FieldValue::Eval(Value::Bool(true))))
        | matches!(value, Some(FieldValue::Text(t)) if t == "True" || t == "1")
}

fn field_from_ttype(
    name: &str,
    ttype: &str,
    relation: Option<String>,
    relation_field: Option<String>,
    required: bool,
) -> Result<Field, RusdooError> {
    use FieldType::*;
    let missing = |what: &str| {
        RusdooError::Validation(format!(
            "ir.model.fields {name:?} ({ttype}) requires '{what}'"
        ))
    };
    let ty = match ttype {
        "char" => Char { size: None },
        "text" => Text,
        "html" => Html,
        "integer" => Integer,
        "float" => Float { digits: None },
        "boolean" => Boolean,
        "date" => Date,
        "datetime" => Datetime,
        "many2one" => Many2one {
            comodel: relation.ok_or_else(|| missing("relation"))?,
        },
        "one2many" => One2many {
            comodel: relation.ok_or_else(|| missing("relation"))?,
            inverse: relation_field.ok_or_else(|| missing("relation_field"))?,
        },
        other => {
            return Err(RusdooError::Validation(format!(
                "ir.model.fields ttype {other:?} not yet supported"
            )))
        }
    };
    let field = Field::new(name, ty);
    Ok(if required { field.required() } else { field })
}

/// Consume `ir.model` / `ir.model.fields` records, registering the
/// declared models (or extending existing ones); everything else is
/// returned for normal data loading. Returns the touched model names.
fn apply_model_definitions(
    registry: &mut Registry,
    records: &[DataRecord],
) -> Result<(Vec<DataRecord>, Vec<String>), RusdooError> {
    struct PendingModel {
        tech_name: String,
        fields: Vec<Field>,
    }
    let mut remaining = Vec::new();
    let mut model_xmlids: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<PendingModel> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();

    for record in records {
        match record.model.as_str() {
            "ir.model" => {
                let tech_name = text_of(record_field(record, "model")).ok_or_else(|| {
                    RusdooError::Validation("ir.model record requires a 'model' field".into())
                })?;
                if let Some(xml_id) = &record.xml_id {
                    model_xmlids.insert(xml_id.clone(), tech_name.clone());
                }
                index_of.insert(tech_name.clone(), pending.len());
                pending.push(PendingModel {
                    tech_name,
                    fields: Vec::new(),
                });
            }
            "ir.model.fields" => {
                let Some(FieldValue::Ref(model_ref)) = record_field(record, "model_id") else {
                    return Err(RusdooError::Validation(
                        "ir.model.fields requires model_id ref".into(),
                    ));
                };
                let tech_name = model_xmlids.get(model_ref).ok_or_else(|| {
                    RusdooError::Validation(format!(
                        "ir.model.fields model_id ref {model_ref:?} does not match an \
                         ir.model record in this module"
                    ))
                })?;
                let name = text_of(record_field(record, "name")).ok_or_else(|| {
                    RusdooError::Validation("ir.model.fields requires 'name'".into())
                })?;
                let ttype = text_of(record_field(record, "ttype")).ok_or_else(|| {
                    RusdooError::Validation("ir.model.fields requires 'ttype'".into())
                })?;
                let field = field_from_ttype(
                    &name,
                    &ttype,
                    text_of(record_field(record, "relation")),
                    text_of(record_field(record, "relation_field")),
                    bool_of(record_field(record, "required")),
                )?;
                let index = index_of[tech_name.as_str()];
                pending[index].fields.push(field);
            }
            _ => remaining.push(record.clone()),
        }
    }

    let mut touched = Vec::new();
    for model_def in pending {
        if registry.get(&model_def.tech_name).is_some() {
            // in-place extension keeps the existing table
            registry.register(Model::new(
                ModelMeta {
                    name: model_def.tech_name.clone(),
                    table: String::new(),
                    inherit: vec![model_def.tech_name.clone()],
                    inherits: vec![],
                },
                model_def.fields,
            ))?;
        } else {
            let table = model_def.tech_name.replace('.', "_");
            registry.register(Model::new(
                ModelMeta {
                    name: model_def.tech_name.clone(),
                    table,
                    inherit: vec![],
                    inherits: vec![],
                },
                model_def.fields,
            ))?;
        }
        touched.push(model_def.tech_name);
    }
    Ok((remaining, touched))
}
