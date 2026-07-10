//! Applying parsed data records to the database, port of the record
//! creation in `odoo/tools/convert.py` + the external-id bookkeeping
//! of `ir.model.data` (in-memory until the base models are ported).

use crate::data::{DataRecord, FieldValue};
use rusdoo_core::RusdooError;
use rusdoo_orm::registry::Registry;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;

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

/// Apply records in file order: create when the external id is new,
/// update when it exists (unless noupdate), resolve `ref`s through the
/// accumulated external ids.
pub async fn load_records(
    pool: &PgPool,
    registry: &Registry,
    module: &str,
    records: &[DataRecord],
    xml_ids: &mut XmlIds,
) -> Result<LoadStats, RusdooError> {
    let mut stats = LoadStats::default();
    for record in records {
        let mut values: Vec<(String, Value)> = Vec::new();
        for (name, field_value) in &record.fields {
            let value = match field_value {
                FieldValue::Text(text) => Value::String(text.clone()),
                FieldValue::Eval(value) => value.clone(),
                FieldValue::Ref(xml_id) => {
                    let key = qualify(module, xml_id);
                    let (_, id) = xml_ids.get(&key).ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "unresolved external id {key} (field {name:?} on {})",
                            record.model
                        ))
                    })?;
                    Value::from(*id)
                }
            };
            values.push((name.clone(), value));
        }
        let pairs: Vec<(&str, Value)> = values
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();

        match &record.xml_id {
            None => {
                registry.create(pool, &record.model, pairs).await?;
                stats.created += 1;
            }
            Some(xml_id) => {
                let key = qualify(module, xml_id);
                match xml_ids.get(&key) {
                    Some(_) if record.noupdate => stats.skipped += 1,
                    Some((_, existing)) => {
                        let existing = *existing;
                        registry
                            .write(pool, &record.model, &[existing], pairs)
                            .await?;
                        stats.updated += 1;
                    }
                    None => {
                        let id = registry.create(pool, &record.model, pairs).await?;
                        xml_ids.insert(key, record.model.clone(), id);
                        stats.created += 1;
                    }
                }
            }
        }
    }
    Ok(stats)
}
