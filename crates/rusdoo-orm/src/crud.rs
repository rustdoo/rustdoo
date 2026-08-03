//! SQL builders for the CRUD entry points of `BaseModel`:
//! `search`, `read`, `create`, `write`, `unlink`.

use crate::domain::{Domain, Operator, Term};
use crate::fields::{Field, FieldType};
use crate::model::Model;
use crate::registry::{Registry, MAX_DELEGATION_DEPTH};
use crate::sql::{bind, quote_ident, render, Ctx};
use rusdoo_core::RusdooError;
use serde_json::Value;
use std::collections::HashMap;

/// Odoo's `_active_name`: the field that archives a record.
pub const ACTIVE_FIELD: &str = "active";

#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// e.g. `"name asc, id desc"`, mirroring Odoo's `order` argument
    pub order: Option<String>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// Odoo's `active_test` context flag: archived records stay out of
    /// every search unless the caller asks for them
    pub active_test: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            order: None,
            limit: None,
            offset: None,
            // Odoo's default: `active_test` is only ever turned off
            active_test: true,
        }
    }
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
        let (mut sql, params) = self.select_from_where(r#""id""#, domain, opts, ctx)?;
        // a search with no order of its own gets the model's `_order`,
        // never PostgreSQL's whim
        let order = opts.order.as_deref().unwrap_or_else(|| self.order());
        sql.push_str(&format!(" ORDER BY {}", self.order_by(order)?));
        if let Some(limit) = opts.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = opts.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        Ok((sql, params))
    }

    /// `SELECT <columns> FROM <table> WHERE <domain>`, with no ordering
    /// and no paging — the part a search and a count agree on.
    fn select_from_where(
        &self,
        columns: &str,
        domain: &Domain,
        opts: &SearchOptions,
        ctx: Ctx,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let mut params = Vec::new();
        let active_test = self.active_test_domain(domain, opts);
        let where_sql = render(active_test.as_ref().unwrap_or(domain), &mut params, ctx)?;
        Ok((
            format!(
                "SELECT {columns} FROM {} WHERE {where_sql}",
                quote_ident(&self.meta.table)?
            ),
            params,
        ))
    }

    /// How many records the domain matches, as SQL.
    pub fn count_sql(
        &self,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        self.count_sql_with(domain, opts, Ctx::model(self))
    }

    /// `SELECT COUNT(*)` over the same WHERE a search would run.
    /// Counting in the database is the point: materializing every id only
    /// to call `.len()` on them pulls the whole table through the wire
    /// for a number.
    ///
    /// A `limit` still caps the count (Odoo's `search_count(limit=n)`
    /// answers "at least n"), so the subquery keeps the paging clauses.
    pub(crate) fn count_sql_with(
        &self,
        domain: &Domain,
        opts: &SearchOptions,
        ctx: Ctx,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        // no paging: count straight from the table. The count is built
        // from the same pieces as the search rather than by editing its
        // SQL as a string — a `COUNT(*)` that inherited the search's
        // `ORDER BY name` is rejected by PostgreSQL outright, and that is
        // what a text substitution had been quietly getting away with
        // while every model's order was `None`.
        if opts.limit.is_none() && opts.offset.is_none() {
            return self.select_from_where("COUNT(*)", domain, opts, ctx);
        }
        // with paging the subquery must keep its order: `LIMIT` over an
        // unordered select is a different set of rows every time
        let (inner, params) = self.search_sql_with(domain, opts, ctx)?;
        Ok((
            format!("SELECT COUNT(*) FROM ({inner}) AS \"counted\""),
            params,
        ))
    }

    pub fn read_sql(
        &self,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let mut columns = vec![r#""id""#.to_string()];
        for name in fields {
            self.stored_field(name)?;
            let quoted = quote_ident(name)?;
            let cast = self
                .field(name)
                .map(|f| crate::sql::read_cast_for(&f.ty))
                .unwrap_or("");
            columns.push(if cast.is_empty() {
                quoted
            } else {
                format!("({quoted}){cast} AS {quoted}")
            });
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
        let values = self.with_defaults(values);
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
            self.writable_field(name)?;
            columns.push(quote_ident(name)?);
            let placeholder = bind_or_null(&mut params, self.typed_value(name, value));
            placeholders.push(format!("{placeholder}{}", self.column_cast(name)));
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
            self.writable_field(name)?;
            let placeholder = bind_or_null(&mut params, self.typed_value(name, value));
            assignments.push(format!(
                "{} = {placeholder}{}",
                quote_ident(name)?,
                self.column_cast(name)
            ));
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

    /// The domain a search actually runs, once archived records are
    /// excluded: `active = True` is AND-ed in when the model has the
    /// field, the caller did not turn `active_test` off, and the domain
    /// does not already speak about it (`odoo/orm/models.py::_search`).
    /// `None` means the domain is used as it came.
    fn active_test_domain(&self, domain: &Domain, opts: &SearchOptions) -> Option<Domain> {
        if !opts.active_test || self.field(ACTIVE_FIELD).is_none() || domain.mentions(ACTIVE_FIELD)
        {
            return None;
        }
        Some(Domain::And(vec![
            domain.clone(),
            Domain::Term(Term {
                field: ACTIVE_FIELD.to_string(),
                op: Operator::Eq,
                value: Value::Bool(true),
            }),
        ]))
    }

    /// `"name asc, id desc"` -> `"name" ASC, "id" DESC`, validated against
    /// the model's fields so arbitrary SQL can never reach ORDER BY.
    /// The model's own `_order`, rendered as SQL — what a relation reads
    /// its lines by.
    pub(crate) fn order_sql(&self) -> Result<String, RusdooError> {
        self.order_by(self.order())
    }

    fn order_by(&self, spec: &str) -> Result<String, RusdooError> {
        let clauses: Vec<String> = spec
            .split(',')
            .map(|part| {
                let mut words = part.split_whitespace();
                let field = words
                    .next()
                    .ok_or_else(|| RusdooError::Validation("empty ORDER BY clause".into()))?;
                if field != "id" {
                    match self.field(field) {
                        // ordering happens in SQL: a field with no column
                        // of its own cannot appear in ORDER BY
                        Some(f) if !f.stored => {
                            return Err(RusdooError::Validation(format!(
                                "field is not stored, cannot order by it: {field:?}"
                            )))
                        }
                        Some(_) => {}
                        None => {
                            return Err(RusdooError::Validation(format!(
                                "unknown field in order: {field:?}"
                            )))
                        }
                    }
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

    /// Fill in the declared defaults of every field the create left out
    /// (`odoo/orm/models.py::create`). Values the caller passed always
    /// win, including an explicit null — "unset on purpose" is a
    /// decision, not a gap to fill.
    fn with_defaults<'a>(&'a self, mut values: Vec<(&'a str, Value)>) -> Vec<(&'a str, Value)> {
        for field in self.fields() {
            let Some(default) = &field.default else {
                continue;
            };
            // readonly fields are framework-owned (the LOG_ACCESS stamp);
            // unstored ones have no column to write
            if field.readonly || !field.stored {
                continue;
            }
            if values.iter().any(|(name, _)| *name == field.name) {
                continue;
            }
            values.push((field.name.as_str(), default.clone()));
        }
        values
    }

    /// The cast a bound parameter needs to land in this column. Dates and
    /// datetimes travel as strings (the JSON-RPC wire format), so their
    /// parameter is text: PostgreSQL has no implicit text -> date, and
    /// the insert would fail on the column type.
    /// The value as its column's type, not as JSON happened to spell it.
    ///
    /// Two rows of the same INSERT share one prepared statement, whose
    /// parameter types are fixed by the first row: `1250` bound as an
    /// integer and `890.5` bound as a float in the next row would be read
    /// through the wrong decoder — the bytes of a float read as an
    /// integer are a number in the billions, which a `numeric(16,2)`
    /// column rejects as an overflow. Typing the value by its column is
    /// what keeps every row of a batch binding the same way.
    fn typed_value(&self, name: &str, value: Value) -> Value {
        let Some(field) = self.field(name) else {
            return value;
        };
        let Value::Number(number) = &value else {
            return value;
        };
        match field.ty {
            FieldType::Float { .. } | FieldType::Monetary => number
                .as_f64()
                .map_or(value.clone(), Value::from),
            FieldType::Integer | FieldType::Many2one { .. } => number
                .as_i64()
                .map_or(value.clone(), Value::from),
            _ => value,
        }
    }

    pub(crate) fn column_cast(&self, name: &str) -> &'static str {
        crate::sql::value_cast_for(self.field(name).map(|f| &f.ty))
    }

    /// A stored, non-readonly field: the write path rejects readonly
    /// fields (e.g. the LOG_ACCESS audit columns) so a client can never
    /// forge them — they are set only by the ORM's own stamping.
    fn writable_field(&self, name: &str) -> Result<(), RusdooError> {
        // a related field mirrors another record: writing it means
        // writing there, which the ORM does not do yet — say so instead
        // of the generic "not stored"
        if let Some(path) = self.field(name).and_then(|f| f.related.as_ref()) {
            return Err(RusdooError::Validation(format!(
                "field {name:?} is related to {path:?}: write the target instead"
            )));
        }
        self.stored_field(name)?;
        if self.field(name).is_some_and(|f| f.readonly) {
            return Err(RusdooError::Validation(format!(
                "field is readonly: {name:?}"
            )));
        }
        Ok(())
    }
}

/// A JSON null is "no value": it renders as an untyped SQL NULL literal
/// instead of a bound parameter. A null parameter carries the type sqlx
/// binds it as (text), which PostgreSQL then refuses to store in a
/// boolean or integer column.
fn bind_or_null(params: &mut Vec<Value>, value: Value) -> String {
    if value.is_null() {
        return "NULL".to_string();
    }
    bind(params, value)
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
            // a fixed-precision column is read as float8, under its own
            // name so the decoder still finds it
            let cast = crate::sql::read_cast_for(&field.ty);
            columns.push(if cast.is_empty() {
                col
            } else {
                format!("({col}){cast} AS {}", quote_ident(name)?)
            });
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
