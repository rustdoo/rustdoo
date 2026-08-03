//! The two decorators that run against a single row: `@api.depends` on
//! a computed field, and `@api.constrains` on a rule.
//!
//! The ORM's contract for both is narrow, and that is what makes them
//! bridgeable: the row of everything the decorator named goes in, and
//! either a value or a refusal comes out. No connection, no transaction,
//! nothing to await — which is exactly right, because a compute runs
//! inside the read that asked for it and a constraint inside the
//! transaction about to commit, and neither may go back to the database
//! underneath the other.
//!
//! So both cross the same way method dispatch does, minus the
//! environment: names in, a dict in, a value out.

use pyo3::prelude::*;
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::DynCompute;
use rusdoo_orm::model::DynConstraint;
use serde_json::{Map, Value};

/// Run `body` with the GIL, without stalling the runtime that called us.
///
/// CPython is synchronous and holds one lock. On a multi-threaded worker
/// that has to be announced, or every task sharing the thread stalls
/// behind the GIL; anywhere else there is nothing to announce.
fn without_stalling<T>(body: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(body)
        }
        _ => body(),
    }
}

/// Call `odoo.models.<entry>` with `names` and the record's row.
fn call_shim(
    entry: &str,
    names: &[&str],
    row: &Map<String, Value>,
) -> Result<Value, RusdooError> {
    without_stalling(|| {
        Python::attach(|py| {
            crate::install_shim(py)?;
            let models = py
                .import("odoo.models")
                .map_err(|error| crate::python_error(py, error))?;
            let mut args: Vec<Py<PyAny>> = Vec::with_capacity(names.len() + 1);
            for name in names {
                args.push(
                    name.into_pyobject(py)
                        .map_err(|error| crate::python_error(py, error.into()))?
                        .into_any()
                        .unbind(),
                );
            }
            args.push(
                crate::depythonize(py, &Value::Object(row.clone()))
                    .map_err(|error| crate::python_error(py, error))?,
            );
            let args = pyo3::types::PyTuple::new(py, args)
                .map_err(|error| crate::python_error(py, error))?;
            let answer = models
                .getattr(entry)
                .and_then(|dispatch| dispatch.call1(args))
                .map_err(|error| crate::python_error(py, error))?;
            crate::pythonize(&answer).map_err(|error| crate::python_error(py, error))
        })
    })
}

/// One rule an addon declared with `@api.constrains`.
pub struct PyConstraint {
    model: String,
    method: String,
}

impl PyConstraint {
    pub fn new(model: &str, method: &str) -> PyConstraint {
        PyConstraint {
            model: model.to_string(),
            method: method.to_string(),
        }
    }
}

impl DynConstraint for PyConstraint {
    fn check(&self, row: &Map<String, Value>) -> Result<(), RusdooError> {
        call_shim("dispatch_constraint", &[&self.model, &self.method], row).map(|_| ())
    }
}

/// One pre-delete check an addon declared with `@api.ondelete`.
pub struct PyUnlinkHook {
    model: String,
    method: String,
}

impl PyUnlinkHook {
    pub fn new(model: &str, method: &str) -> PyUnlinkHook {
        PyUnlinkHook {
            model: model.to_string(),
            method: method.to_string(),
        }
    }
}

impl rusdoo_orm::unlink::DynUnlinkHook for PyUnlinkHook {
    fn check(&self, row: &Map<String, Value>) -> Result<(), RusdooError> {
        call_shim("dispatch_ondelete", &[&self.model, &self.method], row).map(|_| ())
    }
}

/// One reaction an addon declared with `@api.onchange`.
pub struct PyOnchange {
    model: String,
    method: String,
    /// every field of the model, so a form that did not fill one in
    /// reads it as empty instead of as an error. See `dispatch_onchange`.
    known: Vec<String>,
}

impl PyOnchange {
    pub fn new(model: &str, method: &str, known: Vec<String>) -> PyOnchange {
        PyOnchange {
            model: model.to_string(),
            method: method.to_string(),
            known,
        }
    }
}

impl rusdoo_orm::model::DynOnchange for PyOnchange {
    fn call(&self, values: &Map<String, Value>) -> Result<Map<String, Value>, RusdooError> {
        let known = Value::Array(self.known.iter().cloned().map(Value::String).collect());
        let answer = without_stalling(|| {
            Python::attach(|py| {
                crate::install_shim(py)?;
                let models = py
                    .import("odoo.models")
                    .map_err(|error| crate::python_error(py, error))?;
                let known = crate::depythonize(py, &known)
                    .map_err(|error| crate::python_error(py, error))?;
                let form = crate::depythonize(py, &Value::Object(values.clone()))
                    .map_err(|error| crate::python_error(py, error))?;
                let answer = models
                    .getattr("dispatch_onchange")
                    .and_then(|entry| entry.call1((&self.model, &self.method, known, form)))
                    .map_err(|error| crate::python_error(py, error))?;
                crate::pythonize(&answer).map_err(|error| crate::python_error(py, error))
            })
        })?;
        answer.as_object().cloned().ok_or_else(|| {
            RusdooError::Validation(format!(
                "{}.{} answered something that is not a set of values",
                self.model, self.method
            ))
        })
    }
}

/// One computed field an addon declared, reachable by name.
pub struct PyCompute {
    model: String,
    /// the field being computed — a method may assign several, and this
    /// says which one this declaration is about
    field: String,
    method: String,
}

impl PyCompute {
    pub fn new(model: &str, field: &str, method: &str) -> PyCompute {
        PyCompute {
            model: model.to_string(),
            field: field.to_string(),
            method: method.to_string(),
        }
    }
}

impl DynCompute for PyCompute {
    fn call(&self, row: &Map<String, Value>) -> Result<Value, RusdooError> {
        call_shim(
            "dispatch_compute",
            &[&self.model, &self.method, &self.field],
            row,
        )
    }
}
