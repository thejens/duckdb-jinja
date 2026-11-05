//! Inline templates: sources passed directly as SQL arguments.

use std::num::NonZeroUsize;

use lru::LruCache;
use minijinja::{Environment, Error, Value};

use crate::environment::base_environment;

/// Upper bound on distinct inline template sources kept compiled per
/// connection. Hitting it only costs a re-parse; it exists so that workloads
/// generating a unique template per row cannot grow memory without bound.
const CACHE_CAPACITY: usize = 512;

/// Cache key. The autoescape mode is part of it because MiniJinja decides
/// escaping when a template is compiled, so the same source must compile
/// separately for each mode.
#[derive(Hash, PartialEq, Eq)]
struct Key {
    source: String,
    autoescape: bool,
}

/// Renders inline templates. It has no loader, so `{% include %}` and
/// friends cannot reach `jinja_templates`. That is what keeps
/// `jinja_render` a pure function, so DuckDB may treat it as one.
pub struct InlineEngine {
    env: Environment<'static>,
    /// Maps template source to the name it is registered under in `env`.
    /// Keyed by the full source rather than a hash, so a hash collision can
    /// never render the wrong template.
    cache: LruCache<Key, String>,
    next_id: u64,
}

impl InlineEngine {
    pub fn new(strict: bool) -> Self {
        Self {
            env: base_environment(strict),
            cache: LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).expect("non-zero")),
            next_id: 0,
        }
    }

    pub fn render(
        &mut self,
        source: &str,
        autoescape: bool,
        ctx: Value,
        fuel: Option<u64>,
    ) -> Result<String, Error> {
        let name = self.template_name(source, autoescape)?;
        // Fuel can change between calls (jinja_set_fuel), so it is set per
        // render. It only aborts runaway templates and never changes the
        // output of a render that completes.
        self.env.set_fuel(fuel);
        self.env.get_template(&name)?.render(ctx)
    }

    fn template_name(&mut self, source: &str, autoescape: bool) -> Result<String, Error> {
        let key = Key {
            source: source.to_owned(),
            autoescape,
        };
        if let Some(name) = self.cache.get(&key) {
            return Ok(name.clone());
        }
        self.next_id += 1;
        // The `.html` suffix is what turns on HTML escaping: MiniJinja's
        // default autoescape callback selects the mode from the name's
        // extension.
        let name = if autoescape {
            format!("inline#{}.html", self.next_id)
        } else {
            format!("inline#{}", self.next_id)
        };
        // Compile before caching so a syntax error leaves no entry behind.
        self.env
            .add_template_owned(name.clone(), source.to_owned())?;
        if let Some((_, evicted)) = self.cache.push(key, name.clone()) {
            self.env.remove_template(&evicted);
        }
        Ok(name)
    }
}
