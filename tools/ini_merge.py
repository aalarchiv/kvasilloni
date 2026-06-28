#!/usr/bin/env python3
"""
ini_merge.py - produce a well-commented kvasilloni.ini from a user's settings.

Takes the documented template (kvasilloni.ini.example - the "default, commented"
ini) and a user-supplied kvasilloni.ini, and writes a merged ini that keeps ALL
the template's explanatory comments and section layout but with the user's values
filled in:

  * a key the user set is emitted as an active line with the user's value,
    uncommenting the template's default line if it was commented out;
  * a key the user did NOT set keeps the template line unchanged (an active
    default stays, a commented optional default stays commented);
  * a user key the template doesn't know about is passed through at the end under
    a clearly marked section (and a warning is printed to stderr) so nothing the
    user configured is silently lost.

Key matching follows the shim's own loader (src/config.rs): keys are
case-insensitive, a `[section]` header is ignored, and an inline `;`/`#` comment
after a value is stripped before the value is used.

Usage:
    ./ini_merge.py user.ini                       # template auto-located; -> stdout
    ./ini_merge.py user.ini -o kvasilloni.ini     # write to a file
    ./ini_merge.py user.ini -t path/to/template.ini

The template defaults to kvasilloni.ini.example next to this script's repo (or
pass -t). Output goes to stdout unless -o is given.
"""

import argparse
import os
import re
import sys

# The config keys the shim's loader recognises (src/config.rs). Used to tell a
# real key line apart from prose that happens to contain '='.
KNOWN_KEYS = {
    "host", "port", "localport", "proto", "tcprole",
    "peercheck", "allow", "udpportfallback",
    "connecttimeout", "accepttimeout", "channels", "log",
}

# A key line, optionally commented out: leading indent, optional ;/# marker, the
# key, '=', then the rest (value plus any trailing inline comment).
KEY_LINE = re.compile(r"^(\s*)([;#]\s*)?([A-Za-z_]\w*)(\s*)=(\s*)(.*)$")
# An inline comment inside the value part (the first ; or #, with its leading ws).
INLINE_COMMENT = re.compile(r"\s*[;#].*$")


def force_utf8_io() -> None:
    """Make stdout/stderr UTF-8, matching the explicit utf-8 file I/O below.

    On Windows the console and a redirected stdout default to the legacy ANSI
    code page (e.g. cp1252), not UTF-8. Without this, a non-ASCII value in a
    user's ini makes `sys.stdout.write`/error output raise UnicodeEncodeError,
    and a redirected default-stdout run would write the file in cp1252 instead
    of the UTF-8 that the `-o` path already produces. `reconfigure` is Python
    3.7+; on 3.6 the streams are left as-is (and Windows users on 3.7+ get the
    fix)."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8")


def warn(message: str) -> None:
    print(f"ini_merge: {message}", file=sys.stderr)


def strip_inline_comment(text: str) -> str:
    """Drop an inline ;/# comment and surrounding whitespace, like the shim does."""
    return INLINE_COMMENT.sub("", text).strip()


def parse_user_ini(text: str) -> "dict[str, str]":
    """Map key -> value from a user ini, mirroring src/config.rs parse_ini:
    lowercase keys, ignore blank/comment/[section] lines, strip inline comments.
    Last occurrence of a duplicated key wins (same as a HashMap insert)."""
    values = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line[0] in ";#[":
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        values[key.strip().lower()] = strip_inline_comment(value)
    return values


def merge(template: str, user: "dict[str, str]") -> str:
    """Walk the template, substituting the user's values into recognised key
    lines and returning the merged text. `user` is consumed in place: keys that
    get applied are removed, so whatever remains is "not in the template"."""
    out_lines = []
    applied = []
    for raw in template.splitlines():
        m = KEY_LINE.match(raw)
        if not m:
            out_lines.append(raw)
            continue
        indent, _marker, key, _ws_k, _ws_v, rest = m.groups()
        key_lc = key.lower()
        if key_lc not in KNOWN_KEYS or key_lc not in user:
            out_lines.append(raw)  # prose-with-'=', or a key the user left alone
            continue
        # Apply the user's value: emit an active line, preserve any explanatory
        # inline comment the template attached to this key.
        cmatch = INLINE_COMMENT.search(rest)
        inline = cmatch.group() if cmatch else ""
        out_lines.append(f"{indent}{key} = {user[key_lc]}{inline}")
        applied.append(key_lc)
        del user[key_lc]
    return "\n".join(out_lines), applied


def main() -> None:
    force_utf8_io()
    here = os.path.dirname(os.path.abspath(__file__))
    default_template = os.path.normpath(os.path.join(here, "..", "kvasilloni.ini.example"))

    parser = argparse.ArgumentParser(
        description="Merge a user's kvasilloni.ini onto the commented template.",
    )
    parser.add_argument("user", help="the user-supplied kvasilloni.ini")
    parser.add_argument("-t", "--template", default=default_template,
                        help=f"commented template ini (default: {default_template})")
    parser.add_argument("-o", "--output", default=None,
                        help="output file (default: stdout)")
    args = parser.parse_args()

    try:
        with open(args.template, "r", encoding="utf-8") as f:
            template = f.read()
    except OSError as exc:
        sys.exit(f"error: cannot read template {args.template!r}: {exc}")
    try:
        with open(args.user, "r", encoding="utf-8") as f:
            user_text = f.read()
    except OSError as exc:
        sys.exit(f"error: cannot read user ini {args.user!r}: {exc}")

    user = parse_user_ini(user_text)
    total_user_keys = len(user)
    merged, applied = merge(template, user)  # `user` now holds only leftovers

    # Anything left in `user` had no matching key line in the template.
    leftovers = user
    if leftovers:
        extra = ["", "; --- settings from your ini with no matching template key ---"]
        for key in sorted(leftovers):
            if key not in KNOWN_KEYS:
                warn(f"unknown key {key!r} passed through (the shim ignores unknown keys)")
                extra.append(f"; (not a recognised kvasilloni key)")
            extra.append(f"{key} = {leftovers[key]}")
        merged = merged + "\n" + "\n".join(extra)

    output = merged.rstrip("\n") + "\n"
    if args.output:
        try:
            with open(args.output, "w", encoding="utf-8") as f:
                f.write(output)
        except OSError as exc:
            sys.exit(f"error: cannot write {args.output!r}: {exc}")
        dest = args.output
    else:
        sys.stdout.write(output)
        dest = "stdout"

    warn(f"applied {len(applied)} of {total_user_keys} user setting(s) "
         f"({len(leftovers)} passed through) -> {dest}")


if __name__ == "__main__":
    main()
