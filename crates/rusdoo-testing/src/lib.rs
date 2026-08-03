//! rusdoo-testing — port of `odoo.tests.common.TransactionCase`: a test
//! that leaves nothing behind.
//!
//! Odoo runs each test method inside a savepoint and closes the cursor
//! without committing, so the database ends the test exactly as it
//! started. The property that matters is that one — isolation, and no
//! residue — not the savepoint itself.
//!
//! The mechanism here is different, and deliberately so: this ORM takes
//! a pool, not a transaction, so threading one cursor through every call
//! would mean a second API next to the real one. A case gets a schema of
//! its own instead: dropped and recreated when it opens, dropped again
//! when it closes. Two tests can therefore run side by side over the
//! same tables — which is what made three of this port's own tests
//! flaky before it existed.
//!
//! ```ignore
//! let Some(case) = TransactionCase::open("wizard", &["base", "mail", "sale"]).await else {
//!     return; // sem banco de teste configurado
//! };
//! let service = OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods());
//! // ...
//! case.close().await;
//! ```

use rusdoo_core::RusdooError;
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

/// The database the suite runs against, and the variable that names it.
/// A test with no database is skipped, never silently passed.
pub const DATABASE_ENV: &str = "RUSDOO_TEST_DATABASE_URL";

/// What a module does to a registry, and to the method table.
type Extend = fn(&mut Registry) -> Result<(), RusdooError>;
type ExtendMethods = fn(&mut MethodRegistry) -> Result<(), RusdooError>;

/// A code module of the port: its models, its methods, and the sequence
/// rows its addon would have loaded.
struct Module {
    name: &'static str,
    extend: Extend,
    methods: Option<ExtendMethods>,
    /// `(code, prefix)` of each `ir.sequence` the addon ships
    sequences: &'static [(&'static str, &'static str)],
}

fn modules() -> Vec<Module> {
    vec![
        Module {
            name: "base",
            extend: rusdoo_base::extend,
            methods: Some(rusdoo_base::extend_methods),
            sequences: &[],
        },
        Module {
            name: "mail",
            extend: rusdoo_mail::extend,
            methods: None,
            sequences: &[],
        },
        Module {
            name: "product",
            extend: rusdoo_product::extend,
            methods: None,
            sequences: &[],
        },
        Module {
            name: "account",
            extend: rusdoo_account::extend,
            methods: Some(rusdoo_account::extend_methods),
            sequences: &[("account.move", "FAT/")],
        },
        Module {
            name: "stock",
            extend: rusdoo_stock::extend,
            methods: Some(rusdoo_stock::extend_methods),
            sequences: &[("stock.picking.out", "WH/OUT/"), ("stock.picking.in", "WH/IN/")],
        },
        Module {
            name: "purchase",
            extend: rusdoo_purchase::extend,
            methods: Some(rusdoo_purchase::extend_methods),
            sequences: &[("purchase.order", "PO")],
        },
        Module {
            name: "sale",
            extend: rusdoo_sale::extend,
            methods: Some(rusdoo_sale::extend_methods),
            sequences: &[("sale.order", "SO")],
        },
    ]
}

/// One test's database: its own schema, its own registry, and the
/// methods of the modules it asked for.
pub struct TransactionCase {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    schema: String,
}

