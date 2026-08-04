//! Execution layer: runs the SQL built by `crud`/`ddl` on a PgPool.
//!
//! Values travel as `serde_json::Value` (the JSON-RPC wire format of the
//! Odoo web client), bound as typed PostgreSQL parameters here.

use crate::crud::{bind_ids, SearchOptions};
use crate::ddl::{create_relation_table_sql, create_table_sql};
use crate::domain::Domain;
use crate::fields::{Field, FieldType};
use crate::group::{Aggregate, GroupBy, GroupOptions};
use crate::model::Model;
use crate::registry::Registry;
use crate::sql::{bind, quote_ident};
use rusdoo_core::{RusdooError, SUPERUSER_ID};
use serde_json::{json, Map, Value};
use sqlx::postgres::{PgArguments, PgPool, PgPoolOptions, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

const MAX_POOL_CONNECTIONS: u32 = 5;

/// A read on a caller's connection, boxed so the pipeline can recurse
/// into itself (a related hop and a compute both read again).
type ReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Map<String, Value>>, RusdooError>> + Send + 'a>>;

/// One hop of a related read, boxed so the walk can recurse.
type RelatedFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HashMap<i64, Value>, RusdooError>> + Send + 'a>>;

/// How far a `related` path may reach. The chain is declared in code,
/// not by a caller, but a cycle through it would loop forever.
const MAX_RELATED_DEPTH: usize = 8;

/// Pool options shared by every connector. Pins each connection's session
/// timezone to UTC so `CURRENT_TIMESTAMP` (a `timestamptz`) lands in our
/// `timestamp` audit columns as UTC wall-clock, not the server's local
/// zone — otherwise `create_date`/`write_date` would silently drift.
fn pool_options() -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(MAX_POOL_CONNECTIONS)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET TIME ZONE 'UTC'").await?;
                Ok(())
            })
        })
}

pub async fn connect(url: &str) -> Result<PgPool, RusdooError> {
    pool_options().connect(url).await.map_err(db_err)
}

/// Pool that only connects on first use — for tests and tooling.
pub fn lazy_pool(url: &str) -> Result<PgPool, RusdooError> {
    pool_options().connect_lazy(url).map_err(db_err)
}

/// PostgreSQL's SQLSTATEs for a reference the database will not allow.
///
/// Two, not one: `23503` is a plain foreign key violation — what an
/// insert pointing at nothing gets — and `23001` is the one a `RESTRICT`
/// raises when something still points at the row being deleted. They are
/// different codes for the two sides of the same reference.
const FOREIGN_KEY_VIOLATION: &str = "23503";
const RESTRICT_VIOLATION: &str = "23001";

fn db_err(e: sqlx::Error) -> RusdooError {
    RusdooError::Database(e.to_string())
}

impl Model {
    /// A database error turned into something the user can act on.
    ///
    /// PostgreSQL says `duplicate key value violates unique constraint
    /// "ir_config_parameter_key_uniq"`. The model already knows what that
    /// constraint is for and said so when it declared it; showing the
    /// declared message instead of the driver's is the whole point of
    /// having declared it.
    pub(crate) fn explain(&self, error: sqlx::Error) -> RusdooError {
        self.explain_for(error, Wrote::Record)
    }

    /// The same, told which side of the reference the caller was on.
    ///
    /// A broken foreign key gives PostgreSQL the same SQLSTATE either
    /// way, and the two mean opposite things: writing one means the
    /// record you pointed at is not there, deleting one means something
    /// still points at you. A message that guessed wrong would send the
    /// user looking in the wrong place.
    pub(crate) fn explain_for(&self, error: sqlx::Error, wrote: Wrote) -> RusdooError {
        let sqlx::Error::Database(ref db) = error else {
            return db_err(error);
        };
        if let Some(constraint) = db.constraint().and_then(|name| {
            self.sql_constraints()
                .iter()
                .find(|constraint| constraint.name == name)
        }) {
            return RusdooError::Validation(constraint.message.clone());
        }
        // a foreign key has no declared message — it was not declared at
        // all, it came from the reference itself — so the message is
        // built from what the database refused. The constraint's *name*
        // is not always there: a RESTRICT reported against the table on
        // the other side of the reference arrives without one, and the
        // reading does not depend on it.
        if matches!(
            db.code().as_deref(),
            Some(FOREIGN_KEY_VIOLATION) | Some(RESTRICT_VIOLATION)
        ) {
            return RusdooError::Validation(match wrote {
                Wrote::Record => {
                    let column = db
                        .constraint()
                        .and_then(|name| {
                            name.strip_prefix(&format!("{}_", self.meta.table))
                                .and_then(|rest| rest.strip_suffix("_fkey"))
                        })
                        .map(|column| format!(".{column}"))
                        .unwrap_or_default();
                    format!(
                        "{}{column} points at a record that does not exist",
                        self.meta.name
                    )
                }
                Wrote::Deletion => {
                    "records depend on this one: delete them or unlink them first".into()
                }
            });
        }
        db_err(error)
    }
}

/// Which side of a reference the failing statement was on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Wrote {
    Record,
    Deletion,
}

fn bind_value<'q>(query: PgQuery<'q>, value: &'q Value) -> Result<PgQuery<'q>, RusdooError> {
    Ok(match value {
        Value::Null => query.bind(None::<String>),
        Value::Bool(b) => query.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                return Err(RusdooError::Validation(format!(
                    "unsupported numeric parameter: {n}"
                )));
            }
        }
        Value::String(s) => query.bind(s.as_str()),
        // arrays/objects land in jsonb columns
        Value::Array(_) | Value::Object(_) => query.bind(value.clone()),
    })
}

/// Run a statement on a connection the caller already has, translating a
/// constraint violation into the message the model declared.
pub(crate) async fn execute_in(
    conn: &mut PgConnection,
    sql: &str,
    params: &[Value],
    model: &Model,
) -> Result<u64, RusdooError> {
    let done = build_query(sql, params)?
        .execute(conn)
        .await
        .map_err(|error| model.explain_for(error, Wrote::Deletion))?;
    Ok(done.rows_affected())
}

fn build_query<'q>(sql: &'q str, params: &'q [Value]) -> Result<PgQuery<'q>, RusdooError> {
    let mut query = sqlx::query(sql);
    for value in params {
        query = bind_value(query, value)?;
    }
    Ok(query)
}

/// `id` columns are SERIAL (int4); exposed as i64 like Odoo's XML-RPC ids.
fn row_id(row: &PgRow) -> Result<i64, RusdooError> {
    row.try_get::<i32, _>("id").map(i64::from).map_err(db_err)
}

