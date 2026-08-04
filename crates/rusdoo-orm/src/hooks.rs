//! What a model does *because* it was written, port of Odoo's `create`
//! and `write` overrides.
//!
//! Half of Odoo's behaviour hangs off those two methods. Writing
//! `partner_ids` on a meeting creates the attendees; confirming an order
//! raises the purchase behind it; writing a stage stamps the date it
//! changed. None of that is a constraint — it does not refuse anything —
//! and none of it is a computed field, because it writes *other records*.
//!
//! Until now this port had no such thing, and the two addons that needed
//! it grew explicit methods instead (`action_join_meeting`,
//! `action_generate_purchase_orders`). Everything inside the port called
//! them; nothing outside it did — and Odoo's own client is outside it,
//! writing `partner_ids` straight onto a form and expecting the guest
//! list to be there afterwards.
//!
//! ## What a hook is given, and when
//!
//! The records **already exist** and the values are **already written**:
//! a hook reacts, it does not intercept. It runs inside the same
//! transaction, after the x2many commands of the same call and before the
//! constraints — so a hook may create the very rows a constraint is about
//! to check, and a hook that fails rolls the whole write back.
//!
//! ## Re-entrancy
//!
//! A hook writes, and a write runs hooks. That is not a mistake to
//! forbid — a meeting's hook writing an attendee should let the
//! attendee's own hook run — but a cycle is fatal, so the depth is
//! counted and capped. The error names the model, which is the only way
//! anybody finds the loop.

use crate::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use sqlx::{PgConnection, Postgres, Transaction};
use std::future::Future;
use std::pin::Pin;

/// How deep a hook may set off another before this is called a cycle.
/// Real chains are two or three deep; anything past this is a loop
/// somebody wrote by accident.
pub const MAX_HOOK_DEPTH: usize = 8;

tokio::task_local! {
    /// How many hooks are on the stack of *this* operation. A task-local
    /// and not a field on the registry: the registry is shared by every
    /// request in the process, and this counts one call chain.
    static DEPTH: usize;
}

/// The current depth, zero outside any hook.
pub fn depth() -> usize {
    DEPTH.try_with(|depth| *depth).unwrap_or(0)
}

/// Run `work` one level deeper, refusing past the cap.
pub async fn nested<F, T>(model: &str, work: F) -> Result<T, RusdooError>
where
    F: Future<Output = Result<T, RusdooError>>,
{
    let next = depth() + 1;
    if next > MAX_HOOK_DEPTH {
        return Err(RusdooError::Validation(format!(
            "the create/write hooks of {model} are more than {MAX_HOOK_DEPTH} deep: \
             a hook is setting off the hook that set it off"
        )));
    }
    DEPTH.scope(next, work).await
}

/// What a record was written by, and what it was written with.
pub struct HookCtx<'a> {
    pub registry: &'a Registry,
    /// the transaction the write is in — a hook writes through this, so
    /// what it does lives or dies with the write that caused it
    pub tx: &'a mut Transaction<'static, Postgres>,
    pub uid: i64,
    pub model: &'a str,
    /// the records that were just written, all of them: a hook asks the
    /// database one question instead of one per record
    pub ids: &'a [i64],
    /// the values the caller passed, before the ORM's own defaults —
    /// what a hook reads to know *what changed*, which a re-read of the
    /// record cannot tell it
    pub values: &'a Map<String, Value>,
}

impl HookCtx<'_> {
    /// Whether this call touched `field`, which is the question most
    /// hooks open with: `write` runs for every write, and a hook that
    /// reacts to a field nobody wrote is a hook that runs for nothing.
    pub fn wrote(&self, field: &str) -> bool {
        self.values.contains_key(field)
    }

    /// The value the caller passed for `field`, if any.
    pub fn value(&self, field: &str) -> Option<&Value> {
        self.values.get(field)
    }

    /// The records as they are now, read inside the same transaction —
    /// so a hook sees what it is reacting to, including what the write
    /// just did.
    pub async fn records(
        &mut self,
        fields: &[&str],
    ) -> Result<Vec<Map<String, Value>>, RusdooError> {
        let conn: &mut PgConnection = &mut *self.tx;
        self.registry
            .read_conn(conn, self.model, self.ids, fields)
            .await
    }
}