impl TransactionCase {
    /// Open a case for `name`, with `wanted` modules installed.
    ///
    /// `None` means there is no test database configured — the caller
    /// returns, and the runner shows the test as passed-because-skipped
    /// with a notice on stderr. Every module `wanted` depends on must be
    /// listed too: this installs what it is told, in the order given,
    /// and a missing dependency is a panic here rather than a confusing
    /// error three calls later.
    pub async fn open(name: &str, wanted: &[&str]) -> Option<TransactionCase> {
        let Ok(url) = std::env::var(DATABASE_ENV) else {
            eprintln!("skipped: {DATABASE_ENV} not set");
            return None;
        };
        let schema = format!("rusdoo_case_{name}");
        let pool = pool_for(&url, schema.clone());

        // a schema left behind by a run that panicked is not a reason to
        // fail today's run: it is dropped, not reused
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
            .execute(&pool)
            .await
            .expect("dropping the case schema");
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
            .execute(&pool)
            .await
            .expect("creating the case schema");

        let available = modules();
        let mut registry = Registry::new();
        let mut methods = MethodRegistry::new();
        let mut sequences: Vec<(&str, &str)> = Vec::new();
        for name in wanted {
            let module = available
                .iter()
                .find(|module| module.name == *name)
                .unwrap_or_else(|| panic!("módulo desconhecido no teste: {name}"));
            (module.extend)(&mut registry).expect("registrando os modelos do módulo");
            if let Some(extend_methods) = module.methods {
                extend_methods(&mut methods).expect("registrando os métodos do módulo");
            }
            sequences.extend(module.sequences.iter().copied());
        }

        // every registered model gets its table and its references, like
        // the boot does
        registry
            .init_tables(&pool)
            .await
            .expect("criando as tabelas dos modelos");
        // the superuser exists, like it does after a real boot: every
        // call a case makes is made *as* uid 1, and a record stamped
        // with an author who is not in the table is a reference the
        // database now refuses — rightly
        if registry.get("res.users").is_some() {
            sqlx::query(
                r#"INSERT INTO "res_users" ("id", "login", "name", "active")
                   VALUES (1, 'admin', 'Administrador', true)
                   ON CONFLICT ("id") DO NOTHING"#,
            )
            .execute(&pool)
            .await
            .expect("criando o superusuário do caso");
            // the serial has to move past the row that was given an id
            sqlx::query(r#"SELECT setval('res_users_id_seq', GREATEST(1, (SELECT MAX("id") FROM "res_users")))"#)
                .execute(&pool)
                .await
                .expect("avançando a sequência de res.users");
        }
        // and the sequences the addons' data files would have loaded,
        // without which a numbered document cannot be created at all
        for (code, prefix) in sequences {
            registry
                .create(
                    &pool,
                    "ir.sequence",
                    vec![
                        ("name", json!(code)),
                        ("code", json!(code)),
                        ("prefix", json!(prefix)),
                        ("padding", json!(5)),
                        ("number_next", json!(1)),
                    ],
                )
                .await
                .expect("carregando a sequência do módulo");
        }

        Some(TransactionCase {
            registry: Arc::new(registry),
            methods,
            pool,
            schema,
        })
    }

    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// The registry by reference, for the ORM calls a test makes.
    pub fn models(&self) -> &Registry {
        &self.registry
    }

    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    pub fn methods(&self) -> MethodRegistry {
        self.methods.clone()
    }

    /// Drop the schema: the database ends the test as it started.
    ///
    /// A test that panics never reaches this, and the schema survives
    /// until the next run of the same case drops it — noise in the
    /// database is a better failure than a test that hides its own
    /// wreckage.
    pub async fn close(self) {
        if let Err(error) = sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&self.pool)
            .await
        {
            tracing::warn!("não foi possível limpar {}: {error}", self.schema);
        }
    }
}

/// A pool whose every connection lands in the case's schema.
fn pool_for(url: &str, schema: String) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
            let schema = schema.clone();
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .expect("conectando ao banco de teste")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_case_installs_what_it_asked_for_and_leaves_nothing_live() {
        let Some(case) = TransactionCase::open("selftest", &["base", "product", "sale"]).await
        else {
            return;
        };
        // the modules asked for are there, and nothing else is
        assert!(case.models().get("sale.order").is_some());
        assert!(case.models().get("product.product").is_some());
        assert!(case.models().get("res.partner").is_some());
        assert!(
            case.models().get("stock.picking").is_none(),
            "um caso instala o que pediu, não o mundo"
        );
        // the sequence of the module is loaded, so a document can be born
        // with a number
        let partner = case
            .models()
            .create(&case.pool(), "res.partner", vec![("name", json!("Ana"))])
            .await
            .unwrap();
        let order = case
            .models()
            .create(
                &case.pool(),
                "sale.order",
                vec![("partner_id", json!(partner))],
            )
            .await
            .unwrap();
        let rows = case
            .models()
            .read(&case.pool(), "sale.order", &[order], &["name"])
            .await
            .unwrap();
        assert_eq!(rows[0]["name"], "SO00001");

        let pool = case.pool();
        let schema = case.schema.clone();
        case.close().await;

        // the schema is gone: the database ends the test as it started
        let left: Option<String> = sqlx::query_scalar(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = $1",
        )
        .bind(&schema)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(left, None, "o caso não deixa resíduo");
    }
}
