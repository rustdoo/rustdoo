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
        // the LOG_ACCESS columns are emitted above from MAGIC_COLUMNS; the
        // registry also exposes them as fields, so skip them here to avoid
        // declaring the same column twice
        if MAGIC_COLUMNS.iter().any(|(name, _)| *name == field.name) {
            continue;
        }
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

/// The columns a model wants that its table may not have yet, as one
/// `ALTER TABLE` each — the port of what Odoo does on `-u`.
///
/// A field added later (by an upgrade, or by another module extending
/// the model) has no column on a database that already exists, and a
/// read of it fails with a message about SQL rather than about the
/// field. `CREATE TABLE IF NOT EXISTS` alone can never fix that: the
/// table is already there.
///
/// The column is added nullable even when the field is required, and
/// the constraint is attempted separately: a table with rows in it
/// cannot take a NOT NULL column, and refusing to add the column at all
/// would be worse than adding it without the constraint.
pub fn add_missing_columns_sql(model: &Model) -> Result<Vec<(String, bool)>, RusdooError> {
    let table = quote_ident(&model.meta.table)?;
    let mut statements = Vec::new();
    for field in model.fields() {
        if MAGIC_COLUMNS.iter().any(|(name, _)| *name == field.name) {
            continue;
        }
        let Some(column_type) = field.column_type() else {
            continue;
        };
        statements.push((
            format!(
                "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {} {column_type}",
                quote_ident(&field.name)?
            ),
            field.required,
        ));
    }
    Ok(statements)
}

/// Ask PostgreSQL to enforce a required field, for the callers that add
/// a column to a table that might be empty.
pub fn set_not_null_sql(model: &Model, field: &str) -> Result<String, RusdooError> {
    Ok(format!(
        "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL",
        quote_ident(&model.meta.table)?,
        quote_ident(field)?
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
