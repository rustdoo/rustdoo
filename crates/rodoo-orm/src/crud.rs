//! SQL builders for the CRUD entry points of `BaseModel`:
//! `search`, `read`, `create`, `write`, `unlink`.

use crate::domain::Domain;
use crate::model::Model;
use crate::sql::{bind, quote_ident, render};
use rodoo_core::RodooError;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// e.g. `"name asc, id desc"`, mirroring Odoo's `order` argument
    pub order: Option<String>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl Model {
    pub fn search_sql(
        &self,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<(String, Vec<Value>), RodooError> {
        let mut params = Vec::new();
        let where_sql = render(domain, &mut params, Some(self))?;
        let mut sql = format!(
            r#"SELECT "id" FROM {} WHERE {where_sql}"#,
            quote_ident(&self.meta.table)?
        );
        if let Some(order) = &opts.order {
            sql.push_str(&format!(" ORDER BY {}", self.order_by(order)?));
        }
        if let Some(limit) = opts.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = opts.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        Ok((sql, params))
    }

    pub fn read_sql(
        &self,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<(String, Vec<Value>), RodooError> {
        let mut columns = vec![r#""id""#.to_string()];
        for name in fields {
            self.stored_field(name)?;
            columns.push(quote_ident(name)?);
        }
        let mut params = Vec::new();
        let id_list = bind_ids(ids, &mut params)?;
        let sql = format!(
            r#"SELECT {} FROM {} WHERE "id" IN ({id_list})"#,
            columns.join(", "),
            quote_ident(&self.meta.table)?
        );
        Ok((sql, params))
    }

    pub fn insert_sql(
        &self,
        values: Vec<(&str, Value)>,
    ) -> Result<(String, Vec<Value>), RodooError> {
        if values.is_empty() {
            return Err(RodooError::Validation(
                "insert requires at least one value".into(),
            ));
        }
        let mut columns = Vec::new();
        let mut params = Vec::new();
        let mut placeholders = Vec::new();
        for (name, value) in values {
            self.stored_field(name)?;
            columns.push(quote_ident(name)?);
            placeholders.push(bind(&mut params, value));
        }
        let sql = format!(
            r#"INSERT INTO {} ({}) VALUES ({}) RETURNING "id""#,
            quote_ident(&self.meta.table)?,
            columns.join(", "),
            placeholders.join(", ")
        );
        Ok((sql, params))
    }

    pub fn update_sql(
        &self,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(String, Vec<Value>), RodooError> {
        if values.is_empty() {
            return Err(RodooError::Validation(
                "update requires at least one value".into(),
            ));
        }
        let mut params = Vec::new();
        let mut assignments = Vec::new();
        for (name, value) in values {
            self.stored_field(name)?;
            let placeholder = bind(&mut params, value);
            assignments.push(format!("{} = {placeholder}", quote_ident(name)?));
        }
        let id_list = bind_ids(ids, &mut params)?;
        let sql = format!(
            r#"UPDATE {} SET {} WHERE "id" IN ({id_list})"#,
            quote_ident(&self.meta.table)?,
            assignments.join(", ")
        );
        Ok((sql, params))
    }

    pub fn delete_sql(&self, ids: &[i64]) -> Result<(String, Vec<Value>), RodooError> {
        let mut params = Vec::new();
        let id_list = bind_ids(ids, &mut params)?;
        let sql = format!(
            r#"DELETE FROM {} WHERE "id" IN ({id_list})"#,
            quote_ident(&self.meta.table)?
        );
        Ok((sql, params))
    }

    /// `"name asc, id desc"` -> `"name" ASC, "id" DESC`, validated against
    /// the model's fields so arbitrary SQL can never reach ORDER BY.
    fn order_by(&self, spec: &str) -> Result<String, RodooError> {
        let clauses: Vec<String> = spec
            .split(',')
            .map(|part| {
                let mut words = part.split_whitespace();
                let field = words
                    .next()
                    .ok_or_else(|| RodooError::Validation("empty ORDER BY clause".into()))?;
                if field != "id" && self.field(field).is_none() {
                    return Err(RodooError::Validation(format!(
                        "unknown field in order: {field:?}"
                    )));
                }
                let direction = match words.next().map(str::to_ascii_lowercase).as_deref() {
                    None | Some("asc") => "ASC",
                    Some("desc") => "DESC",
                    Some(other) => {
                        return Err(RodooError::Validation(format!(
                            "invalid order direction: {other:?}"
                        )))
                    }
                };
                if words.next().is_some() {
                    return Err(RodooError::Validation(format!(
                        "malformed order clause: {part:?}"
                    )));
                }
                Ok(format!("{} {direction}", quote_ident(field)?))
            })
            .collect::<Result<_, _>>()?;
        Ok(clauses.join(", "))
    }

    fn stored_field(&self, name: &str) -> Result<(), RodooError> {
        match self.field(name) {
            Some(field) if field.stored => Ok(()),
            Some(_) => Err(RodooError::Validation(format!(
                "field is not stored: {name:?}"
            ))),
            None => Err(RodooError::Validation(format!(
                "unknown field on {}: {name:?}",
                self.meta.name
            ))),
        }
    }
}

fn bind_ids(ids: &[i64], params: &mut Vec<Value>) -> Result<String, RodooError> {
    if ids.is_empty() {
        return Err(RodooError::Validation("no record ids given".into()));
    }
    let placeholders: Vec<String> = ids
        .iter()
        .map(|id| bind(params, Value::from(*id)))
        .collect();
    Ok(placeholders.join(", "))
}
