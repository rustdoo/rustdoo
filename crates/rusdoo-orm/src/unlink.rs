//! Deleting a record: the checks that run first, and the references the
//! database has to deal with afterwards.
//!
//! Port of `@api.ondelete` (`odoo/orm/models.py::unlink`) and of the
//! `ondelete=` a many2one declares.
//!
//! The two are not the same job and neither replaces the other. A hook
//! says *this record must not go* in the language of the business — a
//! confirmed order, a posted invoice — and can explain itself. A foreign
//! key says what happens to everything pointing at it, and is enforced
//! against writers the application never sees.

use crate::fields::OnDelete;
use crate::model::Model;
use crate::registry::Registry;
use rusdoo_core::RusdooError;
use sqlx::{PgConnection, PgPool};
use std::future::Future;
use std::pin::Pin;

/// The `_log_access` columns the framework owns and stamps itself.
const AUDIT_COLUMNS: [&str; 2] = ["create_uid", "write_uid"];

fn db_err(error: sqlx::Error) -> RusdooError {
    RusdooError::Database(error.to_string())
}

/// What a pre-delete hook is given: who is deleting, and what.
pub struct UnlinkCtx<'a> {
    pub registry: &'a Registry,
    pub conn: &'a mut PgConnection,
    pub uid: i64,
    pub model: &'a str,
    /// the records about to be deleted — all of them, so a hook can ask
    /// one question of the database instead of one per record
    pub ids: &'a [i64],
}

impl UnlinkCtx<'_> {
    /// The records about to be deleted, read inside the delete's own
    /// transaction.
    ///
    /// A hook asks for what it needs to decide and gets records, not
    /// rows: a module should not have to write SQL — nor know this
    /// model's table name — to say "not while it is confirmed".
    pub async fn read(
        &mut self,
        fields: &[&str],
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, RusdooError> {
        let model = self
            .registry
            .get(self.model)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {}", self.model)))?;
        model.read_in(&mut *self.conn, self.ids, fields).await
    }
}

/// A check that runs before a delete, port of `@api.ondelete`.
///
/// `Ok(())` lets the delete through; an error refuses it and the message
/// is what the user reads.
pub type OnDeleteFn =
    for<'a> fn(UnlinkCtx<'a>) -> Pin<Box<dyn Future<Output = Result<(), RusdooError>> + Send + 'a>>;

/// One registered pre-delete check.
#[derive(Clone, Copy)]
pub struct UnlinkHook {
    pub name: &'static str,
    pub check: OnDeleteFn,
}

