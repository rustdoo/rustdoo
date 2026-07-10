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
}

#[derive(Debug, Clone)]
pub struct Model {
    pub meta: ModelMeta,
    fields: Vec<Field>,
}

impl Model {
    pub fn new(meta: ModelMeta, fields: Vec<Field>) -> Self {
        Model { meta, fields }
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub(crate) fn into_parts(self) -> (ModelMeta, Vec<Field>) {
        (self.meta, self.fields)
    }
}
