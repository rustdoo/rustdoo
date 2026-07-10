//! Execution layer: runs the SQL built by `crud`/`ddl` on a PgPool.
//!
//! Values travel as `serde_json::Value` (the JSON-RPC wire format of the
//! Odoo web client), bound as typed PostgreSQL parameters here.

use crate::crud::SearchOptions;
use crate::ddl::create_table_sql;
use crate::domain::Domain;
use crate::fields::FieldType;
use crate::model::Model;
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use sqlx::postgres::{PgArguments, PgPool, PgPoolOptions, PgRow};
use sqlx::{Postgres, Row};

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

const MAX_POOL_CONNECTIONS: u32 = 5;

pub async fn connect(url: &str) -> Result<PgPool, RusdooError> {
    PgPoolOptions::new()
        .max_connections(MAX_POOL_CONNECTIONS)
        .connect(url)
        .await
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
        let sql = create_table_sql(self)?;
        sqlx::query(&sql).execute(pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn search(
        &self,
        pool: &PgPool,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Vec<i64>, RusdooError> {
        let (sql, params) = self.search_sql(domain, opts)?;
        let rows = build_query(&sql, &params)?
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_id).collect()
    }

    pub async fn create(
        &self,
        pool: &PgPool,
        values: Vec<(&str, Value)>,
    ) -> Result<i64, RusdooError> {
        let (sql, params) = self.insert_sql(values)?;
        let row = build_query(&sql, &params)?
            .fetch_one(pool)
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
        let (sql, params) = self.update_sql(ids, values)?;
        let done = build_query(&sql, &params)?
            .execute(pool)
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
            let value = match &field.ty {
                FieldType::Boolean => row
                    .try_get::<Option<bool>, _>(*name)
                    .map_err(db_err)?
                    .map(Value::from),
                FieldType::Integer | FieldType::Many2one { .. } => row
                    .try_get::<Option<i32>, _>(*name)
                    .map_err(db_err)?
                    .map(|v| Value::from(i64::from(v))),
                FieldType::Float { digits: None } => row
                    .try_get::<Option<f64>, _>(*name)
                    .map_err(db_err)?
                    .map(Value::from),
                FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Selection(_) => row
                    .try_get::<Option<String>, _>(*name)
                    .map_err(db_err)?
                    .map(Value::from),
                other => {
                    // explicit gap, never a silently-wrong value
                    return Err(RusdooError::Validation(format!(
                        "read not yet supported for field type {other:?}"
                    )));
                }
            };
            record.insert((*name).to_string(), value.unwrap_or(Value::Null));
        }
        Ok(record)
    }
}
