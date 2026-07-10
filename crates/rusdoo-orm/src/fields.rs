//! Port of `odoo/orm/fields*.py`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Boolean,
    Integer,
    Float {
        digits: Option<(u8, u8)>,
    },
    Char {
        size: Option<u32>,
    },
    Text,
    Html,
    Date,
    Datetime,
    Binary,
    Selection(Vec<(String, String)>),
    Many2one {
        comodel: String,
    },
    One2many {
        comodel: String,
        inverse: String,
    },
    Many2many {
        comodel: String,
        /// relation table joining the two models
        relation: String,
        /// column referencing this model in the relation table
        column1: String,
        /// column referencing the comodel
        column2: String,
    },
    Json,
    Monetary,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
    pub readonly: bool,
    pub stored: bool,
}

impl Field {
    pub fn new(name: &str, ty: FieldType) -> Self {
        let stored = !matches!(ty, FieldType::One2many { .. } | FieldType::Many2many { .. });
        Field {
            name: name.to_string(),
            ty,
            required: false,
            readonly: false,
            stored,
        }
    }

    pub fn required(self) -> Self {
        Field {
            required: true,
            ..self
        }
    }

    pub fn readonly(self) -> Self {
        Field {
            readonly: true,
            ..self
        }
    }

    /// PostgreSQL column type; `None` for field types without their own
    /// column (one2many lives on the inverse, many2many in a relation table).
    pub fn column_type(&self) -> Option<String> {
        use FieldType::*;
        Some(match &self.ty {
            Boolean => "bool".into(),
            Integer => "int4".into(),
            Float {
                digits: Some((precision, scale)),
            } => format!("numeric({precision},{scale})"),
            Float { digits: None } => "float8".into(),
            Monetary => "numeric".into(),
            Char { size: Some(n) } => format!("varchar({n})"),
            Char { size: None } | Selection(_) => "varchar".into(),
            Text | Html => "text".into(),
            Date => "date".into(),
            Datetime => "timestamp".into(),
            Binary => "bytea".into(),
            Many2one { .. } => "int4".into(),
            Json => "jsonb".into(),
            One2many { .. } | Many2many { .. } => return None,
        })
    }

    /// The value stored when the field is "not set" but the column holds a
    /// concrete default — mirrors `falsy_value` on Odoo field classes
    /// (0 for numeric, '' for text, false for boolean). `None` means the
    /// only unset representation is SQL NULL.
    pub fn falsy_value(&self) -> Option<Value> {
        use FieldType::*;
        match &self.ty {
            Boolean => Some(Value::Bool(false)),
            Integer | Float { .. } | Monetary => Some(Value::from(0)),
            Char { .. } | Text | Html => Some(Value::String(String::new())),
            _ => None,
        }
    }
}
