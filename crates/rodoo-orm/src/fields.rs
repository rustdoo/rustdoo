//! Port of `odoo/orm/fields*.py`.

/// Field types supported by the Odoo ORM.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Boolean,
    Integer,
    Float { digits: Option<(u8, u8)> },
    Char { size: Option<u32> },
    Text,
    Html,
    Date,
    Datetime,
    Binary,
    Selection(Vec<(String, String)>),
    Many2one { comodel: String },
    One2many { comodel: String, inverse: String },
    Many2many { comodel: String },
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
