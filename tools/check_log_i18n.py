#!/usr/bin/env python3
"""Fail when a runtime log message contains Chinese text.

Runtime logs are read by operators who may not read Chinese, so every
`trace!`/`debug!`/`info!`/`warn!`/`error!` invocation in this crate -- and in
the vendored `napcat-link` dependency -- must use an English format string.
Vendored code is additionally checked for `println!`/`eprintln!`, which it
uses for runtime status; this crate's own `println!` is CLI output and staying
localized is allowed there.

The check parses each Rust file, locates those macro invocations, walks the
balanced parentheses of each one, and inspects only the string literals
inside. Comments, field names, and non-log code are therefore out of scope --
Chinese identifiers or comments never trip it, and Chinese *data* passed as a
field value (a chat message, for example) does not either, because only
literals in the macro body are read.

Usage: python tools/check_log_i18n.py [root ...]
Defaults to `src` and `vendor/napcat-link/src`.
Exit code 0 when clean, 1 when any Chinese literal is found.
"""

import sys
from pathlib import Path

TRACING_MACROS = {"trace", "debug", "info", "warn", "error"}
PRINT_MACROS = {"println", "eprintln"}
MACRO_NAMES = TRACING_MACROS | PRINT_MACROS

DEFAULT_ROOTS = ("src", "vendor/napcat-link/src")

# CJK ideographs plus the fullwidth punctuation the logs used to contain.
CHINESE = [
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xF900, 0xFAFF),
    (0x3000, 0x303F),
    (0xFF01, 0xFF60),
]

IDENT = frozenset(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"
)


def is_chinese(text):
    return any(lo <= ord(ch) <= hi for ch in text for lo, hi in CHINESE)


def _raw_string_end(text, start):
    """Return (body_start, body_end, end) for a raw string at `start`, or None.

    Accepts the `r"..."`, `r#"..."#` family with an optional `b`/`c` prefix.
    """
    i = start
    if text[i] in "bc":
        i += 1
    if i >= len(text) or text[i] != "r":
        return None
    if start > 0 and text[start - 1] in IDENT:
        return None  # the `r` that ends an identifier, not a raw string
    i += 1
    hashes = 0
    while i < len(text) and text[i] == "#":
        hashes += 1
        i += 1
    if i >= len(text) or text[i] != '"':
        return None
    body_start = i + 1
    closing = '"' + "#" * hashes
    body_end = text.find(closing, body_start)
    if body_end < 0:
        return body_start, len(text), len(text)
    return body_start, body_end, body_end + len(closing)


def _char_literal_end(text, start):
    """Return the index past a char literal at `start`, or None for a lifetime."""
    i = start + 1
    n = len(text)
    if i >= n:
        return None
    if text[i] == "\\":
        # An escape always means a char literal, never a lifetime. The escaped
        # character is one code point, except for `\u{...}` which spans more.
        i += 2
        while i < n and text[i] != "'":
            if text[i] == "\\":
                i += 1
            i += 1
        return i + 1 if i < n else None
    if i + 1 < n and text[i + 1] == "'":
        return i + 2
    return None


def _normal_string_end(text, start):
    """Return (body_start, body_end, end) for a normal string literal."""
    i = start + 1
    n = len(text)
    while i < n:
        if text[i] == "\\":
            i += 2
            continue
        if text[i] == '"':
            return start + 1, i, i + 1
        i += 1
    return start + 1, n, n


def tokens(text):
    """Yield (kind, payload, offset) across the file.

    `kind` is "code" for an ordinary character, "literal" for a string body,
    and "ident" for an identifier. Comments, char literals, and raw-string
    delimiters are consumed without yielding.
    """
    i = 0
    n = len(text)
    while i < n:
        if text.startswith("//", i):
            i = text.find("\n", i)
            if i < 0:
                return
            continue
        if text.startswith("/*", i):
            depth = 1
            i += 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            continue
        raw = _raw_string_end(text, i)
        if raw:
            body_start, body_end, end = raw
            yield "literal", text[body_start:body_end], body_start
            i = end
            continue
        if text[i] == "'":
            end = _char_literal_end(text, i)
            i = end if end else i + 1
            continue
        if text[i] == '"':
            body_start, body_end, end = _normal_string_end(text, i)
            yield "literal", text[body_start:body_end], body_start
            i = end
            continue
        if text[i] in IDENT:
            end = i
            while end < n and text[end] in IDENT:
                end += 1
            yield "ident", text[i:end], i
            i = end
            continue
        yield "code", text[i], i
        i += 1


def _significant(text):
    """Return the non-whitespace tokens of `text`."""
    return [t for t in tokens(text) if t[0] != "code" or not t[1].isspace()]


def macro_invocations(text):
    """Yield (name, open_paren_offset) for each log macro invoked in code."""
    stream = _significant(text)
    for index, (kind, payload, _) in enumerate(stream):
        if kind != "ident" or payload not in MACRO_NAMES or index + 2 >= len(stream):
            continue
        bang, paren = stream[index + 1], stream[index + 2]
        if (bang[0], bang[1], paren[0], paren[1]) != ("code", "!", "code", "("):
            continue
        yield payload, paren[2]


def invocation_end(text, open_paren):
    """Return the index just past the `)` that closes `text[open_paren]`."""
    depth = 0
    for kind, payload, offset in tokens(text[open_paren:]):
        if kind != "code":
            continue
        if payload == "(":
            depth += 1
        elif payload == ")":
            depth -= 1
            if depth == 0:
                return open_paren + offset + 1
    return len(text)


def line_of(text, offset):
    return text.count("\n", 0, offset) + 1


def is_vendored(path):
    """True for dependency source copied under `vendor/`."""
    return "vendor" in path.parts


def scan_file(path):
    """Yield (line, macro name, literal) for Chinese literals in log macros."""
    text = path.read_text(encoding="utf-8")
    vendored = is_vendored(path)
    for name, paren in macro_invocations(text):
        if name in PRINT_MACROS and not vendored:
            continue  # this crate's own println! is localized CLI output
        end = invocation_end(text, paren)
        for kind, payload, _ in tokens(text[paren:end]):
            if kind == "literal" and is_chinese(payload):
                yield line_of(text, paren), name, payload


def main():
    roots = [Path(arg) for arg in sys.argv[1:]] or [
        Path(root) for root in DEFAULT_ROOTS
    ]
    findings = 0
    tracing = 0
    prints = 0
    for root in roots:
        if not root.is_dir():
            print(f"skip: {root} is not a directory")
            continue
        for path in sorted(root.rglob("*.rs")):
            text = path.read_text(encoding="utf-8")
            vendored = is_vendored(path)
            for name, _ in macro_invocations(text):
                if name in TRACING_MACROS:
                    tracing += 1
                elif vendored:
                    prints += 1
            for line, name, literal in scan_file(path):
                findings += 1
                print(f"{path}:{line}: {name}! -> {literal!r}")
    print(
        f"checked {tracing} tracing macro invocation(s) "
        f"and {prints} vendored println!/eprintln! call(s)"
    )
    if findings:
        print(f"FAIL: {findings} Chinese string literal(s) inside runtime log macros")
        return 1
    print("OK: no Chinese string literals inside runtime log macros")
    return 0


if __name__ == "__main__":
    sys.exit(main())
