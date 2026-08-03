//! Mirrors `odoo.models.BaseModel` metadata and its field registry.

use crate::fields::Field;

#[derive(Debug, Clone)]
pub struct ModelMeta {
    /// Odoo model name, e.g. `res.partner`
    pub name: String,
    /// PostgreSQL table, e.g. `res_partner`
    pub table: String,
    /// `_inherit` chain
    pub inherit: Vec<String>,
    /// `_inherits` delegation: (parent model, many2one link field)
    pub inherits: Vec<(String, String)>,
}

/// A rule a record must satisfy, port of `@api.constrains`.
///
/// It watches fields and answers with what is wrong, not with a bool: a
/// record refused without a reason is a screen the user cannot fix.
#[derive(Clone)]
pub struct Constraint {
    /// what it is called, for the log and for a message with no text
    pub name: String,
    /// the fields whose change makes it worth checking again
    pub fields: Vec<String>,
    /// `Ok` when the record is fine, `Err(reason)` when it is not
    pub check: fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
}

impl std::fmt::Debug for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Constraint")
            .field("name", &self.name)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub meta: ModelMeta,
    fields: Vec<Field>,
    constraints: Vec<Constraint>,
    /// a `TransientModel`: its records are a dialog somebody had open,
    /// not data the business keeps
    transient: bool,
}

impl Model {
    pub fn new(meta: ModelMeta, fields: Vec<Field>) -> Self {
        Model {
            meta,
            fields,
            constraints: Vec::new(),
            transient: false,
        }
    }

    /// Mark the model transient (Odoo's `TransientModel`): the rows are
    /// the state of a dialog, and old ones are swept away instead of
    /// being kept forever.
    pub fn transient(mut self) -> Self {
        self.transient = true;
        self
    }

    pub fn is_transient(&self) -> bool {
        self.transient
    }

    /// Attach a rule every record of this model must satisfy.
    pub fn constrained(
        mut self,
        name: &str,
        fields: &[&str],
        check: fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
    ) -> Self {
        self.constraints.push(Constraint {
            name: name.to_string(),
            fields: fields.iter().map(|f| (*f).to_string()).collect(),
            check,
        });
        self
    }

    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub(crate) fn into_parts(self) -> (ModelMeta, Vec<Field>, Vec<Constraint>, bool) {
        (self.meta, self.fields, self.constraints, self.transient)
    }

    /// Rebuild with the constraints kept — the registry folds fields
    /// from the `_inherit` chain and must not drop the rules on the way.
    pub(crate) fn with_constraints(mut self, constraints: Vec<Constraint>) -> Self {
        self.constraints = constraints;
        self
    }
}
