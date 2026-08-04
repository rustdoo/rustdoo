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
    /// the caller's context, for the parts of a search that depend on
    /// *in what terms* it was asked — the language a `name_search`
    /// matches in, a module's own flag
    pub context: crate::context::Context,
}

impl SearchOptions {
    /// The options a context implies: `active_test` read out of it, and
    /// the context itself carried along for whatever else looks at it.
    pub fn from_context(context: crate::context::Context) -> SearchOptions {
        SearchOptions {
            active_test: context.active_test(),
            context,
            ..SearchOptions::default()
        }
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            order: None,
            limit: None,
            offset: None,
            // Odoo's default: `active_test` is only ever turned off
            active_test: true,
            context: crate::context::Context::new(),
        }
    }
}

/// What a single `Binary` field may carry, in bytes of decoded content.
///
/// Odoo has no cap of its own and leans on the HTTP layer's; a hard
/// number here is what keeps a 200 MB paste from becoming a 270 MB JSON
/// response that the client tries to hold in a string.
pub const MAX_BINARY_BYTES: usize = 64 * 1024 * 1024;

/// Bytes on the way in: base64, well-formed, and within the cap.
fn check_base64(name: &str, value: Value) -> Result<Value, RusdooError> {
    use base64::Engine;
    let text = match &value {
        // Odoo's "no bytes" is `false`, and an empty string means the
        // same thing — both clear the column
        Value::Null | Value::Bool(false) => return Ok(Value::Null),
        Value::String(text) if text.is_empty() => return Ok(Value::Null),
        Value::String(text) => text,
        other => {
            return Err(RusdooError::Validation(format!(
                "{name}: a binary field takes base64, not {other}"
            )))
        }
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|error| {
            RusdooError::Validation(format!("{name}: invalid base64 ({error})"))
        })?;
    if decoded.len() > MAX_BINARY_BYTES {
        return Err(RusdooError::Validation(format!(
            "{name}: {} bytes excedem o limite de {MAX_BINARY_BYTES}",
            decoded.len()
        )));
    }
    Ok(value)
}

impl Model {
    pub fn search_sql(
        &self,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        self.search_sql_with(domain, opts, Ctx::model(self).in_lang(opts.context.lang()))
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
        // an order the caller sent empty is no order at all, not an
        // error: the web client spells "I have no preference" as `""`,
        // and Odoo reads any falsy order as the model's own
        let order = opts
            .order
            .as_deref()
            .filter(|spec| !spec.trim().is_empty())
            .unwrap_or_else(|| self.order());
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
        self.count_sql_with(domain, opts, Ctx::model(self).in_lang(opts.context.lang()))
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
        self.read_sql_in(ids, fields, crate::context::DEFAULT_LANG)
    }

