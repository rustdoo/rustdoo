//! rodoo-modules — port of `odoo/modules/`: manifest parsing
//! (`__manifest__.py`), dependency graph, module loading order.

/// Mirrors the keys of an addon `__manifest__.py`.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub depends: Vec<String>,
    pub auto_install: bool,
    pub installable: bool,
}
