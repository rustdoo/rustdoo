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
use serde_json::{Map, Value};
use sqlx::postgres::{PgArguments, PgPool, PgPoolOptions, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

const MAX_POOL_CONNECTIONS: u32 = 5;

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

fn db_err(e: sqlx::Error) -> RusdooError {
    RusdooError::Database(e.to_string())
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
        for field in self.fields() {
            if let Some(rel_sql) = create_relation_table_sql(field)? {
                sqlx::query(&rel_sql)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)
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
        let (sql, params) = self.insert_sql(uid, values)?;
        let row = build_query(&sql, &params)?
            .fetch_one(&mut *conn)
            .await
            .map_err(db_err)?;
        row_id(&row)
    }

    pub async fn read(
        &self,
        pool: &PgPool,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let (sql, params) = self.read_sql(ids, fields)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|row| self.row_to_json(row, fields))
            .collect()
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
        let (sql, params) = self.update_sql(uid, ids, values)?;
        let done = build_query(&sql, &params)?
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    pub async fn unlink(&self, pool: &PgPool, ids: &[i64]) -> Result<u64, RusdooError> {
        let (sql, params) = self.delete_sql(ids)?;
        let done = build_query(&sql, &params)?
            .execute(pool)
            .await
            .map_err(db_err)?;
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

impl Registry {
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
        FieldType::Float { digits: None } => row
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
        let mut tx = pool.begin().await.map_err(db_err)?;
        let id = self.create_in(&mut tx, uid, model_name, values).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(id)
    }

    fn create_in<'a>(
        &'a self,
        tx: &'a mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &'a str,
        values: Vec<(&'a str, Value)>,
    ) -> Pin<Box<dyn Future<Output = Result<i64, RusdooError>> + Send + 'a>> {
        Box::pin(async move {
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            let (values, x2many) = self.split_x2many(model, values)?;
            if model.meta.inherits.is_empty() {
                let id = model.create_conn(&mut *tx, uid, values).await?;
                self.apply_x2many_all(&mut *tx, uid, &x2many, id).await?;
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
                        self.write_in(&mut *tx, uid, parent_name, &[parent_id], parent_values)
                            .await?;
                    }
                    continue;
                }
                let parent_id = self
                    .create_in(&mut *tx, uid, parent_name, parent_values)
                    .await?;
                local.push((link.as_str(), Value::from(parent_id)));
            }
            let id = model.create_conn(&mut *tx, uid, local).await?;
            self.apply_x2many_all(&mut *tx, uid, &x2many, id).await?;
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
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;

        // x2many fields have no column: split them out and fetch their
        // ids from the relation table / inverse column separately
        let mut scalar: Vec<&str> = Vec::new();
        let mut x2many: Vec<&Field> = Vec::new();
        for name in fields {
            match model.field(name).map(|f| &f.ty) {
                Some(FieldType::Many2many { .. } | FieldType::One2many { .. }) => {
                    x2many.push(model.field(name).expect("just matched"));
                }
                _ => scalar.push(name),
            }
        }

        let (sql, params, resolved) = self.read_query(model_name, ids, &scalar)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(pool)
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

        for field in x2many {
            let related = self.read_x2many(pool, field, ids).await?;
            for record in &mut records {
                let owner = record["id"].as_i64().expect("id present");
                let list = related.get(&owner).cloned().unwrap_or_default();
                record.insert(field.name.clone(), Value::from(list));
            }
        }

        // many2one reads as [id, display_name], like Odoo's name_get
        let m2o: Vec<(String, String)> = fields
            .iter()
            .filter_map(|name| match model.field(name).map(|f| &f.ty) {
                Some(FieldType::Many2one { comodel }) => Some((name.to_string(), comodel.clone())),
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
            let names = self.name_map(pool, &comodel, &linked).await?;
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
    }

    /// Resolve display names for a set of comodel ids (Odoo's name_get):
    /// the comodel's `name`/`display_name` field, or the id when neither
    /// exists. A single flat query, no per-record round-trips.
    async fn name_map(
        &self,
        pool: &PgPool,
        comodel: &str,
        ids: &[i64],
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
        let sql = format!(
            r#"SELECT "id", {} FROM {} WHERE "id" IN ({})"#,
            quote_ident(rec_name)?,
            quote_ident(&model.meta.table)?,
            placeholders.join(", ")
        );
        let mut query = sqlx::query_as::<_, (i32, Option<String>)>(&sql);
        for id in ids {
            query = query.bind(*id as i32);
        }
        let rows = query.fetch_all(pool).await.map_err(db_err)?;
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
        pool: &PgPool,
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
                format!(
                    r#"SELECT {}, "id" FROM {} WHERE {} IN ({in_list})"#,
                    quote_ident(inverse)?,
                    quote_ident(&co.meta.table)?,
                    quote_ident(inverse)?,
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
        let pairs = query.fetch_all(pool).await.map_err(db_err)?;
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
        let mut tx = pool.begin().await.map_err(db_err)?;
        self.write_in(&mut tx, uid, model_name, ids, values).await?;
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
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let mut local: Vec<(&str, Value)> = Vec::new();
        let mut x2many_local: Vec<(Field, Vec<Value>)> = Vec::new();
        let mut delegated: HashMap<DelegationChain, Vec<(&str, Value)>> = HashMap::new();
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
            let mut params: Vec<Value> = Vec::new();
            let mut assignments = Vec::new();
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
                let placeholder = bind(&mut params, value);
                assignments.push(format!("{} = {placeholder}", quote_ident(name)?));
            }
            // stamp LOG_ACCESS on the delegated parent too, like a direct write
            let uid_ph = bind(&mut params, Value::from(uid));
            assignments.push(format!(r#""write_uid" = {uid_ph}"#));
            assignments.push(r#""write_date" = CURRENT_TIMESTAMP"#.to_string());
            let id_list = bind_ids(ids, &mut params)?;
            // walk the link columns: child ids -> ... -> owner ids
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
                r#"UPDATE {} SET {} WHERE {target}"#,
                quote_ident(&owner.meta.table)?,
                assignments.join(", ")
            );
            build_query(&sql, &params)?
                .execute(&mut **tx)
                .await
                .map_err(db_err)?;
        }
        if !local.is_empty() {
            model.write_conn(&mut *tx, uid, ids, local).await?;
        }
        for (field, commands) in &x2many_local {
            for id in ids {
                self.apply_x2many(&mut *tx, uid, field, *id, commands)
                    .await?;
            }
        }
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