impl Model {
    pub async fn init_table(&self, pool: &PgPool) -> Result<(), RusdooError> {
        // one transaction: never leave a half-initialized schema
        let mut tx = pool.begin().await.map_err(db_err)?;
        let sql = create_table_sql(self)?;
        sqlx::query(&sql).execute(&mut *tx).await.map_err(db_err)?;
        // the table may predate a field the model gained since — from an
        // upgrade, or from another module extending this model. Adding
        // the column here is what makes either of those work on a
        // database that already has data.
        let mut wants_not_null = Vec::new();
        for (statement, required) in crate::ddl::add_missing_columns_sql(self)? {
            sqlx::query(&statement)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            if required {
                wants_not_null.push(statement);
            }
        }
        for field in self.fields() {
            if let Some(rel_sql) = create_relation_table_sql(field)? {
                sqlx::query(&rel_sql)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)?;

        // the constraint comes after the commit, one statement at a
        // time: a table with rows in it rejects a NOT NULL column, and a
        // failed statement inside the transaction above would take the
        // column it belongs to down with it.
        for statement in wants_not_null {
            let Some(column) = statement
                .rsplit("ADD COLUMN IF NOT EXISTS ")
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .map(|column| column.trim_matches('"').to_string())
            else {
                continue;
            };
            let not_null = crate::ddl::set_not_null_sql(self, &column)?;
            if sqlx::query(&not_null).execute(pool).await.is_err() {
                // saying so beats refusing the upgrade: the field is
                // required in the model, and the rows that predate it
                // are the ones the database cannot vouch for
                tracing::warn!(
                    "{}: column {column:?} is required, but rows exist without a value — \
                     the NOT NULL constraint was not applied",
                    self.meta.name
                );
            }
        }
        self.convert_translated_columns(pool).await?;
        self.init_sql_constraints(pool).await?;
        Ok(())
    }

    /// Convert a column whose field became translatable — or stopped
    /// being — port of `tools/sql.py::convert_column_translatable`.
    ///
    /// Without this, marking an existing field `translatable()` is a
    /// server that boots and then fails every read of that column: the
    /// model says `jsonb` and the table still says `varchar`. Nobody
    /// upgrading would see it until the first screen that shows the
    /// field.
    ///
    /// The existing text becomes the source value, which is what it was.
    async fn convert_translated_columns(&self, pool: &PgPool) -> Result<(), RusdooError> {
        for field in self.fields() {
            let Some(wanted) = field.column_type() else {
                continue;
            };
            // a `Json` field is jsonb and always was: only a field whose
            // *translatability* changed has a column to convert
            if matches!(field.ty, crate::fields::FieldType::Json) {
                continue;
            }
            let current: Option<String> = sqlx::query_scalar(
                "SELECT udt_name FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
            )
            .bind(&self.meta.table)
            .bind(&field.name)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
            let Some(current) = current else {
                continue;
            };
            let column = crate::sql::quote_ident(&field.name)?;
            let table = crate::sql::quote_ident(&self.meta.table)?;
            let statement = match (field.translate, current.as_str()) {
                // already the shape the model asks for
                (true, "jsonb") => continue,
                (false, other) if other != "jsonb" => continue,
                (true, _) => format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} DROP DEFAULT, \
                     ALTER COLUMN {column} TYPE jsonb \
                     USING CASE WHEN {column} IS NOT NULL \
                       THEN jsonb_build_object('en_US', {column}::varchar) END"
                ),
                (false, _) => format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} DROP DEFAULT, \
                     ALTER COLUMN {column} TYPE {wanted} USING {column}->>'en_US'"
                ),
            };
            if let Err(error) = sqlx::query(&statement).execute(pool).await {
                tracing::warn!(
                    "{}: column {:?} could not be converted to {wanted} ({error})",
                    self.meta.name,
                    field.name
                );
            } else {
                tracing::info!(
                    "{}: coluna {:?} convertida de {current} para {wanted}",
                    self.meta.name,
                    field.name
                );
            }
        }
        Ok(())
    }

    /// The `_sql_constraints` of this model, added to the table if they
    /// are not there yet.
    ///
    /// Same shape as the NOT NULL pass above and for the same reason:
    /// outside the transaction, one at a time, and a constraint the
    /// existing rows already violate is reported rather than allowed to
    /// refuse the whole upgrade. What is refused is the *rule*, not the
    /// boot — and the log says which, because a uniqueness rule that is
    /// silently absent is worse than one that never existed.
    async fn init_sql_constraints(&self, pool: &PgPool) -> Result<(), RusdooError> {
        for constraint in self.sql_constraints() {
            let existing: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM pg_constraint WHERE conname = $1
                 AND conrelid = to_regclass($2)",
            )
            .bind(&constraint.name)
            .bind(&self.meta.table)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
            if existing.is_some() {
                continue;
            }
            let sql = crate::ddl::add_constraint_sql(self, constraint)?;
            if let Err(error) = sqlx::query(&sql).execute(pool).await {
                tracing::warn!(
                    "{}: constraint {:?} could not be applied ({error}) — \
                     the rows already there violate it",
                    self.meta.name,
                    constraint.name
                );
            }
        }
        Ok(())
    }

    pub async fn search(
        &self,
        pool: &PgPool,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Vec<i64>, RusdooError> {
        let (sql, params) = self.search_sql(domain, opts)?;
        fetch_ids(pool, &sql, &params).await
    }

    pub async fn create(
        &self,
        pool: &PgPool,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        let mut conn = pool.acquire().await.map_err(db_err)?;
        self.create_conn(&mut conn, SUPERUSER_ID, values).await
    }

    pub(crate) async fn create_conn(
        &self,
        conn: &mut PgConnection,
        uid: i64,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_conn_lang(conn, uid, values, crate::context::DEFAULT_LANG)
            .await
    }

    pub(crate) async fn create_conn_lang(
        &self,
        conn: &mut PgConnection,
        uid: i64,
        values: Vec<(&str, Value)>,
        lang: &str,
    ) -> Result<i64, RusdooError> {
        let (sql, params) = self.insert_sql_in(uid, values, lang)?;
        let row = build_query(&sql, &params)?
            .fetch_one(&mut *conn)
            .await
            .map_err(|error| self.explain(error))?;
        row_id(&row)
    }

    pub async fn read(
        &self,
        pool: &PgPool,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        self.read_lang(pool, ids, fields, crate::context::DEFAULT_LANG)
            .await
    }

    /// `read` answered in `lang`: translated fields come back in it,
    /// falling back to the source language.
    pub async fn read_lang(
        &self,
        pool: &PgPool,
        ids: &[i64],
        fields: &[&str],
        lang: &str,
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        // `id` is always in the answer, and asking for it explicitly is
        // legal (Odoo's read does the same) — it is not a column to select
        let fields = &without_id(fields)[..];
        let (sql, params) = self.read_sql_in(ids, fields, lang)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
        let mut records: Vec<Map<String, Value>> = rows
            .iter()
            .map(|row| self.row_to_json(row, fields))
            .collect::<Result<_, RusdooError>>()?;
        order_like(&mut records, ids);
        Ok(records)
    }

    /// `read` on a connection the caller already holds — what a hook
    /// running inside a delete's transaction needs.
    pub(crate) async fn read_in(
        &self,
        conn: &mut PgConnection,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let fields = &without_id(fields)[..];
        let (sql, params) = self.read_sql(ids, fields)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(conn)
            .await
            .map_err(db_err)?;
        let mut records: Vec<Map<String, Value>> = rows
            .iter()
            .map(|row| self.row_to_json(row, fields))
            .collect::<Result<_, RusdooError>>()?;
        order_like(&mut records, ids);
        Ok(records)
    }

    pub async fn write(
        &self,
        pool: &PgPool,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<u64, RusdooError> {
        let mut conn = pool.acquire().await.map_err(db_err)?;
        self.write_conn(&mut conn, SUPERUSER_ID, ids, values).await
    }

    pub(crate) async fn write_conn(
        &self,
        conn: &mut PgConnection,
        uid: i64,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<u64, RusdooError> {
        self.write_conn_lang(conn, uid, ids, values, crate::context::DEFAULT_LANG)
            .await
    }

    pub(crate) async fn write_conn_lang(
        &self,
        conn: &mut PgConnection,
        uid: i64,
        ids: &[i64],
        values: Vec<(&str, Value)>,
        lang: &str,
    ) -> Result<u64, RusdooError> {
        let (sql, params) = self.update_sql_in(uid, ids, values, lang)?;
        let done = build_query(&sql, &params)?
            .execute(&mut *conn)
            .await
            .map_err(|error| self.explain(error))?;
        Ok(done.rows_affected())
    }

    pub async fn unlink(&self, pool: &PgPool, ids: &[i64]) -> Result<u64, RusdooError> {
        let (sql, params) = self.delete_sql(ids)?;
        let done = build_query(&sql, &params)?
            .execute(pool)
            .await
            .map_err(|error| self.explain_for(error, Wrote::Deletion))?;
        Ok(done.rows_affected())
    }

    fn row_to_json(&self, row: &PgRow, fields: &[&str]) -> Result<Map<String, Value>, RusdooError> {
        let mut record = Map::new();
        record.insert("id".into(), Value::from(row_id(row)?));
        for name in fields {
            let field = self.field(name).ok_or_else(|| {
                RusdooError::Validation(format!("unknown field on {}: {name:?}", self.meta.name))
            })?;
            record.insert((*name).to_string(), decode_field(row, name, field)?);
        }
        Ok(record)
    }
}

/// Put `records` back in the order their ids were asked for. Ids with no
/// row (deleted between the search and the read) simply are not there.
fn order_like(records: &mut [Map<String, Value>], ids: &[i64]) {
    let position: HashMap<i64, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    records.sort_by_key(|record| {
        record
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|id| position.get(&id).copied())
            .unwrap_or(usize::MAX)
    });
}

/// The requested fields minus `id`, which every read returns anyway and
/// no model declares as a column.
fn without_id<'a>(fields: &[&'a str]) -> Vec<&'a str> {
    fields.iter().copied().filter(|name| *name != "id").collect()
}

impl Registry {
    /// How many records the domain matches, counted in the database.
    pub async fn search_count(
        &self,
        pool: &PgPool,
        model_name: &str,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<i64, RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let (sql, params) =
            model.count_sql_with(
                domain,
                opts,
                crate::sql::Ctx::full(model, self).in_lang(opts.context.lang()),
            )?;
        let row = build_query(&sql, &params)?
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
        row.try_get::<i64, _>(0).map_err(db_err)
    }

    /// Run a grouped read: one map per group, keyed by the specs the
    /// caller asked for (`"country_id"`, `"__count"`, `"qty:sum"`, ...).
    /// Values arrive as JSON because every aggregate produces its own SQL
    /// type; `to_jsonb` in the query makes the decoding uniform.
    pub async fn read_group(
        &self,
        pool: &PgPool,
        model_name: &str,
        domain: &Domain,
        groupby: &[GroupBy],
        aggregates: &[Aggregate],
        opts: &GroupOptions,
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let query = self.read_group_sql(model_name, domain, groupby, aggregates, opts)?;
        let rows = build_query(&query.sql, &query.params)?
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|row| {
                let mut group = Map::new();
                for column in &query.columns {
                    // an empty group value (NULL) selects as SQL NULL, not
                    // as a JSON null — decode it as one
                    let value: Option<Value> =
                        row.try_get(column.alias.as_str()).map_err(db_err)?;
                    group.insert(column.spec.clone(), value.unwrap_or(Value::Null));
                }
                Ok(group)
            })
            .collect()
    }

    /// Like [`Model::search`], but with relational context: dotted paths
    /// and any/not any resolve against this registry.
    pub async fn search(
        &self,
        pool: &PgPool,
        model_name: &str,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Vec<i64>, RusdooError> {
        let (sql, params) = self.search_sql(model_name, domain, opts)?;
        fetch_ids(pool, &sql, &params).await
    }
}

