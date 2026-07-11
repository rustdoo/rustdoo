//! SQL builders for the CRUD entry points of `BaseModel`:
//! `search`, `read`, `create`, `write`, `unlink`.

use crate::domain::Domain;
use crate::fields::Field;
use crate::model::Model;
use crate::registry::{Registry, MAX_DELEGATION_DEPTH};
use crate::sql::{bind, quote_ident, render, Ctx};
use rusdoo_core::RusdooError;
use serde_json::Value;
use std::collections::HashMap;

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
    ) -> Result<(String, Vec<Value>), RusdooError> {
        self.search_sql_with(domain, opts, Ctx::model(self))
    }

    pub(crate) fn search_sql_with(
        &self,
        domain: &Domain,
        opts: &SearchOptions,
        ctx: Ctx,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let mut params = Vec::new();
        let where_sql = render(domain, &mut params, ctx)?;
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
    ) -> Result<(String, Vec<Value>), RusdooError> {
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
        uid: i64,
        values: Vec<(&str, Value)>,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        if values.is_empty() {
            // delegation may create a parent row with no explicit values;
            // it still gets the full LOG_ACCESS stamp
            let mut params = Vec::new();
            let uid_ph = bind(&mut params, Value::from(uid));
            let sql = format!(
                r#"INSERT INTO {} ("create_uid", "write_uid", "create_date", "write_date")                    VALUES ({uid_ph}, {uid_ph}, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) RETURNING "id""#,
                quote_ident(&self.meta.table)?
            );
            return Ok((sql, params));
        }
        let mut columns = Vec::new();
        let mut params = Vec::new();
        let mut placeholders = Vec::new();
        for (name, value) in values {
            self.stored_field(name)?;
            columns.push(quote_ident(name)?);
            placeholders.push(bind(&mut params, value));
        }
        // audit columns Odoo stamps on every create (LOG_ACCESS)
        let uid_ph = bind(&mut params, Value::from(uid));
        columns.push(r#""create_uid""#.to_string());
        columns.push(r#""write_uid""#.to_string());
        columns.push(r#""create_date""#.to_string());
        columns.push(r#""write_date""#.to_string());
        placeholders.push(uid_ph.clone());
        placeholders.push(uid_ph);
        placeholders.push("CURRENT_TIMESTAMP".to_string());
        placeholders.push("CURRENT_TIMESTAMP".to_string());
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
        uid: i64,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        if values.is_empty() {
            return Err(RusdooError::Validation(
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
        // Odoo refreshes write_uid/write_date on every write
        let uid_ph = bind(&mut params, Value::from(uid));
        assignments.push(format!(r#""write_uid" = {uid_ph}"#));
        assignments.push(r#""write_date" = CURRENT_TIMESTAMP"#.to_string());
        let id_list = bind_ids(ids, &mut params)?;
        let sql = format!(
            r#"UPDATE {} SET {} WHERE "id" IN ({id_list})"#,
            quote_ident(&self.meta.table)?,
            assignments.join(", ")
        );
        Ok((sql, params))
    }

    pub fn delete_sql(&self, ids: &[i64]) -> Result<(String, Vec<Value>), RusdooError> {
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
    fn order_by(&self, spec: &str) -> Result<String, RusdooError> {
        let clauses: Vec<String> = spec
            .split(',')
            .map(|part| {
                let mut words = part.split_whitespace();
                let field = words
                    .next()
                    .ok_or_else(|| RusdooError::Validation("empty ORDER BY clause".into()))?;
                if field != "id" && self.field(field).is_none() {
                    return Err(RusdooError::Validation(format!(
                        "unknown field in order: {field:?}"
                    )));
                }
                let direction = match words.next().map(str::to_ascii_lowercase).as_deref() {
                    None | Some("asc") => "ASC",
                    Some("desc") => "DESC",
                    Some(other) => {
                        return Err(RusdooError::Validation(format!(
                            "invalid order direction: {other:?}"
                        )))
                    }
                };
                if words.next().is_some() {
                    return Err(RusdooError::Validation(format!(
                        "malformed order clause: {part:?}"
                    )));
                }
                Ok(format!("{} {direction}", quote_ident(field)?))
            })
            .collect::<Result<_, _>>()?;
        Ok(clauses.join(", "))
    }

    fn stored_field(&self, name: &str) -> Result<(), RusdooError> {
        match self.field(name) {
            Some(field) if field.stored => Ok(()),
            Some(_) => Err(RusdooError::Validation(format!(
                "field is not stored: {name:?}"
            ))),
            None => Err(RusdooError::Validation(format!(
                "unknown field on {}: {name:?}",
                self.meta.name
            ))),
        }
    }
}

pub(crate) fn bind_ids(ids: &[i64], params: &mut Vec<Value>) -> Result<String, RusdooError> {
    if ids.is_empty() {
        return Err(RusdooError::Validation("no record ids given".into()));
    }
    let placeholders: Vec<String> = ids
        .iter()
        .map(|id| bind(params, Value::from(*id)))
        .collect();
    Ok(placeholders.join(", "))
}

impl Registry {
    /// Search on a registered model with full relational context
    /// (dotted paths and any/not any resolve against this registry).
    pub fn search_sql(
        &self,
        model_name: &str,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        model.search_sql_with(domain, opts, Ctx::full(model, self))
    }
}

/// A read column resolved through delegation: its request name and the
/// field definition that owns it (used to decode the row).
pub(crate) struct ResolvedColumn {
    pub(crate) name: String,
    pub(crate) field: Field,
}

impl Registry {
    /// Read with `_inherits` delegation: parent fields are fetched through
    /// LEFT JOINs on the link columns.
    pub fn read_sql(
        &self,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let (sql, params, _) = self.read_query(model_name, ids, fields)?;
        Ok((sql, params))
    }

    pub(crate) fn read_query(
        &self,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<(String, Vec<Value>, Vec<ResolvedColumn>), RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let mut joins: Vec<String> = Vec::new();
        let mut alias_map: HashMap<(String, String, String), String> = HashMap::new();
        let mut alias_count = 0usize;
        let mut columns = vec![r#""t0"."id""#.to_string()];
        let mut resolved = Vec::new();
        for name in fields {
            let Some((col, field)) = self.resolve_column(
                model,
                "t0",
                name,
                &mut joins,
                &mut alias_map,
                &mut alias_count,
                0,
            )?
            else {
                return Err(RusdooError::Validation(format!(
                    "unknown field on {model_name}: {name:?}"
                )));
            };
            columns.push(col);
            resolved.push(ResolvedColumn {
                name: (*name).to_string(),
                field,
            });
        }
        let mut params = Vec::new();
        let id_list = bind_ids(ids, &mut params)?;
        let sql = format!(
            r#"SELECT {} FROM {} "t0"{} WHERE "t0"."id" IN ({id_list})"#,
            columns.join(", "),
            quote_ident(&model.meta.table)?,
            joins.concat()
        );
        Ok((sql, params, resolved))
    }

    /// Qualified column for `name` on `model`, adding LEFT JOINs for
    /// delegation hops (joins are reused across sibling fields).
    #[allow(clippy::too_many_arguments)]
    fn resolve_column(
        &self,
        model: &Model,
        alias: &str,
        name: &str,
        joins: &mut Vec<String>,
        alias_map: &mut HashMap<(String, String, String), String>,
        alias_count: &mut usize,
        depth: usize,
    ) -> Result<Option<(String, Field)>, RusdooError> {
        if depth > MAX_DELEGATION_DEPTH {
            return Err(RusdooError::Validation(
                "delegation chain exceeds maximum depth".into(),
            ));
        }
        if let Some(field) = model.field(name) {
            if !field.stored {
                return Err(RusdooError::Validation(format!(
                    "field is not stored: {name:?}"
                )));
            }
            return Ok(Some((
                format!(r#""{alias}".{}"#, quote_ident(name)?),
                field.clone(),
            )));
        }
        for (parent_name, link) in &model.meta.inherits {
            let parent = self.get(parent_name).ok_or_else(|| {
                RusdooError::Validation(format!("_inherits parent not registered: {parent_name}"))
            })?;
            if !self.owns_field(parent, name, 0) {
                continue;
            }
            let key = (alias.to_string(), link.clone(), parent_name.clone());
            let parent_alias = match alias_map.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    *alias_count += 1;
                    let new_alias = format!("t{alias_count}");
                    joins.push(format!(
                        r#" LEFT JOIN {} "{new_alias}" ON "{alias}".{} = "{new_alias}"."id""#,
                        quote_ident(&parent.meta.table)?,
                        quote_ident(link)?
                    ));
                    alias_map.insert(key, new_alias.clone());
                    new_alias
                }
            };
            return self.resolve_column(
                parent,
                &parent_alias,
                name,
                joins,
                alias_map,
                alias_count,
                depth + 1,
            );
        }
        Ok(None)
    }
}
