//! The SQL functions and their registration.

use std::cell::{RefCell, RefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use thread_local::ThreadLocal;

use crate::duckdb::{Arg, Connection, Flags, Function, Output, Row, Type};
use crate::environment::validate;
use crate::inline::InlineEngine;
use crate::stored::StoredEngine;

/// Default instruction budget for one render. It guards against templates
/// such as `{% for i in range(10**9) %}`: a render never returns control to
/// DuckDB until it finishes, so interrupting the query cannot stop it.
const DEFAULT_FUEL: u64 = 10_000_000;

/// One thread's template engines, indexed by `strict as usize`.
struct Engines {
    inline: [InlineEngine; 2],
    stored: [StoredEngine; 2],
}

/// State shared by all the functions of one database.
struct Jinja {
    /// DuckDB runs a query on many threads at once. Each thread renders
    /// with engines of its own, so rendering never waits for another thread;
    /// the price is one compiled copy of a template per thread that uses it.
    /// The `RefCell` is only ever borrowed by its own thread, so a failed
    /// borrow means a render re-entered itself (see [`engines`]).
    engines: ThreadLocal<RefCell<Engines>>,
    /// Serves the lookups of stored templates for every thread.
    conn: Arc<Connection>,
    /// 0 means unlimited.
    fuel: AtomicU64,
}

impl Jinja {
    fn fuel(&self) -> Option<u64> {
        match self.fuel.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// This thread's engines, created on its first render.
    fn engines(&self) -> Result<RefMut<'_, Engines>, String> {
        self.engines
            .get_or(|| {
                RefCell::new(Engines {
                    inline: [InlineEngine::new(false), InlineEngine::new(true)],
                    stored: [
                        StoredEngine::new(false, self.conn.clone()),
                        StoredEngine::new(true, self.conn.clone()),
                    ],
                })
            })
            // Borrowed already: a render on this thread ran SQL that renders
            // again, for example when `jinja_templates` is a view that calls
            // `jinja_render_named`.
            .try_borrow_mut()
            .map_err(|_| "jinja: recursive rendering is not supported".to_owned())
    }
}

/// Registers the jinja_* functions through `conn`, which also serves the
/// lookups of stored templates.
pub fn register(conn: Arc<Connection>) -> Result<(), String> {
    let state = Arc::new(Jinja {
        engines: ThreadLocal::new(),
        conn: conn.clone(),
        fuel: AtomicU64::new(DEFAULT_FUEL),
    });
    // jinja_render and the validators are pure: the same arguments always
    // give the same result, so DuckDB may fold constant calls.
    let pure = Flags { volatile: false };
    // Reads jinja_templates or changes state: never folded.
    let volatile = Flags { volatile: true };
    use Type::{BigInt, Boolean, Json, Varchar};

    let s = state.clone();
    let render = Arc::new(move |row: &Row<'_>, _: &_| render_inline(&s, row));
    let mut f = Function::new("jinja_render", pure);
    for params in [
        &[Varchar][..],
        &[Varchar, Json],
        &[Varchar, Json, Boolean],
        &[Varchar, Json, Boolean, Boolean],
    ] {
        f = f.overload(params, Varchar, render.clone());
    }
    conn.register(f)?;

    let s = state.clone();
    let named = Arc::new(move |row: &Row<'_>, verified: &_| render_named(&s, row, verified));
    let mut f = Function::new("jinja_render_named", volatile);
    for params in [&[Varchar][..], &[Varchar, Json], &[Varchar, Json, Boolean]] {
        f = f.overload(params, Varchar, named.clone());
    }
    conn.register(f)?;

    conn.register(Function::new("jinja_is_valid", pure).overload(
        &[Varchar],
        Boolean,
        Arc::new(|row: &Row<'_>, _: &_| {
            Ok(match row.arg(0) {
                Arg::Null => Output::Null,
                _ => Output::Boolean(validate(text_arg(row, 0, "template")?).is_ok()),
            })
        }),
    ))?;
    conn.register(Function::new("jinja_syntax_error", pure).overload(
        &[Varchar],
        Varchar,
        Arc::new(|row: &Row<'_>, _: &_| {
            Ok(match row.arg(0) {
                Arg::Null => Output::Null,
                _ => match validate(text_arg(row, 0, "template")?) {
                    Ok(()) => Output::Null,
                    Err(e) => Output::Text(e.to_string()),
                },
            })
        }),
    ))?;

    let s = state;
    conn.register(Function::new("jinja_set_fuel", volatile).overload(
        &[BigInt],
        BigInt,
        Arc::new(move |row: &Row<'_>, _: &_| {
            let Arg::BigInt(fuel) = row.arg(0) else {
                return Err("jinja_set_fuel: fuel must be an integer (0 = unlimited)".into());
            };
            let fuel = u64::try_from(fuel)
                .map_err(|_| "jinja_set_fuel: fuel must be >= 0 (0 = unlimited)".to_owned())?;
            Ok(Output::BigInt(s.fuel.swap(fuel, Ordering::Relaxed) as i64))
        }),
    ))?;
    Ok(())
}

