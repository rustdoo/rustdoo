//! Applying parsed data records to the database, port of the record
//! creation in `odoo/tools/convert.py` + the external-id bookkeeping
//! of `ir.model.data` (in-memory until the base models are ported).

use crate::data::{parse_csv_data, parse_xml_data, DataRecord, FieldValue};
use crate::graph::dependency_order;
use crate::loader::discover_addons;
use crate::manifest::Manifest;
use rusdoo_core::RusdooError;
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

impl XmlIds {
    pub fn new() -> Self {
        Self::default()
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
    let mut tx: Transaction<'static, Postgres> = pool
        .begin()
        .await
        .map_err(|e| RusdooError::Database(e.to_string()))?;
    let mut staged: HashMap<String, (String, i64)> = HashMap::new();
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
                        staged.insert(key, (record.model.clone(), id));
                        stats.created += 1;
                    }
                }
            }
        }
    }

    tx.commit()
        .await
        .map_err(|e| RusdooError::Database(e.to_string()))?;
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
    registry: &Registry,
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
