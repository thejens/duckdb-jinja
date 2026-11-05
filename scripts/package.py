#!/usr/bin/env python3
"""Turns the built library into a loadable DuckDB extension.

DuckDB only loads files that end in a 512-byte metadata footer naming the
platform, the C API version and the ABI. This writes
`<out-dir>/jinja.duckdb_extension`: the library with that footer appended,
unsigned. The format follows DuckDB's extension-ci-tools
(scripts/append_extension_metadata.py).

Usage: python3 scripts/package.py LIBRARY --platform PLATFORM [--out-dir DIR]

PLATFORM is DuckDB's name for the target, as `PRAGMA platform` reports it:
linux_amd64, linux_arm64, osx_arm64, osx_amd64, windows_amd64, ...
"""

import argparse
import pathlib
import re
import shutil

EXTENSION_NAME = "jinja"
# The C API version the extension requests (C_API_VERSION in src/lib.rs).
C_API_VERSION = "v1.5.6"
ROOT = pathlib.Path(__file__).resolve().parent.parent


def field(value: str) -> bytes:
    data = value.encode("ascii")
    if len(data) > 32:
        raise ValueError(f"metadata field {value!r} is longer than 32 bytes")
    return data + b"\x00" * (32 - len(data))


def footer(platform: str, extension_version: str) -> bytes:
    # A WebAssembly custom-section header, which native loaders skip and
    # DuckDB-Wasm reads, then eight 32-byte fields in reverse order, then
    # 256 bytes for a signature (left empty: the extension is unsigned).
    start = bytes([0, 147, 4, 16]) + b"duckdb_signature" + bytes([128, 4])
    fields = [
        "",  # unused
        "",  # unused
        "",  # unused
        "C_STRUCT",  # ABI: the stable C API
        extension_version,
        C_API_VERSION,
        platform,
        "4",  # magic value identifying a DuckDB extension
    ]
    return start + b"".join(field(f) for f in fields) + b"\x00" * 256


def crate_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', text, re.MULTILINE)
    if not match:
        raise SystemExit("Cargo.toml has no version")
    return "v" + match.group(1)


def main() -> None:
    parser = argparse.ArgumentParser(description="Turns the built library into a loadable DuckDB extension.")
    parser.add_argument("library", type=pathlib.Path, help="the built cdylib")
    parser.add_argument("--platform", required=True, help="DuckDB platform, e.g. osx_arm64")
    parser.add_argument("--out-dir", type=pathlib.Path, default=ROOT / "build")
    args = parser.parse_args()

    args.out_dir.mkdir(parents=True, exist_ok=True)
    out = args.out_dir / f"{EXTENSION_NAME}.duckdb_extension"
    tmp = out.with_suffix(".tmp")
    shutil.copyfile(args.library, tmp)
    with open(tmp, "ab") as f:
        f.write(footer(args.platform, crate_version()))
    tmp.replace(out)
    print(out)


if __name__ == "__main__":
    main()
