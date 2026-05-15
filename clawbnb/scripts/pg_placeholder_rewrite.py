#!/usr/bin/env python3
"""
One-shot: convert SQLite-style `?` placeholders in `sqlx::query(...)` /
`sqlx::query_as(...)` callsites to Postgres-native `$1, $2, $3, ...`.

Scope: every `*.rs` file under `src/`. Strategy:
1. Find each `sqlx::query` / `sqlx::query_as` call (including `query_as::<_,T>`).
2. Walk forward to the first `)` that closes the SQL-literal arg only — the
   call may chain `.bind()/.fetch_*()` afterwards. We just need to rewrite
   the SQL inside the literal.
3. Inside that literal, replace each `?` with `$<n>` where `<n>` starts at
   1 per call. Be careful to leave `?` inside string contents that are
   *not* placeholders alone — but in our codebase every `?` inside these
   SQL literals IS a placeholder, so just count them in order.

Run:
    python3 scripts/pg_placeholder_rewrite.py
"""
import re
import sys
from pathlib import Path

CRATE = Path(__file__).resolve().parent.parent
SRC = CRATE / "src"

# Match calls of the form:
#   sqlx::query("...")
#   sqlx::query_as("...")
#   sqlx::query_as::<_, T>("...")
#   sqlx::query_as::<_, (...,...)>("...")
# Capture group 1 = the literal SQL string body (between the first " and
# the closing " of that arg). Rust string literals can span multiple
# lines so the `.` flag must include newline.
CALL_RE = re.compile(
    r'sqlx::query(?:_as)?(?:::<[^>]*>)?\(\s*"((?:\\.|[^"\\])*)"',
    re.DOTALL,
)


def rewrite_sql(sql_body: str) -> tuple[str, int]:
    """Replace each `?` with `$1, $2, ...` in order."""
    out = []
    n = 0
    for ch in sql_body:
        if ch == "?":
            n += 1
            out.append(f"${n}")
        else:
            out.append(ch)
    return ("".join(out), n)


def process_file(path: Path) -> int:
    """Returns number of placeholders rewritten in this file."""
    text = path.read_text(encoding="utf-8")
    total = 0
    new_text_parts = []
    last_end = 0
    for m in CALL_RE.finditer(text):
        sql_body = m.group(1)
        new_sql, n = rewrite_sql(sql_body)
        if n == 0:
            continue
        # Splice: everything up to start of the literal, the rewritten SQL,
        # then resume after the original literal.
        lit_start = m.start(1)
        lit_end = m.end(1)
        new_text_parts.append(text[last_end:lit_start])
        new_text_parts.append(new_sql)
        last_end = lit_end
        total += n
    if total == 0:
        return 0
    new_text_parts.append(text[last_end:])
    path.write_text("".join(new_text_parts), encoding="utf-8")
    return total


def main():
    grand = 0
    files_touched = 0
    for rs in sorted(SRC.rglob("*.rs")):
        # Skip the importer — it deliberately uses $N already because it
        # writes to PG.
        if rs.name == "import_sqlite.rs":
            continue
        n = process_file(rs)
        if n:
            print(f"  {rs.relative_to(CRATE)}: {n} placeholder(s)")
            grand += n
            files_touched += 1
    print(f"\nTotal: {grand} `?` → `$N` rewrites across {files_touched} file(s)")


if __name__ == "__main__":
    main()
