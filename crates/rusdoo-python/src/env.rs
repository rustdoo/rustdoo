//! The environment a Python recordset reaches the database through.
//!
//! `self.env` in Odoo is a registry, a cursor and a user. Here it is the
//! same three, plus the one thing an embedded interpreter forces on the
//! design: CPython is synchronous and this ORM is not, so every call
//! Python makes into the database has to cross that line.
//!
//! It crosses by blocking. The alternative — making the Python side
//! async — is not available: an addon's `models.py` calls
//! `self.env['x'].search(...)` and expects records back on the next
//! line, and no bridge can change what is already written. What a bridge
//! *can* do is make the blocking explicit and keep it off the async
//! executor's threads, which is what [`with_env`] is for.
//!
//! The environment is a thread-local rather than an argument because the
//! call arrives from inside the interpreter: `_rusdoo.read(...)` is
//! reached from Python code that the bridge did not write and cannot
//! thread a handle through.

use rusdoo_core::RusdooError;
use rusdoo_orm::registry::Registry;
use sqlx::PgPool;
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;

/// What a Python call needs to reach the database.
#[derive(Clone)]
pub struct Env {
    pub registry: Arc<Registry>,
    pub pool: PgPool,
    /// the user the calls are made as — a Python method writes as them,
    /// never as root
    pub uid: i64,
    /// the runtime the blocking calls are handed back to
    pub handle: tokio::runtime::Handle,
}

thread_local! {
    static CURRENT: RefCell<Option<Env>> = const { RefCell::new(None) };
}

/// Run `body` with `env` reachable from Python, and take it away after.
///
/// Scoped on purpose: an environment that outlived the call would let a
/// stray Python reference reach a pool whose transaction is over, and
/// the failure would land far from the cause.
pub fn with_env<T>(env: Env, body: impl FnOnce() -> T) -> T {
    CURRENT.with(|slot| *slot.borrow_mut() = Some(env));
    let outcome = body();
    CURRENT.with(|slot| *slot.borrow_mut() = None);
    outcome
}

/// The environment of the call in progress.
pub fn current() -> Result<Env, RusdooError> {
    CURRENT
        .with(|slot| slot.borrow().clone())
        .ok_or_else(|| {
            RusdooError::Validation(
                "this Python code reached the database outside a call: there is no environment"
                    .into(),
            )
        })
}

/// Run an ORM future from Python, blocking until it answers.
///
/// Two things are released, and leaving out either one costs the whole
/// server its concurrency.
///
/// **The GIL.** This is reached from inside a Python call, which holds
/// it, and what follows is a query — microseconds of bytecode in front
/// of a round trip to Postgres. Holding the interpreter lock across that
/// wait would serialize every Python call in the process on the
/// *database's* latency rather than on any Python running, and no second
/// interpreter would fix it: there is nothing to run in parallel, only
/// something to wait for. Measured, holding it flattened throughput to
/// 1.0x at every level of concurrency; releasing it is what makes the
/// bridge scale at all (`examples/gil_bench.rs`).
///
/// **The worker thread.** Blocking a tokio worker without telling the
/// runtime would stall every other task sharing it. Told, the runtime
/// moves the rest of its work elsewhere. Off a runtime thread there is
/// nothing to warn, and the handle blocks directly.
pub fn wait<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    let env = CURRENT.with(|slot| slot.borrow().clone());
    let Some(env) = env else {
        // no environment: the caller is about to get a clear error from
        // `current()` anyway, so this only has to not panic
        return futures_lite_block_on(future);
    };
    let block = move || match tokio::runtime::Handle::try_current() {
        Ok(_) => tokio::task::block_in_place(|| env.handle.block_on(future)),
        Err(_) => env.handle.block_on(future),
    };
    // `attach` first because `detach` needs a token to prove the GIL is
    // there to be given up. On the thread that called into Python it
    // already is, and attaching to what is held costs nothing.
    pyo3::Python::attach(|py| py.detach(block))
}

/// A last-resort executor for the case above.
fn futures_lite_block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}