/// What runs after a create or a write.
pub type HookFn =
    for<'a> fn(HookCtx<'a>) -> Pin<Box<dyn Future<Output = Result<(), RusdooError>> + Send + 'a>>;

/// A hook whose behaviour lives where the compiler cannot see it — the
/// one an addon wrote in Python.
pub trait DynHook: Send + Sync {
    fn run(
        &self,
        model: &str,
        ids: &[i64],
        values: &Map<String, Value>,
    ) -> Result<(), RusdooError>;
}

/// How a hook's body is reached.
#[derive(Clone)]
pub enum HookBody {
    /// compiled into the server, with the transaction to write through
    Native(HookFn),
    /// written somewhere else, and given what happened rather than a
    /// connection to do more with
    Dynamic(std::sync::Arc<dyn DynHook>),
}

/// When a hook wants to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookOn {
    Create,
    Write,
}

/// One registered hook.
#[derive(Clone)]
pub struct Hook {
    pub name: String,
    pub on: HookOn,
    pub body: HookBody,
}

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hook")
            .field("name", &self.name)
            .field("on", &self.on)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Run the hooks a model registered for `on`, inside the write's own
    /// transaction.
    ///
    /// `values` are the caller's, so a hook can tell what changed. A hook
    /// that errors takes the whole write with it, which is the point: a
    /// meeting whose attendees could not be created is not a meeting
    /// anybody should be looking at.
    pub(crate) async fn run_hooks(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        uid: i64,
        model_name: &str,
        ids: &[i64],
        values: &Map<String, Value>,
        on: HookOn,
    ) -> Result<(), RusdooError> {
        if ids.is_empty() {
            return Ok(());
        }
        let Some(model) = self.get(model_name) else {
            return Ok(());
        };
        let hooks: Vec<Hook> = model
            .hooks()
            .iter()
            .filter(|hook| hook.on == on)
            .cloned()
            .collect();
        if hooks.is_empty() {
            return Ok(());
        }
        for hook in hooks {
            match &hook.body {
                HookBody::Native(run) => {
                    let ctx = HookCtx {
                        registry: self,
                        tx: &mut *tx,
                        uid,
                        model: model_name,
                        ids,
                        values,
                    };
                    nested(model_name, run(ctx)).await.map_err(|error| {
                        RusdooError::Validation(format!(
                            "{model_name}: hook {:?}: {error}",
                            hook.name
                        ))
                    })?;
                }
                HookBody::Dynamic(body) => {
                    body.run(model_name, ids, values).map_err(|error| {
                        RusdooError::Validation(format!(
                            "{model_name}: hook {:?}: {error}",
                            hook.name
                        ))
                    })?;
                }
            }
        }
        Ok(())
    }
}

/// The caller's values as a map, for the hooks.
pub(crate) fn as_map(values: &[(&str, Value)]) -> Map<String, Value> {
    values
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_reads_what_the_caller_wrote() {
        let values = as_map(&[("name", Value::from("Ana")), ("active", Value::from(true))]);
        let ctx_wrote = |field: &str| values.contains_key(field);
        assert!(ctx_wrote("name"));
        assert!(!ctx_wrote("email"), "um campo que ninguém escreveu");
    }

    #[tokio::test]
    async fn the_depth_is_counted_and_capped() {
        assert_eq!(depth(), 0, "fora de qualquer hook");
        let inner = nested("x.model", async {
            assert_eq!(depth(), 1);
            nested("x.model", async {
                assert_eq!(depth(), 2);
                Ok::<_, RusdooError>(())
            })
            .await
        })
        .await;
        assert!(inner.is_ok());

        // and a chain that never ends is refused with the model named
        async fn forever(model: &str) -> Result<(), RusdooError> {
            nested(model, async {
                Box::pin(forever(model)).await
            })
            .await
        }
        let error = forever("x.loop").await.expect_err("um ciclo passou");
        assert!(error.to_string().contains("x.loop"), "{error}");
        assert!(error.to_string().contains("deep"), "{error}");
    }
}
