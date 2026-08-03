//! Model methods, port of what a business model in Odoo mostly *is*:
//! `action_confirm`, `message_post`, `copy` — behaviour attached to a
//! model and called over `call_kw` exactly like `read` or `write`.
//!
//! A method declares the access it needs. That is not decoration: the
//! dispatch has no way to guess whether `action_confirm` reads or writes,
//! and guessing wrong is either a hole or a locked door.

use crate::access::Operation;
use crate::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// What a method is called with: the records it was called on, and
/// everything it needs to touch the database as the acting user.
pub struct MethodCtx<'a> {
    pub registry: &'a Registry,
    pub pool: &'a PgPool,
    /// the user making the call — a method writes as them, never as root
    pub uid: i64,
    pub model: &'a str,
    /// the ids `call_kw` was given (`self` in Odoo terms); may be empty
    pub ids: Vec<i64>,
    /// the positional arguments after the recordset.
    ///
    /// `call_kw` sends `[ids, arg1, arg2, ...]` for a method called on
    /// records, so the first real argument is not `args[0]` — that is
    /// the recordset. A method reads its arguments here and never has
    /// to know that; the full `args` are still passed for the few
    /// methods that override `create`, where there is no recordset and
    /// `args[0]` is the values.
    pub rest: Vec<Value>,
}

impl<'a> MethodCtx<'a> {
    /// A context for a call on `ids`, with no positional arguments —
    /// what a test that exercises a method directly wants, and what
    /// keeps a new field on this struct from breaking every one of them.
    pub fn new(
        registry: &'a Registry,
        pool: &'a PgPool,
        uid: i64,
        model: &'a str,
        ids: Vec<i64>,
    ) -> MethodCtx<'a> {
        MethodCtx {
            registry,
            pool,
            uid,
            model,
            ids,
            rest: Vec::new(),
        }
    }

    /// The same, with the positional arguments the call carried.
    pub fn with_rest(mut self, rest: Vec<Value>) -> MethodCtx<'a> {
        self.rest = rest;
        self
    }
}

/// The future a method returns, boxed so methods can be plain functions
/// in any crate.
pub type MethodFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + 'a>>;

/// A model method: `(ctx, args, kwargs) -> value`.
pub type MethodFn =
    for<'a> fn(MethodCtx<'a>, &'a [Value], &'a Map<String, Value>) -> MethodFuture<'a>;

/// One registered method and the access it requires.
#[derive(Clone, Copy)]
pub struct Method {
    pub func: MethodFn,
    /// the `ir.model.access` operation checked before it runs
    pub operation: Operation,
}

impl std::fmt::Debug for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Method")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

/// Every method every module attached to a model.
#[derive(Debug, Default, Clone)]
pub struct MethodRegistry {
    methods: HashMap<(String, String), Method>,
}

impl MethodRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach `name` to `model`. Registering the same name twice is an
    /// error: two modules silently overriding each other is how a system
    /// starts doing something nobody wrote.
    pub fn register(
        &mut self,
        model: &str,
        name: &str,
        operation: Operation,
        func: MethodFn,
    ) -> Result<(), RusdooError> {
        let key = (model.to_string(), name.to_string());
        if self.methods.contains_key(&key) {
            return Err(RusdooError::Validation(format!(
                "method {name:?} already registered on {model}"
            )));
        }
        self.methods.insert(key, Method { func, operation });
        Ok(())
    }

    pub fn get(&self, model: &str, name: &str) -> Option<Method> {
        self.methods
            .get(&(model.to_string(), name.to_string()))
            .copied()
    }

    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// The methods of a model, sorted — what a client may be told exists.
    pub fn names_for(&self, model: &str) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .methods
            .keys()
            .filter(|(owner, _)| owner == model)
            .map(|(_, name)| name.as_str())
            .collect();
        names.sort_unstable();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop<'a>(
        _ctx: MethodCtx<'a>,
        _args: &'a [Value],
        _kwargs: &'a Map<String, Value>,
    ) -> MethodFuture<'a> {
        Box::pin(async { Ok(Value::Bool(true)) })
    }

    #[test]
    fn a_method_is_found_by_model_and_name() {
        let mut registry = MethodRegistry::new();
        registry
            .register("sale.order", "action_confirm", Operation::Write, noop)
            .unwrap();
        assert!(registry.get("sale.order", "action_confirm").is_some());
        assert!(registry.get("sale.order", "action_cancel").is_none());
        assert!(registry.get("res.partner", "action_confirm").is_none());
        assert_eq!(registry.names_for("sale.order"), vec!["action_confirm"]);
    }

    #[test]
    fn registering_the_same_method_twice_is_an_error() {
        let mut registry = MethodRegistry::new();
        registry
            .register("sale.order", "action_confirm", Operation::Write, noop)
            .unwrap();
        let error = registry
            .register("sale.order", "action_confirm", Operation::Read, noop)
            .expect_err("a silent override is not allowed");
        assert!(error.to_string().contains("already registered"));
    }
}