async fn fetch_ids(pool: &PgPool, sql: &str, params: &[Value]) -> Result<Vec<i64>, RusdooError> {
    let rows = build_query(sql, params)?
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
    rows.iter().map(row_id).collect()
}

/// Hops of (link field, parent model) grouping delegated writes.
type DelegationChain = Vec<(String, String)>;

/// x2many fields split out of a create/write with their command tuples.
type X2manyCommands = Vec<(Field, Vec<Value>)>;

/// Result of splitting create/write values into scalar columns + x2many.
type SplitValues<'a> = (Vec<(&'a str, Value)>, X2manyCommands);

/// Decode one column into JSON according to its field type; unset -> Null.
fn decode_field(row: &PgRow, name: &str, field: &Field) -> Result<Value, RusdooError> {
    let value = match &field.ty {
        FieldType::Boolean => row
            .try_get::<Option<bool>, _>(name)
            .map_err(db_err)?
            .map(Value::from),
        FieldType::Integer | FieldType::Many2one { .. } => row
            .try_get::<Option<i32>, _>(name)
            .map_err(db_err)?
            .map(|v| Value::from(i64::from(v))),
        // fixed-precision columns are selected as float8 (see read_cast_for)
        FieldType::Float { .. } | FieldType::Monetary => row
            .try_get::<Option<f64>, _>(name)
            .map_err(db_err)?
            .map(Value::from),
        FieldType::Char { .. } | FieldType::Text | FieldType::Html | FieldType::Selection(_) => row
            .try_get::<Option<String>, _>(name)
            .map_err(db_err)?
            .map(Value::from),
        // Odoo serializes datetimes as "YYYY-MM-DD HH:MM:SS" and dates as
        // "YYYY-MM-DD" (naive, UTC) over the wire
        FieldType::Datetime => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(name)
            .map_err(db_err)?
            .map(|dt| Value::from(dt.format("%Y-%m-%d %H:%M:%S").to_string())),
        FieldType::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(name)
            .map_err(db_err)?
            .map(|d| Value::from(d.format("%Y-%m-%d").to_string())),
        // jsonb round-trips as the structured value itself
        FieldType::Json => row.try_get::<Option<Value>, _>(name).map_err(db_err)?,
        // bytes travel as base64 over JSON-RPC, like Odoo's Binary. An
        // empty column answers null, not "": a client draws a missing
        // image differently from a zero-byte one.
        FieldType::Binary => row
            .try_get::<Option<Vec<u8>>, _>(name)
            .map_err(db_err)?
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| {
                use base64::Engine;
                Value::from(base64::engine::general_purpose::STANDARD.encode(bytes))
            }),
        other => {
            // explicit gap, never a silently-wrong value
            return Err(RusdooError::Validation(format!(
                "read not yet supported for field type {other:?}"
            )));
        }
    };
    Ok(value.unwrap_or(Value::Null))
}

impl Registry {
    /// Create with `_inherits` delegation, atomically: inside one
    /// transaction, parents are created first — or reused when the caller
    /// supplies the link value, in which case the delegated values are
    /// written onto the existing parent (like Odoo).
    pub async fn create(
        &self,
        pool: &PgPool,
        model_name: &str,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_as(pool, SUPERUSER_ID, model_name, values).await
    }

    /// Create attributed to `uid`: the acting user is stamped on
    /// `create_uid`/`write_uid` (LOG_ACCESS) down the whole delegation tree.
    pub async fn create_as(
        &self,
        pool: &PgPool,
        uid: i64,
        model_name: &str,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_as_lang(pool, uid, model_name, values, crate::context::DEFAULT_LANG)
            .await
    }

    /// [`Registry::create_as`] with the language its translated values
    /// are written under — the record is born with its source value and
    /// with the caller's language holding the same text.
    pub async fn create_as_lang(
        &self,
        pool: &PgPool,
        uid: i64,
        model_name: &str,
        values: Vec<(&str, Value)>,
        lang: &str,
    ) -> Result<i64, RusdooError> {
        let mut tx = pool.begin().await.map_err(db_err)?;
        let id = self
            .create_in_lang(&mut tx, uid, model_name, values, lang)
            .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(id)
    }

    /// Fill in the fields numbered by an `ir.sequence` that the caller
    /// left empty, drawing each number inside this create's own
    /// transaction. A caller who passed a number keeps it — importing
    /// documents that already have one must not renumber them.
    async fn draw_sequences<'a>(
        &self,
        conn: &mut PgConnection,
        model: &'a Model,
        mut values: Vec<(&'a str, Value)>,
    ) -> Result<Vec<(&'a str, Value)>, RusdooError> {
        for field in model.fields() {
            let Some(code) = field.sequence.as_deref() else {
                continue;
            };
            let given = values.iter().any(|(name, value)| {
                *name == field.name && !matches!(value, Value::Null | Value::Bool(false))
            });
            if given {
                continue;
            }
            let Some(number) = crate::sequence::next_by_code(&mut *conn, self, code).await? else {
                // a document that must be numbered and has no sequence is
                // a missing configuration, and saying which one beats a
                // not-null violation from the database
                if field.required {
                    return Err(RusdooError::Validation(format!(
                        "{}.{} is numbered by sequence {code:?}, which does not exist: \
                         install the module that defines it",
                        model.meta.name, field.name
                    )));
                }
                tracing::warn!(
                    "field {:?} of {} wants sequence {code:?}, which does not exist",
                    field.name,
                    model.meta.name
                );
                continue;
            };
            values.retain(|(name, _)| *name != field.name);
            values.push((field.name.as_str(), Value::from(number)));
        }
        Ok(values)
    }

    /// Run the dynamic defaults of every field the create left out.
    ///
    /// In the create's own transaction, next to the sequence draw and
    /// for the same reason: what a default reads must be what the record
    /// is about to be stored next to.
    async fn run_dynamic_defaults<'a>(
        &self,
        conn: &mut PgConnection,
        uid: i64,
        model: &'a Model,
        mut values: Vec<(&'a str, Value)>,
    ) -> Result<Vec<(&'a str, Value)>, RusdooError> {
        for field in model.fields() {
            let Some(func) = field.default_fn else {
                continue;
            };
            if field.readonly || !field.stored {
                continue;
            }
            // a value the caller passed always wins, including an
            // explicit null: "unset on purpose" is a decision
            if values.iter().any(|(name, _)| *name == field.name) {
                continue;
            }
            let ctx = crate::fields::DefaultCtx {
                registry: self,
                conn: &mut *conn,
                uid,
                model: &model.meta.name,
            };
            let value = func(ctx).await?;
            if value.is_null() {
                continue;
            }
            values.push((field.name.as_str(), value));
        }
        Ok(values)
    }

