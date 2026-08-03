//! Port of `odoo/orm/registry.py`: the per-database model registry,
//! including `_inherit` extension and prototype inheritance.

use crate::fields::{Field, FieldType};
use crate::model::{Model, ModelMeta};
use rusdoo_core::RusdooError;
use std::collections::{HashMap, HashSet};

/// Delegation chains are structurally acyclic (parents must pre-exist),
/// but every traversal caps its depth as defense in depth.
pub(crate) const MAX_DELEGATION_DEPTH: usize = 8;

/// Rebuilding a model must not quietly turn a dialog into stored data.
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

    pub fn models(&self) -> impl Iterator<Item = &Model> {
        self.models.values()
    }

    /// Register a model definition, applying Odoo's `_inherit` rules:
    /// no `_inherit` creates a new model; `_inherit` naming the model
    /// itself extends it in place; `_inherit` with a different `_name`
    /// starts the new model from a copy of the parents' fields
    /// (prototype inheritance).
    pub fn register(&mut self, model: Model) -> Result<(), RusdooError> {
        let meta = model.meta.clone();
        let mut own_fields = model.fields().to_vec();
        let own_constraints = model.constraints().to_vec();

        if meta.inherit.is_empty() {
            if self.models.contains_key(&meta.name) {
                return Err(RusdooError::Validation(format!(
                    "model already registered: {}",
                    meta.name
                )));
            }
            self.validate_inherits(&meta.inherits, &own_fields)?;
            ensure_log_access(&mut own_fields);
            self.models.insert(
                meta.name.clone(),
                model.rebuilt(meta, own_fields, own_constraints),
            );
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
        // a model that extends another keeps the rules it inherited: an
        // `_inherit` adds fields and behaviour, it does not licence the
        // parent's records to break the parent's constraints
        let mut constraints: Vec<crate::model::Constraint> = Vec::new();
        let mut hooks: Vec<crate::unlink::UnlinkHook> = Vec::new();
        let mut onchanges: Vec<crate::model::Onchange> = Vec::new();
        let mut fields: Vec<Field> = Vec::new();
        // a child that declares no `_order` of its own keeps the one its
        // parent chose: a module that adds a field to `account.move`
        // must not make its list stop being newest-first
        let mut inherited_order: Option<String> = None;
        for parent in meta.inherit.iter().rev() {
            let parent_model = self.models.get(parent).ok_or_else(|| {
                RusdooError::Validation(format!("_inherit parent not registered: {parent}"))
            })?;
            merge_fields(&mut fields, parent_model.fields().iter().cloned());
            constraints.extend(parent_model.constraints().iter().cloned());
            // an `_inherit` adds behaviour, it does not licence the
            // registros do pai a serem apagados sem as checagens dele
            hooks.extend(parent_model.unlink_hooks().iter().cloned());
            onchanges.extend(parent_model.onchanges().iter().cloned());
            if let Some(order) = parent_model.declared_order() {
                inherited_order = Some(order.to_string());
            }
        }
        merge_fields(&mut fields, own_fields);

        // an in-place extension must never move the model to another table
        let table = if extends_self {
            self.models[&meta.name].meta.table.clone()
        } else {
            meta.table.clone()
        };
        // an in-place extension keeps the original delegation setup
        let inherits = if extends_self {
            self.models[&meta.name].meta.inherits.clone()
        } else {
            meta.inherits.clone()
        };
        self.validate_inherits(&inherits, &fields)?;
        ensure_log_access(&mut fields);
        let merged_meta = ModelMeta {
            name: meta.name.clone(),
            table,
            inherit: meta.inherit,
            inherits,
        };
        constraints.extend(own_constraints);
        let mut merged = model;
        if merged.declared_order().is_none() {
            if let Some(order) = inherited_order {
                merged = merged.ordered(&order);
            }
        }
        let merged = merged.with_unlink_hooks(hooks).with_onchanges(onchanges);
        self.models
            .insert(meta.name, merged.rebuilt(merged_meta, fields, constraints));
        Ok(())
    }
}

