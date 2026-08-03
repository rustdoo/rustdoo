//! A computed field whose body an addon wrote in Python.
//!
//! The ORM's contract for a compute is narrow and that is what makes it
//! bridgeable: the row of everything `@api.depends` named goes in, one
//! value comes out. No connection, no transaction, nothing to await —
//! which is exactly right, because the compute runs inside the read that
//! asked for it and must not go back to the database underneath it.
//!
//! So this crosses the same way method dispatch does, minus the
//! environment: a model, a field and a method by name, and a dict.

use pyo3::prelude::*;
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::DynCompute;
use serde_json::{Map, Value};

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
        // CPython is synchronous and holds one lock. On a runtime worker
        // that has to be announced, or every task sharing the thread
        // stalls behind the GIL; off one there is nothing to announce.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| self.run(row))
            }
            _ => self.run(row),
        }
    }
}

impl PyCompute {
    fn run(&self, row: &Map<String, Value>) -> Result<Value, RusdooError> {
        Python::attach(|py| {
            crate::install_shim(py)?;
            let models = py
                .import("odoo.models")
                .map_err(|error| crate::python_error(py, error))?;
            let row = crate::depythonize(py, &Value::Object(row.clone()))
                .map_err(|error| crate::python_error(py, error))?;
            let answer = models
                .getattr("dispatch_compute")
                .and_then(|dispatch| {
                    dispatch.call1((&self.model, &self.method, &self.field, row))
                })
                .map_err(|error| crate::python_error(py, error))?;
            crate::pythonize(&answer).map_err(|error| crate::python_error(py, error))
        })
    }
}
