# MAGI

**A language for reproducible business-data analysis and reconciliation.**

MAGI takes data from SQL databases, Excel workbooks and flat files through source contracts,
normalisation, validation and policy-driven reconciliation, and delivers Excel, CSV, Parquet,
JSON and DuckDB outputs. A MAGI program is compiled — parsed, resolved, type checked, planned —
and executed on an embedded [DuckDB](https://duckdb.org), which defines MAGI's execution
semantics.

```bash
magi check   analysis.magi              # parse, resolve, type check (no execution)
magi plan    analysis.magi              # the steps a run would take
magi run     analysis.magi              # execute and write exports
magi explain analysis.magi matches      # how a relation is computed
magi sql     analysis.magi matches      # the DuckDB SQL `run` executes (whole program: settings first)
magi schema  analysis.magi a            # columns of a source or relation
magi trace   analysis.magi summary.total  # where a column's values come from
magi fmt     analysis.magi lib/         # canonical formatting (files, or every .magi file in a directory)
magi compile analysis.magi --target tmdl  # Power BI semantic model from `model` declarations
```

MAGI is a single binary with DuckDB linked in; the only system library it needs beyond the C
runtime is the ODBC driver manager (unixODBC on Linux/macOS, built into Windows). Build it with
`cargo build --release`.

## A first program

```magi
source a = excel("a.xlsx") { sheet: "Data" }
source b = excel("b.xlsx") { sheet: "Data" }

dataset clean_a = a
    |> derive category = lower(trim(category))
    |> filter amount > 0

dataset clean_b = b
    |> derive category = lower(trim(category))
    |> filter amount > 0

dataset summary = clean_a
    |> join clean_b on organization_id
    |> group by clean_a.category
    |> aggregate {
        a_total = sum(clean_a.amount)
        b_total = sum(clean_b.amount)
    }

export summary to "summary.xlsx"
```

Both join inputs have a `category` column, so the program groups by `clean_a.category`; a bare
`category` is reported as ambiguous (M011) instead of guessed.

## Documentation

- [`docs/language.md`](docs/language.md): the language: sources and source contracts,
  datasets, expressions and functions, validation, reconciliation, exports, BI models, runtime.
- [`docs/architecture.md`](docs/architecture.md): the compiler pipeline and the source layout.
- [`docs/diagnostics.md`](docs/diagnostics.md): every diagnostic code.
- [`docs/roadmap.md`](docs/roadmap.md): what is not implemented yet.

## Testing

```bash
cargo test                                   # unit, end-to-end and snapshot tests
scripts/build-sqlite-odbc.sh                 # builds a SQLite ODBC driver for the ODBC test
MAGI_TEST_SQLITE_ODBC_DRIVER=$(scripts/build-sqlite-odbc.sh) cargo test -- --include-ignored
```

## Status

- Excel files are read with calamine and written with rust_xlsxwriter instead of DuckDB's
  Excel extension, so typed cells, mixed columns and layout traps can be reported precisely and
  no extension download is needed.
- Generated TMDL follows Microsoft's published TMDL syntax; it has not been opened in Power BI
  Desktop as part of MAGI's tests.
