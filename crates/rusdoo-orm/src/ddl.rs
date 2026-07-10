//! Table DDL generation, port of the schema init in `odoo/orm/models.py`.

use crate::fields::{Field, FieldType};
use crate::model::Model;
use crate::sql::quote_ident;
use rusdoo_core::RusdooError;

/// Audit columns Odoo adds to every regular model (LOG_ACCESS_COLUMNS).
const MAGIC_COLUMNS: [(&str, &str); 4] = [
    ("create_uid", "int4"),
    ("create_date", "timestamp"),
    ("write_uid", "int4"),
    ("write_date", "timestamp"),
];

pub fn create_table_sql(model: &Model) -> Result<String, RusdooError> {
    let table = quote_ident(&model.meta.table)?;
    let mut columns = vec![r#""id" SERIAL NOT NULL"#.to_string()];
    for (name, ty) in MAGIC_COLUMNS {
        columns.push(format!("{} {ty}", quote_ident(name)?));
    }
    for field in model.fields() {
        let Some(column_type) = field.column_type() else {
            continue;
        };
        let mut column = format!("{} {column_type}", quote_ident(&field.name)?);
        if field.required {
            column.push_str(" NOT NULL");
        }
        columns.push(column);
    }
    columns.push(r#"PRIMARY KEY("id")"#.into());
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} ({})",
        columns.join(", ")
    ))
}

/// Relation table backing a many2many field (`fields_relational.py`).
/// `IF NOT EXISTS` because both sides of the relation may try to create it.
pub fn create_relation_table_sql(field: &Field) -> Result<Option<String>, RusdooError> {
    let FieldType::Many2many {
        relation,
        column1,
        column2,
        ..
    } = &field.ty
    else {
        return Ok(None);
    };
    let (rel, c1, c2) = (
        quote_ident(relation)?,
        quote_ident(column1)?,
        quote_ident(column2)?,
    );
    Ok(Some(format!(
        "CREATE TABLE IF NOT EXISTS {rel} ({c1} int4 NOT NULL, {c2} int4 NOT NULL, PRIMARY KEY({c1}, {c2}))"
    )))
}
