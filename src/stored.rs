//! Stored templates: rows of the `jinja_templates` table, rendered by name.
//!
//! Each cached template carries an up-to-date check that re-reads its row
//! and compares the source. The check runs at most once per template, query
//! and thread: the first row a thread renders verifies the template and
//! everything it extends, includes or imports, and that thread's later rows
//! of the same query render from its cache. Edits are therefore picked up
//! by the next query, from any connection. A query with many rows reads each
//! template once per thread it runs on, not once per row.
//!
//! The table is read through the extension's own connection, so a query
//! sees committed templates: rows changed in the caller's still-open
//! transaction are not visible yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use minijinja::{Environment, Error, ErrorKind, Value};

use crate::duckdb::Connection;
use crate::environment::base_environment;

pub const TABLE_DDL: &str =
    "CREATE TABLE jinja_templates (name VARCHAR PRIMARY KEY, source VARCHAR NOT NULL)";

fn source_of(conn: &Connection, name: &str) -> Result<Option<String>, String> {
    conn.query_text(c"SELECT source FROM jinja_templates WHERE name = $1", name)
}

pub struct StoredEngine {
    env: Environment<'static>,
    /// While set, up-to-date checks pass without querying, because the
    /// current query has already verified the template being rendered.
    trust_cache: Arc<AtomicBool>,
}

impl StoredEngine {
    pub fn new(strict: bool, conn: Arc<Connection>) -> Self {
        let trust_cache = Arc::new(AtomicBool::new(false));
        let mut env = base_environment(strict);
        // Consult each cached template's up-to-date check on every lookup,
        // including lookups for `extends`, `include` and `import`.
        env.set_auto_reload(true);
        let trust = trust_cache.clone();
        env.set_loader(move |name| {
            let Some(source) = source_of(&conn, name).map_err(|e| load_error(name, e))? else {
                return Ok(None);
            };
            let (trust, conn, name, cached) =
                (trust.clone(), conn.clone(), name.to_owned(), source.clone());
            Ok(Some(
                minijinja::TemplateSource::new(source).with_uptodate_check(move || {
                    // Any error reads as stale, so the loader runs again and
                    // reports it.
                    trust.load(Ordering::Relaxed)
                        || matches!(source_of(&conn, &name), Ok(Some(s)) if s == cached)
                }),
            ))
        });
        Self { env, trust_cache }
    }

    /// Renders `name`. `verified_in_query` says whether an earlier row of
    /// the same query on this thread already checked it and the templates
    /// it uses.
    pub fn render(
        &mut self,
        name: &str,
        ctx: Value,
        fuel: Option<u64>,
        verified_in_query: bool,
    ) -> Result<String, Error> {
        self.trust_cache.store(verified_in_query, Ordering::Relaxed);
        self.env.set_fuel(fuel);
        let result = self.env.get_template(name).and_then(|t| t.render(ctx));
        self.trust_cache.store(false, Ordering::Relaxed);
        result
    }
}

fn load_error(name: &str, err: String) -> Error {
    let hint = if err.contains("jinja_templates does not exist") {
        format!(" (create it with: {TABLE_DDL})")
    } else {
        String::new()
    };
    Error::new(
        ErrorKind::InvalidOperation,
        format!("could not load template {name:?}: {err}{hint}"),
    )
}
