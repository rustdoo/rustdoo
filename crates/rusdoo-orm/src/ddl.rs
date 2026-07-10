//! Table DDL generation, port of the schema init in `odoo/orm/models.py`.

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
    Ok(format!("CREATE TABLE {table} ({})", columns.join(", ")))
}