impl std::fmt::Debug for UnlinkHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlinkHook")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Delete `ids` of `model` as `uid`, running the model's pre-delete
    /// checks first.
    ///
    /// Checks and delete share one transaction: a check that passed and
    /// a delete that then failed must not leave the two disagreeing, and
    /// a second writer must not slip a reference in between them.
    pub async fn unlink_as(
        &self,
        pool: &PgPool,
        uid: i64,
        model_name: &str,
        ids: &[i64],
    ) -> Result<u64, RusdooError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        let mut tx = pool.begin().await.map_err(db_err)?;
        for hook in model.unlink_hooks() {
            let ctx = UnlinkCtx {
                registry: self,
                conn: &mut tx,
                uid,
                model: model_name,
                ids,
            };
            (hook.check)(ctx).await?;
        }
        let (sql, params) = model.delete_sql(ids)?;
        let done = crate::db::execute_in(&mut tx, &sql, &params, model).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(done)
    }

    /// The foreign keys the models declared, added after every table
    /// exists — a key can only be created once both ends are there.
    ///
    /// Like every other constraint the port adds: skipped when already
    /// present, and reported rather than fatal when the data that is
    /// already stored would not satisfy it.
    pub async fn init_foreign_keys(&self, pool: &PgPool) -> Result<(), RusdooError> {
        for model in self.models() {
            for field in model.fields() {
                // as colunas de auditoria ficam de fora, e não por
                // comodidade: `res.users.create_uid` aponta para
                // `res.users`, então o primeiro usuário do banco teria de
                // existir antes de ser criado. O Odoo resolve isso
                // inserindo o admin com id fixo nos dados de `base`; o
                // port carimba o uid da sessão, que nem sempre é uma
                // linha ainda. O que se perde é pequeno: um `create_uid`
                // pode apontar para um usuário apagado, e a tela mostra
                // um autor em branco.
                if AUDIT_COLUMNS.contains(&field.name.as_str()) {
                    continue;
                }
                let crate::fields::FieldType::Many2one { comodel } = &field.ty else {
                    if field.ondelete.is_some() {
                        return Err(RusdooError::Validation(format!(
                            "{}.{}: ondelete só existe em many2one",
                            model.meta.name, field.name
                        )));
                    }
                    continue;
                };
                let Some(target) = self.get(comodel) else {
                    // um comodelo de outro módulo que não está instalado:
                    // a referência fica sem chave, e o log diz qual
                    tracing::warn!(
                        "{}.{}: o comodelo {comodel} não está registrado, \
                         a referência fica sem chave estrangeira",
                        model.meta.name,
                        field.name
                    );
                    continue;
                };
                let behaviour = field
                    .ondelete
                    .unwrap_or_else(|| default_ondelete(model, field, target));
                add_foreign_key(pool, model, &field.name, &target.meta.table, behaviour).await?;
            }
        }
        Ok(())
    }

    /// Create every registered model's table, then the foreign keys.
    ///
    /// What a boot wants: the loop over `init_table` alone leaves the
    /// references undeclared, because a key needs both tables and the
    /// last model is not registered until the loop ends.
    pub async fn init_tables(&self, pool: &PgPool) -> Result<(), RusdooError> {
        for model in self.models() {
            model.init_table(pool).await?;
        }
        self.init_foreign_keys(pool).await
    }
}

/// Odoo's rule when a many2one declares no `ondelete`
/// (`fields_relational.py`): a required reference refuses the delete, an
/// optional one is emptied. A reference *out of* a wizard is cascaded
/// instead, because a dialog somebody left open must never be the reason
/// a record cannot be deleted.
fn default_ondelete(model: &Model, field: &crate::fields::Field, target: &Model) -> OnDelete {
    if model.is_transient() && !target.is_transient() {
        return if field.required {
            OnDelete::Cascade
        } else {
            OnDelete::SetNull
        };
    }
    if field.required {
        OnDelete::Restrict
    } else {
        OnDelete::SetNull
    }
}

async fn add_foreign_key(
    pool: &PgPool,
    model: &Model,
    column: &str,
    target_table: &str,
    behaviour: OnDelete,
) -> Result<(), RusdooError> {
    let name = format!("{}_{column}_fkey", model.meta.table);
    let existing: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM pg_constraint WHERE conname = $1 AND conrelid = to_regclass($2)",
    )
    .bind(&name)
    .bind(&model.meta.table)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    if existing.is_some() {
        return Ok(());
    }
    let sql = format!(
        r#"ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ("id") ON DELETE {}"#,
        crate::sql::quote_ident(&model.meta.table)?,
        crate::sql::quote_ident(&name)?,
        crate::sql::quote_ident(column)?,
        crate::sql::quote_ident(target_table)?,
        behaviour.sql(),
    );
    if let Err(error) = sqlx::query(&sql).execute(pool).await {
        tracing::warn!(
            "{}: a chave estrangeira de {column:?} não pôde ser criada ({error}) — \
             há linhas apontando para registros que não existem",
            model.meta.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_behaviour_spells_itself_the_way_postgres_reads_it() {
        assert_eq!(OnDelete::SetNull.sql(), "SET NULL");
        assert_eq!(OnDelete::Restrict.sql(), "RESTRICT");
        assert_eq!(OnDelete::Cascade.sql(), "CASCADE");
    }
}
