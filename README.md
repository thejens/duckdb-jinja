# duckdb-jinja: Jinja2 templates in DuckDB

[![CI](https://github.com/thejens/duckdb-jinja/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/duckdb-jinja/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Linux, macOS, Windows](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

**duckdb-jinja is a DuckDB extension for rendering Jinja2 templates directly in SQL.**
Generate emails, HTML, Markdown, JSON, prompts or reports from table rows, in any program that uses DuckDB.
It is written in Rust and uses [MiniJinja](https://github.com/mitsuhiko/minijinja), the Jinja2-compatible template engine by the creator of Jinja2.

It is the DuckDB counterpart of [pgjinja](https://github.com/thejens/pgjinja) for PostgreSQL and [sqlite-jinja](https://github.com/thejens/sqlite-jinja) for SQLite. All three share the same function names and template behaviour.

```sql
LOAD 'jinja.duckdb_extension';

SELECT jinja_render(
  'Hi {{ name }}, you have {{ n }} new {{ "message" if n == 1 else "messages" }}',
  {'name': 'Ada', 'n': 3}
);
-- Hi Ada, you have 3 new messages
```

- **Jinja2 syntax you already know:** `{{ }}`, `{% for %}`, `{% if %}`, filters, macros, `{% extends %}`, `{% include %}`, and Python-style methods such as `name.title()`.
- **Render from row data:** pass a STRUCT, a MAP or JSON as the context. DuckDB converts it for you.
- **Store templates in a table:** stored templates can extend, include and import each other, and edits are picked up by the next query.
- **Fast and parallel:** templates are compiled once and cached on each of DuckDB's threads, so rendering scales with the query: about 0.4 µs per row on 8 threads.
- **Safe by design:** HTML/JSON autoescaping, a per-render instruction budget against runaway loops, and no file system or network access.

## Contents

- [Installation](#installation)
- [Loading the extension](#loading-the-extension)
- [Usage](#usage)
- [Function reference](#function-reference)
- [Template language](#template-language)
- [How stored templates stay fresh](#how-stored-templates-stay-fresh)
- [Performance](#performance)
- [FAQ](#faq)
- [Development](#development)

## Installation

duckdb-jinja is built on DuckDB's stable C extension API (C API version v1.5.6), so one build loads into DuckDB 2.0 and the DuckDB releases that provide that API version. Loading into an older DuckDB fails with a message naming the version it needs.

**Prebuilt binaries:** download `jinja.duckdb_extension` for your platform from the [releases page](https://github.com/thejens/duckdb-jinja/releases). Builds are provided for Linux (x86_64, ARM64), macOS (Apple Silicon, Intel) and Windows (x86_64).

**From source:** with a [Rust toolchain](https://rustup.rs) and Python 3:

```sh
git clone https://github.com/thejens/duckdb-jinja.git
cd duckdb-jinja
cargo build --release
python3 scripts/package.py target/release/libduckdb_jinja.dylib --platform osx_arm64
# -> build/jinja.duckdb_extension
```

`package.py` appends the metadata footer DuckDB requires. Pass the library for your platform (`libduckdb_jinja.so` on Linux, `duckdb_jinja.dll` on Windows) and DuckDB's name for it, as `PRAGMA platform` reports it: `linux_amd64`, `linux_arm64`, `osx_arm64`, `osx_amd64`, `windows_amd64`.

## Loading the extension

The extension is not signed by DuckDB, so DuckDB loads it only when unsigned extensions are allowed. Keep the file name `jinja.duckdb_extension`: DuckDB derives the entry point from it.

**DuckDB CLI**

```sh
duckdb -unsigned
```

```sql
LOAD './jinja.duckdb_extension';
```

**Python**

```python
import duckdb

conn = duckdb.connect("app.duckdb", config={"allow_unsigned_extensions": True})
conn.execute("LOAD './jinja.duckdb_extension'")

conn.execute("SELECT jinja_render('Hello {{ who }}', {'who': ?})", ["world"]).fetchone()
```

The functions belong to the database, so every connection to it can use them once one has loaded the extension.

## Usage

### Render a template with row data

Build the context with a STRUCT literal. Nested structs and lists become nested objects and lists:

```sql
CREATE TABLE users (id INTEGER, name VARCHAR, tags VARCHAR[]);
INSERT INTO users VALUES (1, 'Ada', ['math', 'poetry']);

SELECT jinja_render('#{{ id }} {{ name }}: {{ tags | join(", ") }}', {'id': id, 'name': name, 'tags': tags})
FROM users;
-- #1 Ada: math, poetry
```

`to_json(u)` passes a whole row: `SELECT jinja_render('{{ name }}', to_json(u)) FROM users u`. A JSON column or `MAP` works as well.

Templates can also come from a column, so each row can carry its own template:

```sql
SELECT jinja_render(n.template, {'name': u.name})
FROM notifications n JOIN users u ON u.id = n.user_id;
```

### Stored templates with inheritance and includes

Create a `jinja_templates` table and render its rows by name with `jinja_render_named`. Stored templates can `{% extends %}`, `{% include %}` and `{% import %}` each other, like files in a Jinja2 template directory:

```sql
CREATE TABLE jinja_templates (name VARCHAR PRIMARY KEY, source VARCHAR NOT NULL);

INSERT INTO jinja_templates VALUES
  ('layout.html', '<h1>{% block title %}{% endblock %}</h1>{% block body %}{% endblock %}{% include "footer.html" %}'),
  ('footer.html', '<footer>{{ company }}</footer>'),
  ('macros.html', '{% macro badge(t) %}<i>{{ t }}</i>{% endmacro %}'),
  ('page.html',   '{% extends "layout.html" %}{% from "macros.html" import badge %}{% block title %}{{ title }}{% endblock %}{% block body %}{{ badge(tag) }}{% endblock %}');

SELECT jinja_render_named('page.html', {'title': 'Hi <you>', 'tag': 'new', 'company': 'A&B'});
-- <h1>Hi &lt;you&gt;</h1><i>new</i><footer>A&amp;B</footer>
```

Edits to `jinja_templates` take effect from the next query. See [How stored templates stay fresh](#how-stored-templates-stay-fresh).

### HTML and JSON autoescaping

Inline templates are not escaped by default. Pass `true` as the fourth argument (`autoescape`) to turn escaping on:

```sql
SELECT jinja_render('<p>{{ c }}</p>', {'c': '<script>'}, false, true);
-- <p>&lt;script&gt;</p>
```

Stored templates follow the Jinja2 convention of choosing the escaping mode from the template name:

| Name ends in | Escaping |
|---|---|
| `.html`, `.htm`, `.xml` | HTML |
| `.json`, `.json5`, `.js`, `.yaml`, `.yml` | JSON (values are serialized as JSON) |
| anything else | none |

Use `{{ value | safe }}` to opt out for trusted markup.

### Validate templates with a CHECK constraint

```sql
CREATE TABLE email_templates (
  name VARCHAR PRIMARY KEY,
  body VARCHAR NOT NULL CHECK (jinja_is_valid(body))
);

SELECT jinja_syntax_error('{% for x in %}');
-- syntax error: unexpected end of block (in <string>:1)
```

## Function reference

| Function | Description |
|---|---|
| `jinja_render(template [, context [, strict [, autoescape]]])` | Renders an inline template. |
| `jinja_render_named(name [, context [, strict]])` | Renders a template from `jinja_templates`, resolving `extends`, `include` and `import` from the same table. |
| `jinja_is_valid(template)` | `true` if the template parses, else `false`. |
| `jinja_syntax_error(template)` | The parse error, or `NULL` if the template is valid. |
| `jinja_set_fuel(n)` | Sets the database's per-render instruction budget (`0` = unlimited; default 10,000,000) and returns the previous value. |

Arguments:

- `context` is `JSON`: a STRUCT, MAP or JSON text holding an object. `NULL` or omitted means an empty context. Text that is not valid JSON is rejected by DuckDB before rendering.
- A `NULL` template or name returns `NULL`.
- `strict` and `autoescape` are booleans.
- With `strict = true`, referencing an undefined variable raises an error instead of rendering an empty string (Jinja2's `StrictUndefined`).

`jinja_render`, `jinja_is_valid` and `jinja_syntax_error` are pure functions, so DuckDB may evaluate constant calls once. `jinja_render_named` and `jinja_set_fuel` are volatile.

The fuel limit exists because a render doesn't return control to DuckDB until it finishes. Interrupting the query can't stop a template like `{% for i in range(10**9) %}`, but fuel can.

## Template language

duckdb-jinja uses MiniJinja, which implements nearly all of Jinja2:
- variables, filters and tests
- `if`, `for` (including `loop.*`, `break`, `continue`) and `set`
- macros, `call` blocks, and template inheritance with `extends`, `block`, `include`, `import` and `from`
- whitespace control and expressions

See the [MiniJinja syntax documentation](https://docs.rs/minijinja/latest/minijinja/syntax/index.html) and its [list of differences from Jinja2](https://github.com/mitsuhiko/minijinja/blob/main/COMPATIBILITY.md).

Available on top of the built-in filters:
- Python-style methods on strings, lists and dicts (`.upper()`, `.split()`, `.items()`, `.get()`, …)
- `tojson`, `urlencode`, `pluralize`, `truncate` (Jinja2-compatible positional arguments), `striptags`, `filesizeformat`
- `datetimeformat`, `dateformat` and `timeformat`
- `cycler([...])` and `joiner()`

Deliberately not available, to keep rendering deterministic: `now()` and random-number helpers. Pass `now()` from SQL in the context if you need it.

## How stored templates stay fresh

- Each cached stored template remembers the source it was compiled from.
- The **first time a query renders** a template on one of its threads, every template involved (including partials from `extends`, `include` and `import`) is re-read and compared with that source. Anything that changed is recompiled.
- The remaining rows that thread renders for the query use the cache with no table access. A query over a million rows reads `jinja_templates` once per thread, not a million times.

`jinja_templates` is read through the extension's own connection to the database, which has two consequences:

- A query sees **committed** templates. Changes made in a transaction that is still open become visible to `jinja_render_named` once it commits.
- `jinja_templates` should be a table, or a view that does not itself render templates. A render that needs to render to load its template is refused with "recursive rendering is not supported".

`jinja_render` has no access to `jinja_templates`, so `{% include %}` in an inline template fails with "template not found". That separation is what lets `jinja_render` be pure.

## Performance

Measured on an Apple M-series laptop (8 cores) with DuckDB 2.0, release build, 1,000,000 rows:

| Query | 8 threads | 1 thread |
|---|---|---|
| `printf('<p>#%d %s</p>', id, customer)` baseline | 61 ms | 194 ms |
| `jinja_render(...)` with a 2-field STRUCT | 376 ms | 864 ms |
| `jinja_render_named(...)` with `extends` and `block` | 520 ms | 1162 ms |

## FAQ

### How do I render a Jinja2 template in DuckDB?

Load duckdb-jinja and call `jinja_render(template, {'key': value, ...})`.

### Why does DuckDB need `-unsigned` / `allow_unsigned_extensions`?

DuckDB only loads extensions signed by its own key, or community extensions, without that setting. duckdb-jinja is distributed from GitHub, not through DuckDB's extension repository.

### Is it safe to render user-supplied templates?

Templates can't touch the file system, the network or other tables. `jinja_render_named` reads only `jinja_templates`. The fuel limit bounds CPU per render. Fuel does not bound memory: a single expression such as `'x' * 10**9` can still allocate a lot. Treat untrusted templates with the same care as untrusted SQL expressions, and enable autoescaping whenever the output is HTML.

### Does it work with Jinja2 templates from my Python or Ansible project?

Mostly, yes. MiniJinja covers the Jinja2 language. Python-specific extensions, custom filters and Ansible-specific filters are not available.

## Development

```sh
cargo build --release
python3 scripts/package.py target/release/libduckdb_jinja.dylib --platform osx_arm64
pip install -r tests/requirements.txt
python -m unittest discover -s tests -v   # end-to-end tests of build/jinja.duckdb_extension
```

Releases are built by pushing a `v*` tag; see `.github/workflows/release.yml`.

Code layout:

| File | Responsibility |
|---|---|
| `src/lib.rs` | The entry point DuckDB calls on `LOAD` |
| `src/duckdb.rs` | A small safe wrapper over the C extension API |
| `src/functions.rs` | SQL functions, argument handling, registration |
| `src/inline.rs` | Inline templates and their LRU cache |
| `src/stored.rs` | `jinja_templates` loading and per-query freshness checks |
| `src/environment.rs` | Shared MiniJinja configuration and filters |
| `scripts/package.py` | Appends DuckDB's extension metadata to the built library |
| `tests/` | End-to-end tests of the packaged extension |

## License

MIT. See [LICENSE](LICENSE).