    fn create_in<'a>(
        &'a self,
        tx: &'a mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &'a str,
        values: Vec<(&'a str, Value)>,
    ) -> Pin<Box<dyn Future<Output = Result<i64, RusdooError>> + Send + 'a>> {
        self.create_in_lang(tx, uid, model_name, values, crate::context::DEFAULT_LANG)
    }

    /// [`Registry::create_in`] with the language a translated value is
    /// written under.
    ///
    /// The language travels as an argument because the ORM has no
    /// environment object yet; when the Python bridge needs one (issue
    /// #10), `(uid, context)` becomes that object and this parameter
    /// goes with it.
    fn create_in_lang<'a>(
        &'a self,
        tx: &'a mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &'a str,
        values: Vec<(&'a str, Value)>,
        lang: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<i64, RusdooError>> + Send + 'a>> {
        Box::pin(async move {
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            let (values, x2many) = self.split_x2many(model, values)?;
            let values = self
                .run_dynamic_defaults(&mut *tx, uid, model, values)
                .await?;
            let values = self.draw_sequences(&mut *tx, model, values).await?;
            if model.meta.inherits.is_empty() {
                let id = model.create_conn_lang(&mut *tx, uid, values, lang).await?;
                self.apply_x2many_all(&mut *tx, uid, &x2many, id).await?;
                self.recompute_stored(&mut *tx, model_name, &[id], None)
                    .await?;
                self.check_constraints(&mut *tx, model_name, &[id], None)
                    .await?;
                return Ok(id);
            }
            let mut local: Vec<(&str, Value)> = Vec::new();
            let mut per_parent: Vec<Vec<(&str, Value)>> =
                vec![Vec::new(); model.meta.inherits.len()];
            'values: for (name, value) in values {
                if model.field(name).is_some() {
                    local.push((name, value));
                    continue;
                }
                for (i, (parent_name, _)) in model.meta.inherits.iter().enumerate() {
                    let parent = self.get(parent_name).ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "_inherits parent not registered: {parent_name}"
                        ))
                    })?;
                    if self.owns_field(parent, name, 0) {
                        per_parent[i].push((name, value));
                        continue 'values;
                    }
                }
                return Err(RusdooError::Validation(format!(
                    "unknown field on {model_name}: {name:?}"
                )));
            }
            for (i, (parent_name, link)) in model.meta.inherits.iter().enumerate() {
                let parent_values = std::mem::take(&mut per_parent[i]);
                // caller supplied the link: reuse that parent and write the
                // delegated values onto it instead of creating a new row
                if let Some((_, link_value)) = local.iter().find(|(name, _)| *name == link.as_str())
                {
                    if !parent_values.is_empty() {
                        let parent_id = link_value.as_i64().ok_or_else(|| {
                            RusdooError::Validation(format!(
                                "link field {link:?} must hold an integer id"
                            ))
                        })?;
                        // in the caller's language, like the local values:
                        // a record born in Portuguese has its delegated
                        // name in Portuguese too, or half of it is stored
                        // under a language nobody asked for
                        self.write_in_lang(
                            &mut *tx,
                            uid,
                            parent_name,
                            &[parent_id],
                            parent_values,
                            lang,
                        )
                        .await?;
                    }
                    continue;
                }
                let parent_id = self
                    .create_in_lang(&mut *tx, uid, parent_name, parent_values, lang)
                    .await?;
                local.push((link.as_str(), Value::from(parent_id)));
            }
            let id = model.create_conn(&mut *tx, uid, local).await?;
            self.apply_x2many_all(&mut *tx, uid, &x2many, id).await?;
            self.recompute_stored(&mut *tx, model_name, &[id], None)
                .await?;
            self.check_constraints(&mut *tx, model_name, &[id], None)
                .await?;
            Ok(id)
        })
    }

    /// Like [`Registry::create`], inside a caller-managed transaction.
    pub async fn create_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_in(tx, SUPERUSER_ID, model_name, values).await
    }

    /// Like [`Registry::create_as`], inside a caller-managed transaction:
    /// the caller can still refuse the record after it exists but before
    /// anyone else can see it.
    pub async fn create_as_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &str,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_in(tx, uid, model_name, values).await
    }

    /// Like [`Registry::search`], inside a caller-managed transaction —
    /// it therefore sees that transaction's uncommitted rows.
    pub async fn search_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Vec<i64>, RusdooError> {
        let (sql, params) = self.search_sql(model_name, domain, opts)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(&mut **tx)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_id).collect()
    }

    /// Like [`Registry::write`], inside a caller-managed transaction.
    pub async fn write_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        self.write_in(tx, SUPERUSER_ID, model_name, ids, values)
            .await
    }

    /// Read with `_inherits` delegation (LEFT JOINs on the link fields).
    pub async fn read(
        &self,
        pool: &PgPool,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        self.read_lang(pool, model_name, ids, fields, crate::context::DEFAULT_LANG)
            .await
    }

    /// [`Registry::read`] answered in `lang` — what a client's context
    /// asked for.
    pub async fn read_lang(
        &self,
        pool: &PgPool,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
        lang: &str,
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let mut conn = pool.acquire().await.map_err(db_err)?;
        self.read_conn_lang(&mut conn, model_name, ids, fields, lang)
            .await
    }

    /// [`Registry::read`] on a caller's connection. A recompute inside a
    /// write has to read through the very transaction that wrote, or it
    /// would not see the row it is recomputing for.
    pub fn read_conn<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        fields: &'a [&'a str],
    ) -> ReadFuture<'a> {
        self.read_conn_lang(conn, model_name, ids, fields, crate::context::DEFAULT_LANG)
    }

    /// [`Registry::read_conn`] in `lang`.
    pub fn read_conn_lang<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        fields: &'a [&'a str],
        lang: &'a str,
    ) -> ReadFuture<'a> {
        self.read_conn_deep(conn, model_name, ids, fields, lang, 0)
    }

    /// [`Registry::read_conn_lang`] with the hop count carried in.
    ///
    /// The count measures hops in the *data* — following a relation —
    /// not entries into a function. Entering a related or a computed
    /// field is continuing the same read, and counting those too made a
    /// legitimate three-level unit-of-measure chain blow a ceiling meant
    /// for eight relations.
    ///
    /// The count has to survive the *whole* read, not each entry point.
    /// A related field walks into a comodel, which may itself have a
    /// related or a computed field, which reads again — and the graph of
    /// models is not a tree. On cyclic data (`a.parent_id = b`,
    /// `b.parent_id = a`) a counter that restarted on every hop never
    /// reached its ceiling, so the recursion had nothing to stop it and
    /// the process died on a stack overflow rather than answering an
    /// error. That is reachable from any screen showing such a field.
    #[allow(clippy::too_many_arguments)]
    fn read_conn_deep<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        fields: &'a [&'a str],
        lang: &'a str,
        depth: usize,
    ) -> ReadFuture<'a> {
        Box::pin(async move {
            if depth > MAX_RELATED_DEPTH {
                return Err(RusdooError::Validation(format!(
                    "reading {model_name} went more than {MAX_RELATED_DEPTH} hops deep: \
                     a related or computed field points in a circle"
                )));
            }
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;

            // fields without a column of their own are split out and fetched
            // separately: x2many from the relation table / inverse column,
            // related fields by following their path
            let fields = &without_id(fields)[..];
            let mut scalar: Vec<&str> = Vec::new();
            let mut x2many: Vec<&Field> = Vec::new();
            let mut related: Vec<&Field> = Vec::new();
            let mut computed: Vec<&Field> = Vec::new();
            for name in fields {
                let field = model.field(name);
                // a stored compute has a column: it reads from there, and is
                // kept current by the recompute on write
                if let Some(field) = field.filter(|f| f.compute.is_some() && !f.stored) {
                    computed.push(field);
                    continue;
                }
                if let Some(field) = field.filter(|f| f.related.is_some()) {
                    related.push(field);
                    continue;
                }
                match field.map(|f| &f.ty) {
                    Some(FieldType::Many2many { .. } | FieldType::One2many { .. }) => {
                        x2many.push(field.expect("just matched"));
                    }
                    _ => scalar.push(name),
                }
            }

            let (sql, params, resolved) = self.read_query_in(model_name, ids, &scalar, lang)?;
            let rows = build_query(&sql, &params)?
                .fetch_all(&mut *conn)
                .await
                .map_err(db_err)?;
            let mut records: Vec<Map<String, Value>> = rows
                .iter()
                .map(|row| {
                    let mut record = Map::new();
                    record.insert("id".into(), Value::from(row_id(row)?));
                    for column in &resolved {
                        record.insert(
                            column.name.clone(),
                            decode_field(row, &column.name, &column.field)?,
                        );
                    }
                    Ok(record)
                })
                .collect::<Result<_, RusdooError>>()?;
            // `WHERE id IN (...)` returns rows in whatever order the
            // database liked. The caller asked for these ids in an order
            // — a search's `ORDER BY`, a thread's newest-first — and
            // losing it here would silently scramble every sorted list.
            order_like(&mut records, ids);

            for field in x2many {
                let related = self.read_x2many(&mut *conn, field, ids).await?;
                for record in &mut records {
                    let owner = record["id"].as_i64().expect("id present");
                    let list = related.get(&owner).cloned().unwrap_or_default();
                    record.insert(field.name.clone(), Value::from(list));
                }
            }

            for field in related {
                let path = field.related.as_deref().expect("filtered above");
                let values = self
                    .read_related(&mut *conn, model_name, ids, path, depth)
                    .await?;
                for record in &mut records {
                    let owner = record["id"].as_i64().expect("id present");
                    let value = values.get(&owner).cloned().unwrap_or(Value::Null);
                    record.insert(field.name.clone(), value);
                }
            }

            // computed last: their dependencies may be plain columns, related
            // fields or other computed ones, and reading them goes back
            // through this same path
            for field in computed {
                let values = self
                    .read_computed(&mut *conn, model_name, ids, field, depth)
                    .await?;
                for record in &mut records {
                    let owner = record["id"].as_i64().expect("id present");
                    let value = values.get(&owner).cloned().unwrap_or(Value::Null);
                    record.insert(field.name.clone(), value);
                }
            }

            // many2one reads as [id, display_name], like Odoo's name_get
            let m2o: Vec<(String, String)> = fields
                .iter()
                .filter_map(|name| match model.field(name).map(|f| &f.ty) {
                    Some(FieldType::Many2one { comodel }) => {
                        Some((name.to_string(), comodel.clone()))
                    }
                    _ => None,
                })
                .collect();
            for (name, comodel) in m2o {
                let linked: Vec<i64> = records
                    .iter()
                    .filter_map(|r| r.get(&name).and_then(Value::as_i64))
                    .collect();
                if linked.is_empty() {
                    continue;
                }
                let names = self
                    .display_names_conn(&mut *conn, &comodel, &linked)
                    .await?;
                for record in &mut records {
                    if let Some(id) = record.get(&name).and_then(Value::as_i64) {
                        let display = names.get(&id).cloned().unwrap_or_default();
                        record.insert(
                            name.clone(),
                            Value::Array(vec![Value::from(id), Value::from(display)]),
                        );
                    }
                }
            }
            Ok(records)
        })
    }

    /// Hold `ids` to the model's constraints, inside the caller's
    /// transaction — a record the rules refuse is rolled back before
    /// anyone else can see it, which is the only way a check about a
    /// record that does not exist yet can mean anything.
    ///
    /// `changed` lists the fields a write touched; `None` means a
    /// create, where every constraint runs.
    pub(crate) async fn check_constraints(
        &self,
        conn: &mut PgConnection,
        model_name: &str,
        ids: &[i64],
        changed: Option<&[&str]>,
    ) -> Result<(), RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        if ids.is_empty() || model.constraints().is_empty() {
            return Ok(());
        }
        for constraint in model.constraints() {
            let touched = match changed {
                None => true,
                Some(changed) => constraint.fields.iter().any(|watched| {
                    let head = watched
                        .split_once('.')
                        .map_or(watched.as_str(), |(head, _)| head);
                    changed.contains(&head)
                }),
            };
            if !touched {
                continue;
            }
            let read: Vec<&str> = constraint.reads.iter().map(String::as_str).collect();
            let rows = self.read_conn(&mut *conn, model_name, ids, &read).await?;
            for row in rows {
                constraint.check.check(&row)?;
            }
        }
        Ok(())
    }

    /// Bring the stored computes of `ids` up to date, inside the caller's
    /// transaction — the rows were just written there and exist nowhere
    /// else yet.
    ///
    /// `changed` lists the fields the write touched; `None` means a
    /// create, where every stored compute has to run. A recompute is the
    /// ORM's own bookkeeping, not a user write: it goes straight to the
    /// column, without the readonly guard that keeps clients out of it
    /// and without stamping write_uid/write_date.
    pub(crate) async fn recompute_stored(
        &self,
        conn: &mut PgConnection,
        model_name: &str,
        ids: &[i64],
        changed: Option<&[&str]>,
    ) -> Result<(), RusdooError> {
        if ids.is_empty() {
            return Ok(());
        }
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let targets: Vec<Field> = model
            .fields()
            .iter()
            .filter(|f| f.stored && f.compute.is_some())
            .filter(|f| match changed {
                None => true,
                Some(changed) => f
                    .compute
                    .as_ref()
                    .expect("filtered above")
                    .depends
                    .iter()
                    // `order_line.price_subtotal` is watched by writing
                    // `order_line`: the commands that changed the lines
                    // came through the parent's write
                    .map(|dep| dep.split_once('.').map_or(dep.as_str(), |(head, _)| head))
                    .any(|dep| changed.contains(&dep)),
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let table = quote_ident(&model.meta.table)?;
        for field in targets {
            let values = self
                .read_computed(&mut *conn, model_name, ids, &field, 0)
                .await?;
            let column = quote_ident(&field.name)?;
            let cast = model.column_cast(&field.name);
            for (id, value) in values {
                let mut params = Vec::new();
                // a computed null is a null column, not a text parameter
                let placeholder = if value.is_null() {
                    "NULL".to_string()
                } else {
                    format!("{}{cast}", bind(&mut params, value))
                };
                let id_ph = bind(&mut params, Value::from(id));
                let sql =
                    format!(r#"UPDATE {table} SET {column} = {placeholder} WHERE "id" = {id_ph}"#);
                build_query(&sql, &params)?
                    .execute(&mut *conn)
                    .await
                    .map_err(db_err)?;
            }
        }
        Ok(())
    }

    /// Follow a related path from `ids` and bring the value back, keyed
    /// by the record it belongs to. Every hop but the last must be a
    /// many2one: that is the only link a single value can travel along.
    ///
    /// One query per hop, never one per record — the whole point of
    /// resolving the path here instead of per row.
    fn read_related<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        path: &'a str,
        depth: usize,
    ) -> RelatedFuture<'a> {
        Box::pin(async move {
            if depth > MAX_RELATED_DEPTH {
                return Err(RusdooError::Validation(format!(
                    "related path exceeds {MAX_RELATED_DEPTH} hops: {path:?}"
                )));
            }
            let Some((head, rest)) = path.split_once('.') else {
                // last hop: the value itself
                let rows = self
                    .read_conn_deep(
                        &mut *conn,
                        model_name,
                        ids,
                        &[path],
                        crate::context::DEFAULT_LANG,
                        depth,
                    )
                    .await?;
                return Ok(rows
                    .into_iter()
                    .filter_map(|mut row| {
                        let id = row.get("id")?.as_i64()?;
                        Some((id, row.remove(path).unwrap_or(Value::Null)))
                    })
                    .collect());
            };
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            let comodel = match model.field(head).map(|f| &f.ty) {
                Some(FieldType::Many2one { comodel }) => comodel.clone(),
                Some(_) => {
                    return Err(RusdooError::Validation(format!(
                        "related path {path:?}: {head:?} is not a many2one"
                    )))
                }
                None => {
                    return Err(RusdooError::Validation(format!(
                        "related path {path:?}: unknown field {head:?} on {model_name}"
                    )))
                }
            };
            // the hop: owner id -> linked id (a m2o reads as [id, name])
            let rows = self
                .read_conn_deep(
                    &mut *conn,
                    model_name,
                    ids,
                    &[head],
                    crate::context::DEFAULT_LANG,
                    depth,
                )
                .await?;
            let hops: Vec<(i64, i64)> = rows
                .iter()
                .filter_map(|row| {
                    let owner = row.get("id")?.as_i64()?;
                    let linked = row.get(head)?.as_array()?.first()?.as_i64()?;
                    Some((owner, linked))
                })
                .collect();
            if hops.is_empty() {
                return Ok(HashMap::new());
            }
            let mut linked_ids: Vec<i64> = hops.iter().map(|(_, linked)| *linked).collect();
            linked_ids.sort_unstable();
            linked_ids.dedup();
            let values = self
                .read_related(&mut *conn, &comodel, &linked_ids, rest, depth + 1)
                .await?;
            Ok(hops
                .into_iter()
                .map(|(owner, linked)| (owner, values.get(&linked).cloned().unwrap_or(Value::Null)))
                .collect())
        })
    }

    /// The values of `head.rest` for each record in `ids`, as a list per
    /// record — Odoo's `@api.depends('order_line.price_subtotal')`.
    ///
    /// Two queries whatever the number of parents: the links, then the
    /// field on every linked record at once. A many2one hop yields a
    /// one-element list, so a compute reads both shapes the same way.
    #[allow(clippy::too_many_arguments)]
    fn read_over_x2many<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        path: &'a str,
        owner_field: &'a str,
        depth: usize,
    ) -> RelatedFuture<'a> {
        Box::pin(async move {
            let (head, rest) = path.split_once('.').expect("caller checked for the dot");
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            let comodel = match model.field(head).map(|f| &f.ty) {
                Some(
                    FieldType::One2many { comodel, .. }
                    | FieldType::Many2many { comodel, .. }
                    | FieldType::Many2one { comodel },
                ) => comodel.clone(),
                _ => {
                    return Err(RusdooError::Validation(format!(
                        "computed field {owner_field:?} depends on {path:?}, but {head:?} is \
                         not a relational field"
                    )))
                }
            };
            // parent -> the records it links to
            let rows = self
                .read_conn_deep(
                    &mut *conn,
                    model_name,
                    ids,
                    &[head],
                    crate::context::DEFAULT_LANG,
                    depth,
                )
                .await?;
            let mut links: Vec<(i64, Vec<i64>)> = Vec::new();
            let mut linked_ids: Vec<i64> = Vec::new();
            for row in &rows {
                let Some(owner) = row.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let children: Vec<i64> = match row.get(head) {
                    // x2many reads as a list of ids, m2o as [id, name]
                    Some(Value::Array(items)) => match items.first() {
                        Some(Value::Number(_)) if items.len() == 2 && items[1].is_string() => {
                            items[0].as_i64().into_iter().collect()
                        }
                        _ => items.iter().filter_map(Value::as_i64).collect(),
                    },
                    Some(Value::Number(number)) => number.as_i64().into_iter().collect(),
                    _ => Vec::new(),
                };
                linked_ids.extend(children.iter().copied());
                links.push((owner, children));
            }
            linked_ids.sort_unstable();
            linked_ids.dedup();
            if linked_ids.is_empty() {
                return Ok(links
                    .into_iter()
                    .map(|(owner, _)| (owner, json!([])))
                    .collect());
            }
            // the field itself, on every linked record in one read
            let values = self
                .read_related(&mut *conn, &comodel, &linked_ids, rest, depth + 1)
                .await?;
            Ok(links
                .into_iter()
                .map(|(owner, children)| {
                    let gathered: Vec<Value> = children
                        .iter()
                        .map(|child| values.get(child).cloned().unwrap_or(Value::Null))
                        .collect();
                    (owner, Value::Array(gathered))
                })
                .collect())
        })
    }

    /// Run a field's compute over `ids`: read what it depends on, then
    /// call it once per record. One read for the whole batch, so a
    /// computed column costs a query, not a query per row.
    ///
    /// A dependency may itself be related or computed, which is why this
    /// goes back through `read` — and why the chain is depth-capped: a
    /// compute that depends on itself would otherwise loop forever.
    fn read_computed<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        model_name: &'a str,
        ids: &'a [i64],
        field: &'a Field,
        depth: usize,
    ) -> RelatedFuture<'a> {
        Box::pin(async move {
            if depth > MAX_RELATED_DEPTH {
                return Err(RusdooError::Validation(format!(
                    "compute chain exceeds {MAX_RELATED_DEPTH} levels at {:?}",
                    field.name
                )));
            }
            let compute = field.compute.as_ref().expect("computed field");
            if compute.depends.is_empty() {
                return Err(RusdooError::Validation(format!(
                    "computed field {:?} declares no dependency",
                    field.name
                )));
            }
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            // a dependency that does not exist is a broken declaration,
            // not a null: the compute would silently return the wrong
            // value for every record
            let mut plain: Vec<&str> = Vec::new();
            let mut through_lines: Vec<&str> = Vec::new();
            for name in &compute.depends {
                match name.split_once('.') {
                    // `order_line.price_subtotal`: the values of a field
                    // on the records the x2many points at
                    Some((head, _)) => {
                        if model.field(head).is_none() {
                            return Err(RusdooError::Validation(format!(
                                "computed field {:?} depends on unknown field {head:?}",
                                field.name
                            )));
                        }
                        through_lines.push(name);
                    }
                    None => {
                        if model.field(name).is_none() && name != "id" {
                            return Err(RusdooError::Validation(format!(
                                "computed field {:?} depends on unknown field {name:?}",
                                field.name
                            )));
                        }
                        plain.push(name);
                    }
                }
            }
            let mut rows = self
                .read_conn_deep(
                    &mut *conn,
                    model_name,
                    ids,
                    &plain,
                    crate::context::DEFAULT_LANG,
                    depth,
                )
                .await?;
            for path in through_lines {
                let gathered = self
                    .read_over_x2many(&mut *conn, model_name, ids, path, &field.name, depth + 1)
                    .await?;
                for row in &mut rows {
                    let Some(id) = row.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    row.insert(
                        path.to_string(),
                        gathered.get(&id).cloned().unwrap_or_else(|| json!([])),
                    );
                }
            }
            let mut computed = HashMap::new();
            for row in rows {
                let Some(id) = row.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                // `?` and not a skip: a compute that refuses is a compute
                // that could not answer, and a read that quietly dropped
                // the record would answer the caller with a hole
                computed.insert(id, compute.func.call(&row)?);
            }
            Ok(computed)
        })
    }

    /// Resolve display names for a set of comodel ids (Odoo's name_get):
    /// the comodel's `name`/`display_name` field, or the id when neither
    /// exists. A single flat query, no per-record round-trips.
    pub async fn display_names(
        &self,
        pool: &PgPool,
        comodel: &str,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, RusdooError> {
        let mut conn = pool.acquire().await.map_err(db_err)?;
        self.display_names_conn(&mut conn, comodel, ids).await
    }

    /// [`Registry::display_names`] on a caller's connection.
    async fn display_names_conn(
        &self,
        conn: &mut PgConnection,
        comodel: &str,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>, RusdooError> {
        self.display_names_lang(conn, comodel, ids, crate::context::DEFAULT_LANG)
            .await
    }

    /// The `[id, "name"]` pair a many2one is read as, in `lang`.
    ///
    /// The name of a record is exactly the kind of thing that gets
    /// translated — a product, a category, a country — so this reads
    /// through the same fallback the field itself does. Reading the raw
    /// column would hand the client the whole language map.
    async fn display_names_lang(
        &self,
        conn: &mut PgConnection,
        comodel: &str,
        ids: &[i64],
        lang: &str,
    ) -> Result<HashMap<i64, String>, RusdooError> {
        // an unregistered comodel (e.g. reading create_uid in a registry
        // without res.users) degrades to id-only display, like a missing
        // name field — never a hard failure of the whole read
        let Some(model) = self.get(comodel) else {
            return Ok(ids.iter().map(|id| (*id, id.to_string())).collect());
        };
        let rec_name = if model.field("name").is_some() {
            "name"
        } else if model.field("display_name").is_some() {
            "display_name"
        } else {
            return Ok(ids.iter().map(|id| (*id, id.to_string())).collect());
        };
        let placeholders: Vec<String> = (1..=ids.len()).map(|n| format!("${n}")).collect();
        let column = quote_ident(rec_name)?;
        let selected = if model.field(rec_name).is_some_and(|f| f.translate) {
            crate::sql::translated_read(&column, lang)?
        } else {
            column
        };
        let sql = format!(
            r#"SELECT "id", {selected} FROM {} WHERE "id" IN ({})"#,
            quote_ident(&model.meta.table)?,
            placeholders.join(", ")
        );
        let mut query = sqlx::query_as::<_, (i32, Option<String>)>(&sql);
        for id in ids {
            query = query.bind(*id as i32);
        }
        let rows = query.fetch_all(&mut *conn).await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, name)| (i64::from(id), name.unwrap_or_default()))
            .collect())
    }

    /// Fetch the related ids of an x2many field for each owner id:
    /// through the relation table (many2many) or the inverse column
    /// (one2many). Owners with no relations are simply absent from the map.
    async fn read_x2many(
        &self,
        conn: &mut PgConnection,
        field: &Field,
        ids: &[i64],
    ) -> Result<HashMap<i64, Vec<i64>>, RusdooError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: Vec<String> = (1..=ids.len()).map(|n| format!("${n}")).collect();
        let in_list = placeholders.join(", ");
        let sql = match &field.ty {
            FieldType::Many2many {
                relation,
                column1,
                column2,
                ..
            } => format!(
                r#"SELECT {}, {} FROM {} WHERE {} IN ({in_list})"#,
                quote_ident(column1)?,
                quote_ident(column2)?,
                quote_ident(relation)?,
                quote_ident(column1)?,
            ),
            FieldType::One2many { comodel, inverse } => {
                let co = self.get(comodel).ok_or_else(|| {
                    RusdooError::Validation(format!("comodel not registered: {comodel}"))
                })?;
                // the lines come back in the comodel's `_order`: a form
                // whose lines move between two reloads of the same record
                // is a form the user stops trusting
                format!(
                    r#"SELECT {}, "id" FROM {} WHERE {} IN ({in_list}) ORDER BY {}"#,
                    quote_ident(inverse)?,
                    quote_ident(&co.meta.table)?,
                    quote_ident(inverse)?,
                    co.order_sql()?,
                )
            }
            other => {
                return Err(RusdooError::Validation(format!(
                    "read_x2many called on non-x2many field type {other:?}"
                )))
            }
        };
        let mut query = sqlx::query_as::<_, (i32, i32)>(&sql);
        for id in ids {
            query = query.bind(*id as i32);
        }
        let pairs = query.fetch_all(&mut *conn).await.map_err(db_err)?;
        let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
        for (owner, related) in pairs {
            map.entry(i64::from(owner))
                .or_default()
                .push(i64::from(related));
        }
        Ok(map)
    }

    /// Write with `_inherits` delegation, atomically. Delegated fields
    /// update the parents linked at call time — before any link
    /// reassignment in the same call; local fields update last.
    pub async fn write(
        &self,
        pool: &PgPool,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        self.write_as(pool, SUPERUSER_ID, model_name, ids, values)
            .await
    }

    /// Write attributed to `uid`: `write_uid`/`write_date` record the acting
    /// user on the record and on any `_inherits` parent it touches.
    pub async fn write_as(
        &self,
        pool: &PgPool,
        uid: i64,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        self.write_as_lang(pool, uid, model_name, ids, values, crate::context::DEFAULT_LANG)
            .await
    }

    /// [`Registry::write_as`] writing translated values into `lang`.
    ///
    /// Editing a product's name while the client is in Portuguese sets
    /// the Portuguese value and leaves every other language alone.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_as_lang(
        &self,
        pool: &PgPool,
        uid: i64,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
        lang: &str,
    ) -> Result<(), RusdooError> {
        let mut tx = pool.begin().await.map_err(db_err)?;
        self.write_in_lang(&mut tx, uid, model_name, ids, values, lang)
            .await?;
        tx.commit().await.map_err(db_err)
    }

    async fn write_in(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        self.write_in_lang(tx, uid, model_name, ids, values, crate::context::DEFAULT_LANG)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_in_lang(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
        lang: &str,
    ) -> Result<(), RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let mut local: Vec<(&str, Value)> = Vec::new();
        let mut x2many_local: Vec<(Field, Vec<Value>)> = Vec::new();
        let mut delegated: HashMap<DelegationChain, Vec<(&str, Value)>> = HashMap::new();
        // what the write touches, for the stored computes that watch it
        let changed: Vec<&str> = values.iter().map(|(name, _)| *name).collect();
        for (name, value) in values {
            match model.field(name).map(|f| &f.ty) {
                Some(FieldType::Many2many { .. } | FieldType::One2many { .. }) => {
                    let field = model.field(name).expect("just matched").clone();
                    x2many_local.push((field, parse_commands(&value)?));
                }
                Some(_) => local.push((name, value)),
                None => {
                    if let Some(chain) = self.delegation_chain(model, name, 0) {
                        delegated.entry(chain).or_default().push((name, value));
                    } else {
                        return Err(RusdooError::Validation(format!(
                            "unknown field on {model_name}: {name:?}"
                        )));
                    }
                }
            }
        }
        // delegated first: the link subqueries must see the links as they
        // were when the call was made
        for (chain, chain_values) in delegated {
            let owner_name = &chain.last().expect("chain is never empty").1;
            let owner = self.get(owner_name).ok_or_else(|| {
                RusdooError::Validation(format!("_inherits parent not registered: {owner_name}"))
            })?;
            let mut values: Vec<(&str, Value)> = Vec::new();
            for (name, value) in chain_values {
                // a readonly field is client-write-protected on the
                // delegated path too, not only the local one
                if owner.field(name).is_some_and(|f| f.readonly) {
                    return Err(RusdooError::Validation(format!(
                        "field is readonly: {name:?}"
                    )));
                }
                match owner.field(name) {
                    Some(field) if field.stored => {}
                    Some(field)
                        if matches!(
                            field.ty,
                            FieldType::Many2many { .. } | FieldType::One2many { .. }
                        ) =>
                    {
                        return Err(RusdooError::Validation(format!(
                            "writing x2many field {name:?} through _inherits delegation                              is not yet supported"
                        )))
                    }
                    _ => {
                        return Err(RusdooError::Validation(format!(
                            "field is not stored: {name:?}"
                        )))
                    }
                }
                values.push((name, value));
            }
            // Which rows of the owner these ids delegate to. Resolved
            // before writing, because the links must be read as they were
            // when the call was made.
            let mut params: Vec<Value> = Vec::new();
            let id_list = bind_ids(ids, &mut params)?;
            let mut target = format!(r#""id" IN ({id_list})"#);
            let mut from_table = model.meta.table.clone();
            for (link, parent_name) in &chain {
                target = format!(
                    r#""id" IN (SELECT {} FROM {} WHERE {target})"#,
                    quote_ident(link)?,
                    quote_ident(&from_table)?
                );
                from_table = self
                    .get(parent_name)
                    .expect("validated above")
                    .meta
                    .table
                    .clone();
            }
            let sql = format!(
                r#"SELECT "id" FROM {} WHERE {target}"#,
                quote_ident(&owner.meta.table)?
            );
            let rows = build_query(&sql, &params)?
                .fetch_all(&mut **tx)
                .await
                .map_err(db_err)?;
            let owner_ids: Vec<i64> = rows.iter().map(row_id).collect::<Result<_, _>>()?;
            // and then the owner writes them itself. Building the UPDATE
            // here instead is how a translatable column (jsonb) came to be
            // bound as text, and a delegated write of a name was refused
            // by Postgres: the owner's own write path is the only one that
            // knows what its columns are.
            if !owner_ids.is_empty() {
                owner
                    .write_conn_lang(&mut *tx, uid, &owner_ids, values, lang)
                    .await?;
            }
        }
        if !local.is_empty() {
            model.write_conn_lang(&mut *tx, uid, ids, local, lang).await?;
        }
        for (field, commands) in &x2many_local {
            for id in ids {
                self.apply_x2many(&mut *tx, uid, field, *id, commands)
                    .await?;
            }
        }
        self.check_constraints(&mut *tx, model_name, ids, Some(&changed))
            .await?;
        self.recompute_stored(&mut *tx, model_name, ids, Some(&changed))
            .await?;
        Ok(())
    }
}