/// Odoo's LOG_ACCESS fields, added to every model (`_log_access`): the
/// audit columns exposed as readonly ORM fields — create_uid/write_uid as
/// many2one to res.users, create_date/write_date as datetimes. The columns
/// themselves are created by the DDL layer and stamped by the write path;
/// here they simply become readable/introspectable and write-protected.
fn log_access_fields() -> [Field; 4] {
    let user = || FieldType::Many2one {
        comodel: "res.users".into(),
    };
    [
        Field::new("create_uid", user()).readonly(),
        Field::new("create_date", FieldType::Datetime).readonly(),
        Field::new("write_uid", user()).readonly(),
        Field::new("write_date", FieldType::Datetime).readonly(),
    ]
}

/// Install the LOG_ACCESS fields as system-owned: they are framework
/// managed (created by the DDL layer, stamped by the write path), so any
/// same-named field a model declares is REPLACED by the canonical
/// definition. This keeps field metadata in lockstep with the fixed audit
/// columns — a model cannot silently redefine their type or drop the audit
/// trail by colliding on the name.
fn ensure_log_access(fields: &mut Vec<Field>) {
    for field in log_access_fields() {
        match fields.iter_mut().find(|f| f.name == field.name) {
            Some(slot) => *slot = field,
            None => fields.push(field),
        }
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

impl Registry {
    /// `_inherits` is only valid when the parent is already registered and
    /// the link field is a declared many2one to it.
    fn validate_inherits(
        &self,
        inherits: &[(String, String)],
        fields: &[Field],
    ) -> Result<(), RusdooError> {
        let mut seen_parents = HashSet::new();
        let mut seen_links = HashSet::new();
        for (parent, link) in inherits {
            if !seen_parents.insert(parent) {
                return Err(RusdooError::Validation(format!(
                    "duplicate _inherits parent: {parent}"
                )));
            }
            if !seen_links.insert(link) {
                return Err(RusdooError::Validation(format!(
                    "duplicate _inherits link field: {link}"
                )));
            }
            if !self.models.contains_key(parent) {
                return Err(RusdooError::Validation(format!(
                    "_inherits parent not registered: {parent}"
                )));
            }
            let link_ok = fields.iter().any(|f| {
                f.name == *link
                    && matches!(&f.ty, FieldType::Many2one { comodel } if comodel == parent)
            });
            if !link_ok {
                return Err(RusdooError::Validation(format!(
                    "_inherits link field {link:?} must be a declared many2one to {parent}"
                )));
            }
        }
        Ok(())
    }

    /// Does `model` own `name`, directly or through delegation?
    pub(crate) fn owns_field(&self, model: &Model, name: &str, depth: usize) -> bool {
        if depth > MAX_DELEGATION_DEPTH {
            return false;
        }
        if model.field(name).is_some() {
            return true;
        }
        model.meta.inherits.iter().any(|(p, _)| {
            self.get(p)
                .is_some_and(|pm| self.owns_field(pm, name, depth + 1))
        })
    }

    /// Hops of (link field, parent model) leading to the owner of `name`.
    /// Registration order makes delegation acyclic, but depth is capped
    /// anyway as defense in depth.
    pub(crate) fn delegation_chain(
        &self,
        model: &Model,
        name: &str,
        depth: usize,
    ) -> Option<Vec<(String, String)>> {
        if depth > MAX_DELEGATION_DEPTH {
            return None;
        }
        for (parent_name, link) in &model.meta.inherits {
            let Some(parent) = self.get(parent_name) else {
                continue;
            };
            if parent.field(name).is_some() {
                return Some(vec![(link.clone(), parent_name.clone())]);
            }
            if let Some(mut chain) = self.delegation_chain(parent, name, depth + 1) {
                chain.insert(0, (link.clone(), parent_name.clone()));
                return Some(chain);
            }
        }
        None
    }
}
