//! Port of `odoo/orm/registry.py`: the per-database model registry,
//! including `_inherit` extension and prototype inheritance.

use crate::fields::Field;
use crate::model::{Model, ModelMeta};
use rusdoo_core::RusdooError;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Registry {
    models: HashMap<String, Model>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Model> {
        self.models.get(name)
    }

    /// Register a model definition, applying Odoo's `_inherit` rules:
    /// no `_inherit` creates a new model; `_inherit` naming the model
    /// itself extends it in place; `_inherit` with a different `_name`
    /// starts the new model from a copy of the parents' fields
    /// (prototype inheritance).
    pub fn register(&mut self, model: Model) -> Result<(), RusdooError> {
        let (meta, own_fields) = model.into_parts();

        if meta.inherit.is_empty() {
            if self.models.contains_key(&meta.name) {
                return Err(RusdooError::Validation(format!(
                    "model already registered: {}",
                    meta.name
                )));
            }
            self.models
                .insert(meta.name.clone(), Model::new(meta, own_fields));
            return Ok(());
        }

        let extends_self = meta.inherit.contains(&meta.name);
        if !extends_self && self.models.contains_key(&meta.name) {
            return Err(RusdooError::Validation(format!(
                "model already registered: {}",
                meta.name
            )));
        }

        // parents folded in reverse so the FIRST-listed parent wins field
        // conflicts (Odoo MRO); own fields, applied last, win over all
        let mut fields: Vec<Field> = Vec::new();
        for parent in meta.inherit.iter().rev() {
            let parent_model = self.models.get(parent).ok_or_else(|| {
                RusdooError::Validation(format!("_inherit parent not registered: {parent}"))
            })?;
            merge_fields(&mut fields, parent_model.fields().iter().cloned());
        }
        merge_fields(&mut fields, own_fields);

        // an in-place extension must never move the model to another table
        let table = if extends_self {
            self.models[&meta.name].meta.table.clone()
        } else {
            meta.table.clone()
        };
        let merged_meta = ModelMeta {
            name: meta.name.clone(),
            table,
            inherit: meta.inherit,
        };
        self.models
            .insert(meta.name, Model::new(merged_meta, fields));
        Ok(())
    }
}

/// Merge `extra` into `acc`, replacing same-name fields. Callers control
/// precedence by merge order: whatever is merged last wins.
fn merge_fields(acc: &mut Vec<Field>, extra: impl IntoIterator<Item = Field>) {
    for field in extra {
        if let Some(slot) = acc.iter_mut().find(|f| f.name == field.name) {
            *slot = field;
        } else {
            acc.push(field);
        }
    }
}