/// Normalize an x2many field value into a list of command tuples.
/// Accepts `[[code, id, values], ...]` (commands) or `[id, id, ...]`
/// (a bare id list, treated as `set`).
pub(crate) fn parse_commands(value: &Value) -> Result<Vec<Value>, RusdooError> {
    let arr = value.as_array().ok_or_else(|| {
        RusdooError::Validation("x2many value must be a list of commands or ids".into())
    })?;
    if arr.iter().all(Value::is_array) {
        Ok(arr.clone())
    } else if arr.iter().all(|v| v.as_i64().is_some()) {
        // reject a bare command tuple (e.g. [4, id, 0]) that lost its
        // outer list: silently reading it as set([...]) would wipe links
        let looks_like_bare_command = arr.len() == 3 && matches!(arr[0].as_i64(), Some(0..=6));
        if looks_like_bare_command {
            return Err(RusdooError::Validation(
                "x2many value looks like a bare command tuple; wrap it in a list,                  e.g. [[4, id, 0]] not [4, id, 0]"
                    .into(),
            ));
        }
        Ok(vec![Value::Array(vec![
            Value::from(6),
            Value::from(0),
            Value::Array(arr.clone()),
        ])])
    } else {
        Err(RusdooError::Validation(
            "x2many value must be command tuples or a plain id list".into(),
        ))
    }
}

