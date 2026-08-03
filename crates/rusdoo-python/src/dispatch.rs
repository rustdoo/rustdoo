//! Calling a Python method from `call_kw`.
//!
//! The half of the bridge that makes an addon's behaviour reachable.
//! Everything before this carried declarations — models, fields — and
//! everything after it carries calls: a button on a form, a scheduled
//! job, one model's method calling another's.
//!
//! What crosses is a name, not an object. Rust asks for
//! `("sale.order", "action_confirm")`, and the Python side finds the
//! class, builds the recordset the method expects as `self`, and calls
//! it. That is all `call_kw` ever knows either, so the bridge does not
//! need to know more.

use crate::env;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::{DynMethod, MethodCtx, MethodFuture, MethodRegistry};
use serde_json::{Map, Value};
use std::sync::Arc;

/// One method an addon wrote, reachable by name.
pub struct PyMethod {
    model: String,
    method: String,
}

impl PyMethod {
    pub fn new(model: &str, method: &str) -> PyMethod {
        PyMethod {
            model: model.to_string(),
            method: method.to_string(),
        }
    }
}

impl DynMethod for PyMethod {
    fn call<'a>(
        &'a self,
        ctx: MethodCtx<'a>,
        _args: &'a [Value],
        kwargs: &'a Map<String, Value>,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let environment = env::Env {
                registry: Arc::clone(&ctx.registry),
                pool: ctx.pool.clone(),
                uid: ctx.uid,
                handle: tokio::runtime::Handle::try_current().map_err(|_| {
                    RusdooError::Validation(
                        "a Python method was called from outside a runtime".into(),
                    )
                })?,
            };
            let model = self.model.clone();
            let method = self.method.clone();
            let ids = ctx.ids.clone();
            // `rest` and not `args`: `call_kw` sends the recordset first,
            // and the recordset is `self` here — passing it again would
            // hand every method one argument too many
            let rest = ctx.rest.clone();
            let kwargs = kwargs.clone();

            // CPython is synchronous and holds one lock. Running it on a
            // worker without telling the runtime would stall every task
            // sharing that thread; told, the runtime moves them first.
            tokio::task::block_in_place(move || {
                env::with_env(environment, || call_python(&model, &method, &ids, &rest, &kwargs))
            })
        })
    }
}

fn call_python(
    model: &str,
    method: &str,
    ids: &[i64],
    rest: &[Value],
    kwargs: &Map<String, Value>,
) -> Result<Value, RusdooError> {
    Python::attach(|py| {
        crate::install_shim(py)?;
        let models = py
            .import("odoo.models")
            .map_err(|error| crate::python_error(py, error))?;
        let args = PyList::empty(py);
        for value in rest {
            let item =
                crate::depythonize(py, value).map_err(|error| crate::python_error(py, error))?;
            args.append(item)
                .map_err(|error| crate::python_error(py, error))?;
        }
        let named = PyDict::new(py);
        for (key, value) in kwargs {
            // `context` is the environment's business, not an argument an
            // addon's method declared — passing it through would make
            // every method that does not name it fail on an unexpected
            // keyword
            if key == "context" {
                continue;
            }
            let item =
                crate::depythonize(py, value).map_err(|error| crate::python_error(py, error))?;
            named
                .set_item(key, item)
                .map_err(|error| crate::python_error(py, error))?;
        }
        let answer = models
            .getattr("dispatch")
            .and_then(|dispatch| {
                dispatch.call1((model, method, ids.to_vec(), args, named))
            })
            .map_err(|error| crate::python_error(py, error))?;
        crate::pythonize(&answer).map_err(|error| crate::python_error(py, error))
    })
}

/// Register every method the loaded Python models declared.
///
/// The access each one requires is `Write`, and that is a decision worth
/// stating: Odoo declares nothing, and checks inside. A method here is
/// reached through `call_kw`, which checks *before* — and a bridge that
/// guessed `Read` would let a reader run something that writes. Guessing
/// the other way only asks for more permission than some methods need,
/// which is a locked door rather than a hole.
pub fn register_methods(
    methods: &mut MethodRegistry,
    declared: &[(String, Vec<String>)],
) -> Result<(), RusdooError> {
    for (model, names) in declared {
        for name in names {
            methods.register_dynamic(
                model,
                name,
                Operation::Write,
                Arc::new(PyMethod::new(model, name)),
            )?;
        }
    }
    Ok(())
}
