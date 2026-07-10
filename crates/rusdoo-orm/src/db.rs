//! Execution layer: runs the SQL built by `crud`/`ddl` on a PgPool.
//!
//! Values travel as `serde_json::Value` (the JSON-RPC wire format of the
//! Odoo web client), bound as typed PostgreSQL parameters here.

use crate::crud::{bind_ids, SearchOptions};
use crate::ddl::{create_relation_table_sql, create_table_sql};
use crate::domain::Domain;
use crate::fields::{Field, FieldType};
use crate::model::Model;
use crate::registry::Registry;
use crate::sql::{bind, quote_ident};
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use sqlx::postgres::{PgArguments, PgPool, PgPoolOptions, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

const MAX_POOL_CONNECTIONS: u32 = 5;

pub async fn connect(url: &str) -> Result<PgPool, RusdooError> {
    PgPoolOptions::new()
        .max_connections(MAX_POOL_CONNECTIONS)
        .connect(url)
        .await
        .map_err(db_err)
}

/// Pool that only connects on first use — for tests and tooling.
pub fn lazy_pool(url: &str) -> Result<PgPool, RusdooError> {
    PgPoolOptions::new()
        .max_connections(MAX_POOL_CONNECTIONS)
        .connect_lazy(url)
        .map_err(db_err)
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
        self.create_conn(&mut conn, values).await
    }

    pub(crate) async fn create_conn(
        &self,
        conn: &mut PgConnection,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        let (sql, params) = self.insert_sql(values)?;
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
        self.write_conn(&mut conn, ids, values).await
    }

    pub(crate) async fn write_conn(
        &self,
        conn: &mut PgConnection,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<u64, RusdooError> {
        let (sql, params) = self.update_sql(ids, values)?;
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
        let mut tx = pool.begin().await.map_err(db_err)?;
        let id = self.create_in(&mut tx, model_name, values).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(id)
    }

    fn create_in<'a>(
        &'a self,
        tx: &'a mut Transaction<'static, Postgres>,
        model_name: &'a str,
        values: Vec<(&'a str, Value)>,
    ) -> Pin<Box<dyn Future<Output = Result<i64, RusdooError>> + Send + 'a>> {
        Box::pin(async move {
            let model = self
                .get(model_name)
                .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
            if model.meta.inherits.is_empty() {
                return model.create_conn(&mut *tx, values).await;
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
                        self.write_in(&mut *tx, parent_name, &[parent_id], parent_values)
                            .await?;
                    }
                    continue;
                }
                let parent_id = self.create_in(&mut *tx, parent_name, parent_values).await?;
                local.push((link.as_str(), Value::from(parent_id)));
            }
            model.create_conn(&mut *tx, local).await
        })
    }

    /// Like [`Registry::create`], inside a caller-managed transaction.
    pub async fn create_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        self.create_in(tx, model_name, values).await
    }

    /// Like [`Registry::write`], inside a caller-managed transaction.
    pub async fn write_tx(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        self.write_in(tx, model_name, ids, values).await
    }

    /// Read with `_inherits` delegation (LEFT JOINs on the link fields).
    pub async fn read(
        &self,
        pool: &PgPool,
        model_name: &str,
        ids: &[i64],
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let (sql, params, resolved) = self.read_query(model_name, ids, fields)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
        rows.iter()
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
            .collect()
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
        let mut tx = pool.begin().await.map_err(db_err)?;
        self.write_in(&mut tx, model_name, ids, values).await?;
        tx.commit().await.map_err(db_err)
    }

    async fn write_in(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        model_name: &str,
        ids: &[i64],
        values: Vec<(&str, Value)>,
    ) -> Result<(), RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let mut local: Vec<(&str, Value)> = Vec::new();
        let mut delegated: HashMap<DelegationChain, Vec<(&str, Value)>> = HashMap::new();
        for (name, value) in values {
            if model.field(name).is_some() {
                local.push((name, value));
            } else if let Some(chain) = self.delegation_chain(model, name, 0) {
                delegated.entry(chain).or_default().push((name, value));
            } else {
                return Err(RusdooError::Validation(format!(
                    "unknown field on {model_name}: {name:?}"
                )));
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
                match owner.field(name) {
                    Some(field) if field.stored => {}
                    _ => {
                        return Err(RusdooError::Validation(format!(
                            "field is not stored: {name:?}"
                        )))
                    }
                }
                let placeholder = bind(&mut params, value);
                assignments.push(format!("{} = {placeholder}", quote_ident(name)?));
            }
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
            model.write_conn(&mut *tx, ids, local).await?;
        }
        Ok(())
    }
}