impl Registry {
    fn split_x2many<'a>(
        &self,
        model: &Model,
        values: Vec<(&'a str, Value)>,
    ) -> Result<SplitValues<'a>, RusdooError> {
        let mut scalars = Vec::new();
        let mut x2many = Vec::new();
        for (name, value) in values {
            match model.field(name).map(|f| &f.ty) {
                Some(FieldType::Many2many { .. } | FieldType::One2many { .. }) => {
                    let field = model.field(name).expect("just matched").clone();
                    x2many.push((field, parse_commands(&value)?));
                }
                _ => scalars.push((name, value)),
            }
        }
        Ok((scalars, x2many))
    }

    async fn apply_x2many_all(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        x2many: &X2manyCommands,
        owner: i64,
    ) -> Result<(), RusdooError> {
        for (field, commands) in x2many {
            self.apply_x2many(tx, uid, field, owner, commands).await?;
        }
        Ok(())
    }

    /// Apply x2many write commands (Odoo's `(code, id, values)` tuples) to
    /// the relation table (many2many) or inverse column (one2many).
    /// A one2many reassignment writes the child row, so it stamps the
    /// child's `write_uid`/`write_date` with `uid` (LOG_ACCESS), like a
    /// direct write; many2many relation rows have no audit columns.
    async fn apply_x2many(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        field: &Field,
        owner: i64,
        commands: &[Value],
    ) -> Result<(), RusdooError> {
        let want_id = command_id;
        for cmd in commands {
            let arr = cmd
                .as_array()
                .ok_or_else(|| RusdooError::Validation("x2many command must be a tuple".into()))?;
            let code = arr.first().and_then(Value::as_i64).ok_or_else(|| {
                RusdooError::Validation("x2many command needs a numeric code".into())
            })?;
            // CREATE/UPDATE/DELETE act on the comodel record itself, so both
            // x2many kinds share them — only how the link is established
            // (one2many inverse column vs relation row) differs
            match code {
                0 => {
                    self.command_create(&mut *tx, uid, field, owner, arr)
                        .await?;
                    continue;
                }
                1 => {
                    self.command_update(&mut *tx, uid, field, owner, arr)
                        .await?;
                    continue;
                }
                2 => {
                    self.command_delete(&mut *tx, field, owner, arr).await?;
                    continue;
                }
                _ => {}
            }
            match &field.ty {
                FieldType::Many2many {
                    relation,
                    column1,
                    column2,
                    ..
                } => {
                    let rel = quote_ident(relation)?;
                    let c1 = quote_ident(column1)?;
                    let c2 = quote_ident(column2)?;
                    let link_sql = format!(
                        "INSERT INTO {rel} ({c1}, {c2}) VALUES ($1, $2) ON CONFLICT DO NOTHING"
                    );
                    match code {
                        4 => {
                            let rid = want_id(arr)?;
                            sqlx::query(&link_sql)
                                .bind(owner as i32)
                                .bind(rid as i32)
                                .execute(&mut **tx)
                                .await
                                .map_err(db_err)?;
                        }
                        3 => {
                            let rid = want_id(arr)?;
                            sqlx::query(&format!(
                                "DELETE FROM {rel} WHERE {c1} = $1 AND {c2} = $2"
                            ))
                            .bind(owner as i32)
                            .bind(rid as i32)
                            .execute(&mut **tx)
                            .await
                            .map_err(db_err)?;
                        }
                        5 => {
                            sqlx::query(&format!("DELETE FROM {rel} WHERE {c1} = $1"))
                                .bind(owner as i32)
                                .execute(&mut **tx)
                                .await
                                .map_err(db_err)?;
                        }
                        6 => {
                            sqlx::query(&format!("DELETE FROM {rel} WHERE {c1} = $1"))
                                .bind(owner as i32)
                                .execute(&mut **tx)
                                .await
                                .map_err(db_err)?;
                            for v in arr.get(2).and_then(Value::as_array).into_iter().flatten() {
                                let rid = v.as_i64().ok_or_else(|| {
                                    RusdooError::Validation("set() ids must be integers".into())
                                })?;
                                sqlx::query(&link_sql)
                                    .bind(owner as i32)
                                    .bind(rid as i32)
                                    .execute(&mut **tx)
                                    .await
                                    .map_err(db_err)?;
                            }
                        }
                        other => {
                            return Err(RusdooError::Validation(format!(
                                "many2many command {other} not yet supported"
                            )))
                        }
                    }
                }
                FieldType::One2many { comodel, inverse } => {
                    let co = self.get(comodel).ok_or_else(|| {
                        RusdooError::Validation(format!("comodel not registered: {comodel}"))
                    })?;
                    let table = quote_ident(&co.meta.table)?;
                    let inv = quote_ident(inverse)?;
                    // one2many reassignment writes the child row, so stamp
                    // its LOG_ACCESS columns (uid bound as the last param)
                    match code {
                        4 => {
                            let rid = want_id(arr)?;
                            sqlx::query(&format!(
                                r#"UPDATE {table} SET {inv} = $1, "write_uid" = $3,
                                   "write_date" = CURRENT_TIMESTAMP WHERE "id" = $2"#
                            ))
                            .bind(owner as i32)
                            .bind(rid as i32)
                            .bind(uid as i32)
                            .execute(&mut **tx)
                            .await
                            .map_err(db_err)?;
                        }
                        3 => {
                            let rid = want_id(arr)?;
                            // scope to this owner: never sever another
                            // record's link (a cross-record corruption)
                            sqlx::query(&format!(
                                r#"UPDATE {table} SET {inv} = NULL, "write_uid" = $3,
                                   "write_date" = CURRENT_TIMESTAMP WHERE "id" = $1 AND {inv} = $2"#
                            ))
                            .bind(rid as i32)
                            .bind(owner as i32)
                            .bind(uid as i32)
                            .execute(&mut **tx)
                            .await
                            .map_err(db_err)?;
                        }
                        5 => {
                            sqlx::query(&format!(
                                r#"UPDATE {table} SET {inv} = NULL, "write_uid" = $2,
                                   "write_date" = CURRENT_TIMESTAMP WHERE {inv} = $1"#
                            ))
                            .bind(owner as i32)
                            .bind(uid as i32)
                            .execute(&mut **tx)
                            .await
                            .map_err(db_err)?;
                        }
                        6 => {
                            sqlx::query(&format!(
                                r#"UPDATE {table} SET {inv} = NULL, "write_uid" = $2,
                                   "write_date" = CURRENT_TIMESTAMP WHERE {inv} = $1"#
                            ))
                            .bind(owner as i32)
                            .bind(uid as i32)
                            .execute(&mut **tx)
                            .await
                            .map_err(db_err)?;
                            for v in arr.get(2).and_then(Value::as_array).into_iter().flatten() {
                                let rid = v.as_i64().ok_or_else(|| {
                                    RusdooError::Validation("set() ids must be integers".into())
                                })?;
                                sqlx::query(&format!(
                                    r#"UPDATE {table} SET {inv} = $1, "write_uid" = $3,
                                       "write_date" = CURRENT_TIMESTAMP WHERE "id" = $2"#
                                ))
                                .bind(owner as i32)
                                .bind(rid as i32)
                                .bind(uid as i32)
                                .execute(&mut **tx)
                                .await
                                .map_err(db_err)?;
                            }
                        }
                        other => {
                            return Err(RusdooError::Validation(format!(
                                "one2many command {other} not yet supported"
                            )))
                        }
                    }
                }
                other => {
                    return Err(RusdooError::Validation(format!(
                        "apply_x2many on non-x2many field type {other:?}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// `Command.CREATE` (`[0, 0, {values}]`): create a record in the
    /// comodel and link it to `owner`. The link itself is framework-owned:
    /// an inverse supplied in the values would attach the new record to
    /// someone else, so the owner always wins over it.
    async fn command_create(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        field: &Field,
        owner: i64,
        arr: &[Value],
    ) -> Result<(), RusdooError> {
        let values = command_values(arr)?;
        match &field.ty {
            FieldType::One2many { comodel, inverse } => {
                let mut values: Vec<(&str, Value)> = values
                    .into_iter()
                    .filter(|(name, _)| *name != inverse.as_str())
                    .collect();
                values.push((inverse.as_str(), Value::from(owner)));
                self.create_in(&mut *tx, uid, comodel, values).await?;
            }
            FieldType::Many2many {
                comodel,
                relation,
                column1,
                column2,
            } => {
                let id = self.create_in(&mut *tx, uid, comodel, values).await?;
                let sql = format!(
                    "INSERT INTO {} ({}, {}) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    quote_ident(relation)?,
                    quote_ident(column1)?,
                    quote_ident(column2)?
                );
                sqlx::query(&sql)
                    .bind(owner as i32)
                    .bind(id as i32)
                    .execute(&mut **tx)
                    .await
                    .map_err(db_err)?;
            }
            other => {
                return Err(RusdooError::Validation(format!(
                    "x2many create on non-x2many field type {other:?}"
                )))
            }
        }
        Ok(())
    }

    /// `Command.UPDATE` (`[1, id, {values}]`): write onto a linked record.
    /// Scoped to `owner` — with no record rules yet, an unscoped update
    /// would turn any relational field into a lever for writing arbitrary
    /// comodel rows, the same corruption the unlink path already refuses.
    ///
    /// Type-erased on purpose: the write it delegates to reaches back here
    /// through `apply_x2many`, and only a declared `Send` bound (rather
    /// than an inferred one) closes that cycle for the compiler.
    fn command_update<'a>(
        &'a self,
        tx: &'a mut Transaction<'static, Postgres>,
        uid: i64,
        field: &'a Field,
        owner: i64,
        arr: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<(), RusdooError>> + Send + 'a>> {
        Box::pin(async move {
            let id = command_id(arr)?;
            let values = command_values(arr)?;
            self.ensure_linked(&mut *tx, field, owner, id).await?;
            if values.is_empty() {
                // Odoo's write({}) is a no-op, not even a LOG_ACCESS stamp
                return Ok(());
            }
            let comodel = comodel_of(field)?;
            self.write_in(&mut *tx, uid, comodel, &[id], values).await
        })
    }

    /// `Command.DELETE` (`[2, id, 0]`): unlink the record and delete it
    /// from the comodel, scoped to `owner` like UPDATE. Relation rows go
    /// first: they carry no foreign key, so deleting the record alone
    /// would leave dangling links behind.
    async fn command_delete(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        field: &Field,
        owner: i64,
        arr: &[Value],
    ) -> Result<(), RusdooError> {
        let id = command_id(arr)?;
        self.ensure_linked(&mut *tx, field, owner, id).await?;
        if let FieldType::Many2many {
            relation, column2, ..
        } = &field.ty
        {
            // every link to the deleted record, not just this owner's
            sqlx::query(&format!(
                "DELETE FROM {} WHERE {} = $1",
                quote_ident(relation)?,
                quote_ident(column2)?
            ))
            .bind(id as i32)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        }
        let comodel = comodel_of(field)?;
        let model = self
            .get(comodel)
            .ok_or_else(|| RusdooError::Validation(format!("comodel not registered: {comodel}")))?;
        let (sql, params) = model.delete_sql(&[id])?;
        build_query(&sql, &params)?
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Fail unless `id` is currently linked to `owner` through `field`:
    /// the one2many child's inverse column points at it, or the relation
    /// table carries the pair.
    async fn ensure_linked(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        field: &Field,
        owner: i64,
        id: i64,
    ) -> Result<(), RusdooError> {
        let sql = match &field.ty {
            FieldType::One2many { comodel, inverse } => {
                let model = self.get(comodel).ok_or_else(|| {
                    RusdooError::Validation(format!("comodel not registered: {comodel}"))
                })?;
                format!(
                    r#"SELECT 1 FROM {} WHERE "id" = $1 AND {} = $2"#,
                    quote_ident(&model.meta.table)?,
                    quote_ident(inverse)?
                )
            }
            FieldType::Many2many {
                relation,
                column1,
                column2,
                ..
            } => format!(
                "SELECT 1 FROM {} WHERE {} = $2 AND {} = $1",
                quote_ident(relation)?,
                quote_ident(column1)?,
                quote_ident(column2)?
            ),
            other => {
                return Err(RusdooError::Validation(format!(
                    "x2many command on non-x2many field type {other:?}"
                )))
            }
        };
        let linked = sqlx::query(&sql)
            .bind(id as i32)
            .bind(owner as i32)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_err)?
            .is_some();
        if linked {
            Ok(())
        } else {
            Err(RusdooError::Validation(format!(
                "record {id} is not linked to record {owner} through field {:?}",
                field.name
            )))
        }
    }
}

/// The record id an x2many command targets (its second slot).
fn command_id(arr: &[Value]) -> Result<i64, RusdooError> {
    arr.get(1)
        .and_then(Value::as_i64)
        .ok_or_else(|| RusdooError::Validation("x2many command needs a record id".into()))
}

/// The values a CREATE/UPDATE command carries (its third slot). A missing
/// or non-object slot is a malformed command — very likely a link tuple
/// (`[4, id, 0]`) that lost its code, so it is refused rather than read as
/// "no values".
fn command_values(arr: &[Value]) -> Result<Vec<(&str, Value)>, RusdooError> {
    match arr.get(2) {
        Some(Value::Object(map)) => Ok(map.iter().map(|(k, v)| (k.as_str(), v.clone())).collect()),
        _ => Err(RusdooError::Validation(
            "x2many create/update command needs a values object in its third slot".into(),
        )),
    }
}

/// The comodel an x2many field points at.
fn comodel_of(field: &Field) -> Result<&str, RusdooError> {
    match &field.ty {
        FieldType::One2many { comodel, .. } | FieldType::Many2many { comodel, .. } => Ok(comodel),
        other => Err(RusdooError::Validation(format!(
            "x2many command on non-x2many field type {other:?}"
        ))),
    }
}
