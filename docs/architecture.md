# How MAGI works

```text
.magi → lexer/parser → AST → name resolution + type checking → HIR
      → logical plans (reconciliation lowered to relational operators)
      → physical plan (unused datasets skipped, single-use datasets inlined as views)
      → SQL AST → DuckDB → exports
```

- `src/syntax` — hand-written lexer and recursive-descent parser with spans.
- `src/semantic` — resolution, types, nullability, functions, lineage (`magi trace`).
- `src/plan` — logical plan IR and the physical planner.
- `src/reconcile` — the reconciliation model and its lowering (documented in `lower.rs`).
- `src/backend` — the SQL AST/renderer, DuckDB lowering and the executor.
- `src/source`, `src/export` — Excel (calamine / rust_xlsxwriter), ODBC (odbc-api), files.
- `src/testing` — `test` declarations: a test's program (given sources, no exports), its run
  and the comparison with expected files (`magi test`).
- `src/lsp` — the language server (`magi lsp`, see [`editors.md`](editors.md)).