    /// The same, answered in `lang`.
    ///
    /// Only translated columns care: for them the select becomes
    /// `COALESCE(col->>'pt_BR', col->>'en_US')`, Odoo's own fallback
    /// chain (`fields_textual.py::get_translation_fallback_langs`). A
    /// value missing in the asked-for language falls back to the source
    /// one rather than to nothing — a screen half in blank is worse than
    /// a screen half in English.
    pub fn read_sql_in(
        &self,
        ids: &[i64],
        fields: &[&str],
        lang: &str,
    ) -> Result<(String, Vec<Value>), RusdooError> {
        let mut columns = vec![r#""id""#.to_string()];
        for name in fields {
            self.stored_field(name)?;
            let quoted = quote_ident(name)?;
            if self.field(name).is_some_and(|f| f.translate) {
                columns.push(format!(
                    "{} AS {quoted}",
                    crate::sql::translated_read(&quoted, lang)?
                ));
                continue;
            }
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
        self.insert_sql_in(uid, values, crate::context::DEFAULT_LANG)
    }

    /// The same, with translated values landing under `lang` as well as
    /// under the source language.
    pub fn insert_sql_in(
        &self,
        uid: i64,
        values: Vec<(&str, Value)>,
        lang: &str,
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
            let placeholder = bind_or_null(&mut params, self.typed_value(name, value)?);
            placeholders.push(if self.field(name).is_some_and(|f| f.translate) {
                // no column to merge into yet: this row is being born
                Self::translated_value(&placeholder, lang)
            } else {
                self.bound_expr(name, &placeholder)
            });
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
        self.update_sql_in(uid, ids, values, crate::context::DEFAULT_LANG)
    }

    /// The same, writing translated values into `lang`.
    pub fn update_sql_in(
        &self,
        uid: i64,
        ids: &[i64],
        values: Vec<(&str, Value)>,
        lang: &str,
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
            let placeholder = bind_or_null(&mut params, self.typed_value(name, value)?);
            let column = quote_ident(name)?;
            let expr = if self.field(name).is_some_and(|f| f.translate) {
                Self::translated_merge(&placeholder, &column, lang)
            } else {
                self.bound_expr(name, &placeholder)
            };
            assignments.push(format!("{column} = {expr}"));
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
            .filter(|part| !part.trim().is_empty())
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
    fn typed_value(&self, name: &str, value: Value) -> Result<Value, RusdooError> {
        let Some(field) = self.field(name) else {
            return Ok(value);
        };
        // bytes are checked before they reach the database: `decode()`
        // would refuse malformed base64 with a message about a SQL
        // function, and the size cap has to be applied to what the
        // client sent, not to what came back out of it
        if matches!(field.ty, FieldType::Binary) {
            return check_base64(name, value);
        }
        let Value::Number(number) = &value else {
            return Ok(value);
        };
        Ok(match field.ty {
            FieldType::Float { .. } | FieldType::Monetary => {
                number.as_f64().map_or(value.clone(), Value::from)
            }
            FieldType::Integer | FieldType::Many2one { .. } => {
                number.as_i64().map_or(value.clone(), Value::from)
            }
            _ => value,
        })
    }

    pub(crate) fn column_cast(&self, name: &str) -> &'static str {
        crate::sql::value_cast_for(self.field(name).map(|f| &f.ty))
    }

    /// How a bound parameter reaches its column.
    ///
    /// Almost always the placeholder with a cast after it. Bytes are the
    /// exception: they arrive as base64 text and `::bytea` would store
    /// the *text*, so PostgreSQL has to decode it — which is a wrapper,
    /// not a suffix.
    fn bound_expr(&self, name: &str, placeholder: &str) -> String {
        if matches!(self.field(name).map(|f| &f.ty), Some(FieldType::Binary)) {
            return format!("decode({placeholder}, 'base64')");
        }
        format!("{placeholder}{}", self.column_cast(name))
    }

    /// How a value reaches a translated column, in `lang`.
    ///
    /// A merge, never a replacement: writing the Portuguese name of a
    /// product must not delete its English one. `en_US` is written too
    /// on the way in, because that is the source value — the same thing
    /// Odoo's `convert_to_column_insert` does.
    fn translated_value(placeholder: &str, lang: &str) -> String {
        if lang == crate::context::DEFAULT_LANG {
            return format!("jsonb_build_object('en_US', {placeholder})");
        }
        format!(
            "jsonb_build_object('en_US', {placeholder}, {}, {placeholder})",
            crate::sql::quote_literal(lang)
        )
    }

    /// How a value reaches a translated column on an update: a merge of
    /// the caller's language *only*.
    ///
    /// Not the same as the insert. On the way in, the text is the source
    /// value and lands under both keys; on the way out, writing the
    /// Portuguese name of a product must change Portuguese and nothing
    /// else — writing both would make every translation overwrite the
    /// original it was translated from.
    fn translated_merge(placeholder: &str, column: &str, lang: &str) -> String {
        format!(
            "COALESCE({column}, '{{}}'::jsonb) || jsonb_build_object({}, {placeholder})",
            crate::sql::quote_literal(lang)
        )
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
        model.search_sql_with(domain, opts, Ctx::full(model, self).in_lang(opts.context.lang()))
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
        self.read_query_in(model_name, ids, fields, crate::context::DEFAULT_LANG)
    }

    /// The same, answered in `lang`.
    pub(crate) fn read_query_in(
        &self,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
        lang: &str,
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
            if field.translate {
                columns.push(format!(
                    "{} AS {}",
                    crate::sql::translated_read(&col, lang)?,
                    quote_ident(name)?
                ));
                resolved.push(ResolvedColumn {
                    name: (*name).to_string(),
                    field,
                });
                continue;
            }
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
