//! rodoo — the server binary, port of `odoo-bin`.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("rodoo 0.1.0 — Rust port of Odoo (bootstrap)");
    Ok(())
}
