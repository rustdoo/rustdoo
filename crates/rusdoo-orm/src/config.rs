//! `ir.config_parameter` — the switches a database is set with.
//!
//! Port of `get_param`/`set_param`
//! (`odoo/addons/base/models/ir_config_parameter.py`). A module that
//! cannot be configured has to pick one behaviour and be wrong for half
//! its users; this is what a module reads to stop doing that.
//!
//! Like Odoo, the read goes straight to SQL instead of through the ORM.
//! It is called from places where the ORM is not necessarily ready — a
//! computed field's dependency, an install still in progress — and a
//! parameter that cannot be read *there* is a parameter nobody can use.

use crate::registry::Registry;
use rusdoo_core::RusdooError;
use sqlx::PgPool;

fn db_err(error: sqlx::Error) -> RusdooError {
    RusdooError::Database(error.to_string())
}

impl Registry {
    /// The value of `key`, or `None` when nobody set it.
    ///
    /// A missing `ir_config_parameter` table answers `None` rather than
    /// an error: a module that asks for a parameter before `base` is
    /// installed should fall back to its default, not fail to boot.
    pub async fn get_param(&self, pool: &PgPool, key: &str) -> Result<Option<String>, RusdooError> {
        let found: Option<Option<String>> =
            sqlx::query_scalar(r#"SELECT "value" FROM "ir_config_parameter" WHERE "key" = $1"#)
                .bind(key)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        Ok(found.flatten())
    }

    /// The value of `key`, or `fallback` when nobody set it.
    pub async fn param_or(&self, pool: &PgPool, key: &str, fallback: &str) -> String {
        self.get_param(pool, key)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Whether `key` is set to something that reads as true.
    ///
    /// Odoo writes these as `"True"`/`"1"` from the settings screen and
    /// as `True` from Python, so all of them count.
    pub async fn param_flag(&self, pool: &PgPool, key: &str, fallback: bool) -> bool {
        match self.get_param(pool, key).await.ok().flatten() {
            Some(value) => matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "t"
            ),
            None => fallback,
        }
    }

    /// Set `key`, answering the value it had before.
    ///
    /// One statement: two requests setting the same key cannot both see
    /// "no such key" and both insert one. That is what the `UNIQUE` on
    /// `key` is for — without it `ON CONFLICT` has nothing to conflict
    /// on, and the upsert quietly becomes a check-then-act again.
    pub async fn set_param(
        &self,
        pool: &PgPool,
        key: &str,
        value: &str,
    ) -> Result<Option<String>, RusdooError> {
        let previous: Option<String> =
            sqlx::query_scalar(r#"SELECT "value" FROM "ir_config_parameter" WHERE "key" = $1"#)
                .bind(key)
                .fetch_optional(pool)
                .await
                .map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "ir_config_parameter" ("key", "value") VALUES ($1, $2)
               ON CONFLICT ("key") DO UPDATE SET "value" = EXCLUDED."value""#,
        )
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
        .map_err(db_err)?;
        Ok(previous)
    }

    /// Forget `key`. Odoo's `set_param(key, False)`.
    pub async fn clear_param(&self, pool: &PgPool, key: &str) -> Result<bool, RusdooError> {
        let done = sqlx::query(r#"DELETE FROM "ir_config_parameter" WHERE "key" = $1"#)
            .bind(key)
            .execute(pool)
            .await
            .map_err(db_err)?;
        Ok(done.rows_affected() > 0)
    }
}
