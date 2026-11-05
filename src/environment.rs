//! The MiniJinja environment configuration shared by every engine.

use minijinja::value::{Kwargs, Rest};
use minijinja::{Environment, Error, ErrorKind, State, UndefinedBehavior, Value};

pub fn base_environment(strict: bool) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(if strict {
        UndefinedBehavior::Strict
    } else {
        UndefinedBehavior::Lenient
    });
    // Python-style methods (`name.upper()`, `d.items()`), which Jinja2 users
    // expect to work.
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    register_contrib(&mut env);
    env
}

/// Registers the deterministic subset of minijinja-contrib.
///
/// `now()` and the `rand` helpers are left out on purpose: `jinja_render` is
/// a consistent (non-volatile) DuckDB function, so the optimizer may fold
/// constant calls and DuckDB may use it in generated columns and CHECK
/// constraints, which is only sound if the same input always renders the
/// same output.
fn register_contrib(env: &mut Environment<'static>) {
    use minijinja_contrib::{filters, globals};
    env.add_filter("pluralize", filters::pluralize);
    env.add_filter("filesizeformat", filters::filesizeformat);
    env.add_filter("truncate", truncate);
    env.add_filter("striptags", filters::striptags);
    env.add_filter("datetimeformat", filters::datetimeformat);
    env.add_filter("dateformat", filters::dateformat);
    env.add_filter("timeformat", filters::timeformat);
    env.add_function("cycler", globals::cycler);
    env.add_function("joiner", globals::joiner);
}

/// Jinja2's `truncate(s, length=255, killwords=False, end='...', leeway=None)`.
///
/// minijinja-contrib's version takes keyword arguments only, so the common
/// Jinja2 spelling `truncate(80)` would fail. This adapter folds positional
/// arguments into keywords and delegates.
fn truncate(
    state: &mut State,
    value: &Value,
    positional: Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    const NAMES: [&str; 4] = ["length", "killwords", "end", "leeway"];
    if positional.len() > NAMES.len() {
        return Err(Error::from(ErrorKind::TooManyArguments));
    }
    let mut merged: Vec<(String, Value)> = NAMES
        .iter()
        .zip(positional.iter())
        .map(|(name, v)| (name.to_string(), v.clone()))
        .collect();
    for name in kwargs.args() {
        if merged.iter().any(|(n, _)| n == name) {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                format!("truncate() got multiple values for argument {name:?}"),
            ));
        }
        merged.push((name.to_string(), kwargs.get::<Value>(name)?));
    }
    kwargs.assert_all_used()?;
    minijinja_contrib::filters::truncate(state, value, merged.into_iter().collect())
}

/// Parses `source` without rendering it and returns the syntax error, if any.
pub fn validate(source: &str) -> Result<(), Error> {
    // A throwaway environment, so that validating many one-off strings (for
    // example in a CHECK constraint during a bulk load) does not evict
    // templates that are actually being rendered.
    Environment::new().template_from_str(source).map(|_| ())
}
