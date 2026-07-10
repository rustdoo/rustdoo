//! Port of `odoo/exceptions.py`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RusdooError {
    /// `odoo.exceptions.UserError`
    #[error("user error: {0}")]
    User(String),
    /// `odoo.exceptions.AccessError`
    #[error("access error: {0}")]
    Access(String),
    /// `odoo.exceptions.ValidationError`
    #[error("validation error: {0}")]
    Validation(String),
    /// `odoo.exceptions.MissingError`
    #[error("missing record: {0}")]
    Missing(String),
}