fn render_inline(state: &Jinja, row: &Row<'_>) -> Result<Output, String> {
    if matches!(row.arg(0), Arg::Null) {
        return Ok(Output::Null);
    }
    let source = text_arg(row, 0, "template")?;
    let context = context_arg(row, 1, "jinja_render")?;
    let strict = flag_arg(row, 2);
    let autoescape = flag_arg(row, 3);
    state.engines()?.inline[strict as usize]
        .render(source, autoescape, context, state.fuel())
        .map(Output::Text)
        .map_err(|e| format!("template render failed: {e}"))
}

fn render_named(
    state: &Jinja,
    row: &Row<'_>,
    verified: &crate::duckdb::QueryState,
) -> Result<Output, String> {
    if matches!(row.arg(0), Arg::Null) {
        return Ok(Output::Null);
    }
    let name = text_arg(row, 0, "name")?;
    let context = context_arg(row, 1, "jinja_render_named")?;
    let strict = flag_arg(row, 2);
    let mut seen = verified.lock().unwrap_or_else(|p| p.into_inner());
    let out = state.engines()?.stored[strict as usize]
        .render(name, context, state.fuel(), seen.contains(name))
        .map_err(|e| format!("rendering template {name:?} failed: {e}"))?;
    seen.insert(name.to_owned());
    Ok(Output::Text(out))
}

fn text_arg<'a>(row: &Row<'a>, idx: usize, what: &str) -> Result<&'a str, String> {
    match row.arg(idx) {
        Arg::Text(bytes) => {
            std::str::from_utf8(bytes).map_err(|_| format!("{what} is not valid UTF-8"))
        }
        other => Err(format!("{what} must be text, got {other:?}")),
    }
}

/// Parses the JSON context argument. Missing or NULL means empty.
fn context_arg(row: &Row<'_>, idx: usize, function: &str) -> Result<minijinja::Value, String> {
    let json = match row.arg(idx) {
        Arg::Null => return Ok(minijinja::Value::from(())),
        Arg::Text(bytes) => bytes,
        other => return Err(format!("{function} context must be JSON, got {other:?}")),
    };
    match serde_json::from_slice::<serde_json::Value>(json) {
        Ok(v @ serde_json::Value::Object(_)) => {
            Ok(minijinja::Value::from(minijinja::value::Serde(v)))
        }
        Ok(serde_json::Value::Null) => Ok(minijinja::Value::from(())),
        Ok(other) => Err(format!(
            "{function} context must be a JSON object, got {}",
            json_type(&other)
        )),
        Err(e) => Err(format!("{function} context is not valid JSON: {e}")),
    }
}

/// An optional BOOLEAN argument: missing or NULL is false.
fn flag_arg(row: &Row<'_>, idx: usize) -> bool {
    matches!(row.arg(idx), Arg::Boolean(true))
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
