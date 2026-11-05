"""End-to-end tests of the packaged extension, loaded into DuckDB from Python.

Usage: python -m unittest discover -s tests -v
  (after `cargo build --release` and `python3 scripts/package.py ...`)
Set DUCKDB_JINJA_PATH to test an extension somewhere other than
build/jinja.duckdb_extension.
"""

import os
import pathlib
import tempfile
import threading
import unittest

import duckdb

ROOT = pathlib.Path(__file__).resolve().parents[1]
EXT_PATH = os.environ.get(
    "DUCKDB_JINJA_PATH", str(ROOT / "build" / "jinja.duckdb_extension")
)
TEMPLATES_DDL = (
    "CREATE TABLE jinja_templates (name VARCHAR PRIMARY KEY, source VARCHAR NOT NULL)"
)


def connect(path=":memory:"):
    # The extension is unsigned, which DuckDB refuses unless told otherwise.
    conn = duckdb.connect(path, config={"allow_unsigned_extensions": True})
    conn.execute(f"LOAD '{EXT_PATH}'")
    return conn


def one(conn, sql, params=None):
    return conn.execute(sql, params).fetchone()[0]


class InlineTest(unittest.TestCase):
    def setUp(self):
        self.conn = connect()

    def test_struct_context(self):
        out = one(
            self.conn,
            "SELECT jinja_render('Hi {{ name }}, {{ items | length }} items', "
            "{'name': 'Ada', 'items': [1, 2]})",
        )
        self.assertEqual(out, "Hi Ada, 2 items")

    def test_json_text_and_bound_parameters(self):
        out = one(
            self.conn,
            "SELECT jinja_render(?, ?::JSON)",
            ["{{ a.b }}-{{ c }}", '{"a": {"b": 1}, "c": "x"}'],
        )
        self.assertEqual(out, "1-x")

    def test_map_and_row_context(self):
        out = one(
            self.conn,
            "SELECT jinja_render('{{ k }}', MAP {'k': 'v'})",
        )
        self.assertEqual(out, "v")
        rows = self.conn.execute(
            "SELECT jinja_render('{{ n }}:{{ s }}', {'n': n, 's': s}) "
            "FROM (VALUES (1, 'a'), (2, 'b')) t(n, s) ORDER BY n"
        ).fetchall()
        self.assertEqual(rows, [("1:a",), ("2:b",)])

    def test_nulls(self):
        self.assertIsNone(one(self.conn, "SELECT jinja_render(NULL, {'a': 1})"))
        self.assertEqual(one(self.conn, "SELECT jinja_render('[{{ a }}]', NULL)"), "[]")
        self.assertEqual(one(self.conn, "SELECT jinja_render('ok')"), "ok")

    def test_strict_and_autoescape(self):
        self.assertEqual(one(self.conn, "SELECT jinja_render('[{{ a }}]', '{}')"), "[]")
        with self.assertRaisesRegex(duckdb.InvalidInputException, "undefined"):
            one(self.conn, "SELECT jinja_render('{{ a }}', '{}', true)")
        out = one(self.conn, "SELECT jinja_render('{{ x }}', {'x': '<b>'}, false, true)")
        self.assertEqual(out, "&lt;b&gt;")

    def test_errors(self):
        with self.assertRaisesRegex(duckdb.InvalidInputException, "syntax error"):
            one(self.conn, "SELECT jinja_render('{{ x }')")
        with self.assertRaisesRegex(duckdb.InvalidInputException, "must be a JSON object"):
            one(self.conn, "SELECT jinja_render('{{ x }}', '[1]')")
        # DuckDB validates text cast to the JSON parameter itself.
        with self.assertRaisesRegex(duckdb.ConversionException, "Malformed JSON"):
            one(self.conn, "SELECT jinja_render('{{ x }}', '{nope')")

    def test_validators(self):
        rows = self.conn.execute(
            "SELECT jinja_is_valid(t), jinja_syntax_error(t) IS NULL "
            "FROM (VALUES ('{{ ok }}'), ('{% for %}'), (NULL)) v(t)"
        ).fetchall()
        self.assertEqual(rows, [(True, True), (False, False), (None, True)])

    def test_check_constraint(self):
        self.conn.execute("CREATE TABLE t (src VARCHAR CHECK (jinja_is_valid(src)))")
        self.conn.execute("INSERT INTO t VALUES ('{{ fine }}')")
        with self.assertRaises(duckdb.ConstraintException):
            self.conn.execute("INSERT INTO t VALUES ('{% if %}')")

    def test_fuel(self):
        loop = "SELECT jinja_render('{% for i in range(100000) %}{{ i }}{% endfor %}')"
        self.assertEqual(one(self.conn, "SELECT jinja_set_fuel(1000)"), 10_000_000)
        with self.assertRaisesRegex(duckdb.InvalidInputException, "fuel"):
            one(self.conn, loop)
        self.assertEqual(one(self.conn, "SELECT jinja_set_fuel(0)"), 1000)
        self.assertEqual(len(one(self.conn, loop)), 488890)
        with self.assertRaisesRegex(duckdb.InvalidInputException, ">= 0"):
            one(self.conn, "SELECT jinja_set_fuel(-1)")

    def test_many_rows_on_many_threads(self):
        self.conn.execute("SET threads = 8")
        n, total = self.conn.execute(
            "SELECT count(DISTINCT r), sum(length(r)) FROM ("
            "  SELECT jinja_render('{{ i * 2 }}', {'i': i}) AS r FROM range(300000) t(i))"
        ).fetchone()
        self.assertEqual(n, 300000)
        expected = sum(len(str(i * 2)) for i in range(300000))
        self.assertEqual(total, expected)

    def test_concurrent_connections(self):
        errors = []

        def work(cursor, k):
            try:
                for i in range(200):
                    out = one(cursor, "SELECT jinja_render('{{ k }}-{{ i }}', {'k': ?, 'i': ?})", [k, i])
                    if out != f"{k}-{i}":
                        errors.append(out)
            except Exception as e:  # reported below; a thread cannot fail the test
                errors.append(e)

        threads = [
            threading.Thread(target=work, args=(self.conn.cursor(), k)) for k in range(6)
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(errors, [])


class StoredTest(unittest.TestCase):
    def test_missing_table_hint(self):
        conn = connect()
        with self.assertRaisesRegex(duckdb.InvalidInputException, "create it with: CREATE TABLE"):
            one(conn, "SELECT jinja_render_named('x')")

    def test_inheritance_and_edits(self):
        conn = connect()
        conn.execute(TEMPLATES_DDL)
        conn.execute(
            """INSERT INTO jinja_templates VALUES
                 ('base.html', '<p>{% block b %}{% endblock %}</p>'),
                 ('row', '{{ x }}{% include "sep" %}'),
                 ('sep', ';'),
                 ('page.html', '{% extends "base.html" %}{% block b %}{{ x }}{% endblock %}')"""
        )
        render = "SELECT jinja_render_named('page.html', {'x': '<b>'})"
        # `.html` names are autoescaped.
        self.assertEqual(one(conn, render), "<p>&lt;b&gt;</p>")

        other = conn.cursor()
        other.execute(
            "UPDATE jinja_templates SET source = '<div>{% block b %}{% endblock %}</div>' "
            "WHERE name = 'base.html'"
        )
        self.assertEqual(one(conn, render), "<div>&lt;b&gt;</div>")

        out = one(
            conn,
            "SELECT string_agg(jinja_render_named('row', {'x': i}), '' ORDER BY i) "
            "FROM range(5) t(i)",
        )
        self.assertEqual(out, "0;1;2;3;4;")

    def test_missing_template_and_strict(self):
        conn = connect()
        conn.execute(TEMPLATES_DDL)
        conn.execute("INSERT INTO jinja_templates VALUES ('t', '[{{ y }}]')")
        self.assertEqual(one(conn, "SELECT jinja_render_named('t')"), "[]")
        with self.assertRaisesRegex(duckdb.InvalidInputException, "undefined"):
            one(conn, "SELECT jinja_render_named('t', '{}', true)")
        with self.assertRaisesRegex(duckdb.InvalidInputException, "not found"):
            one(conn, "SELECT jinja_render_named('nope')")
        self.assertIsNone(one(conn, "SELECT jinja_render_named(NULL)"))

    def test_reads_committed_templates(self):
        conn = connect()
        conn.execute(TEMPLATES_DDL)
        conn.execute("INSERT INTO jinja_templates VALUES ('t', 'old')")
        conn.execute("BEGIN")
        conn.execute("UPDATE jinja_templates SET source = 'new' WHERE name = 't'")
        self.assertEqual(one(conn, "SELECT jinja_render_named('t')"), "old")
        conn.execute("COMMIT")
        self.assertEqual(one(conn, "SELECT jinja_render_named('t')"), "new")

    def test_file_database(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "db.duckdb")
            conn = connect(path)
            conn.execute(TEMPLATES_DDL)
            conn.execute("INSERT INTO jinja_templates VALUES ('t', '{{ 1 + 1 }}')")
            self.assertEqual(one(conn, "SELECT jinja_render_named('t')"), "2")
            conn.close()

    def test_recursive_rendering_is_refused(self):
        conn = connect()
        conn.execute("CREATE TABLE t (name VARCHAR, source VARCHAR)")
        conn.execute("INSERT INTO t VALUES ('a', 'A'), ('b', 'B')")
        conn.execute(
            "CREATE VIEW jinja_templates AS SELECT name, CASE WHEN name = 'b' "
            "THEN jinja_render_named('a') ELSE source END AS source FROM t"
        )
        with self.assertRaisesRegex(duckdb.InvalidInputException, "recursive rendering"):
            one(conn, "SELECT jinja_render_named('b')")


if __name__ == "__main__":
    unittest.main()
