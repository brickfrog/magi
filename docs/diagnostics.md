# Diagnostic codes

Every diagnostic has a stable code. Messages point at the source text involved and, where
possible, suggest a fix (`did you mean ...?`). Diagnostics never print source data values;
they name rows, cells and counts instead.

| Range | Area |
|---|---|
| M0xx | syntax, names, declarations |
| M1xx | types and expressions |
| M2xx | sources |
| M3xx | reconciliation |
| M4xx | validation and run time |
| M5xx | exports |
| M6xx | BI models |
| M7xx | tests |

## Syntax, names, declarations

| Code | Severity | Meaning |
|---|---|---|
| M001 | error | Syntax error (unexpected token or character, unterminated string, chained comparison, ...). Parsing recovers at the next statement, even after an invalid character or unterminated string, so several errors can be reported at once. |
| M002 | error | Unknown relation, or a relation has no such output (`result.matchez`). |
| M003 | error | A name is defined twice (relation, column, tier, identity, validate block, model). DuckDB names are case-insensitive, so this includes names that differ only in letter case: two relations `Sales` and `sales`, a new, renamed or aggregated column `X` next to a column `x`, two source columns whose names differ only in case, or a `row_number:` column named like a source column. |
| M004 | error | Import cycle, or an imported file cannot be read. |
| M005 | warning | A mapping is never used (not reported when the program has errors: the statement using it may be one that failed). |
| M006 | error | A relation depends on itself. |
| M007 | error | Invalid or unknown option, source kind or connection kind, or an unsupported ODBC `auth:` value (only `integrated`); includes `section: 0` and an Excel `section` next to `range` or `header_row`. |
| M008 | warning | A password or a connection string containing one is written in the program; use `env("VAR")`. |
| M009 | error | Unknown connection. |
| M010 | error | A qualifier (`x.col`) names no input of this pipeline or reconciliation. |
| M011 | error | A column name is ambiguous (e.g. after a join); qualify it. |
| M012 | error | A column does not exist. |
| M013 | error | A name starts with `__magi` in any letter case (`__MAGI_N` too), which is reserved for MAGI's internal tables and columns: declared names, new and renamed columns, `row_number:` columns, and source columns (declared or read from the file). |
| M014 | error | A declared name (source, dataset, reconcile, mapping, connection) or a `row_number:` column contains `.`; `x.part` names an output of relation `x` (`x.rejects`, `r.matches`). |

## Types and expressions

| Code | Severity | Meaning |
|---|---|---|
| M101 | error | Unknown type or invalid type parameters in a schema, including a date/time format DuckDB cannot parse (`%Q`) or a format that reads a time zone (`%z`, `%Z`) for a type other than `timestamp_tz`. |
| M102 | error | A condition (`filter`, `require`, check, flag) is not a bool. |
| M103 | error | A check mixes aggregates with single-row columns. |
| M104 | error | A column is neither grouped nor aggregated (aggregate step, rollup tier). |
| M105 | error | Operator or comparison applied to incompatible types. |
| M106 | error | `case` / mapping branches have incompatible types. |
| M107 | error | Unknown function namespace (only `duckdb.` exists), DuckDB has no function of that name, or the function is an aggregate or table function (`duckdb.` calls only scalar functions and macros). |
| M108 | error | Unknown function, wrong arguments, invalid date literal, non-literal format, non-literal `regexp_extract` group. |
| M109 | error | Mapping errors: unknown mapping, duplicate or non-literal pattern, pattern type mismatch. |
| M110 | error | Aggregate in a place that does not allow one, nested aggregates, aggregate over the wrong side of a rollup. |
| M112 | note | A dataset has same-named columns from different inputs (ignoring case); they are renamed `<input>_<column>` (a column made inside the pipeline counts as the pipeline's input), with `_2`, `_3`, ... added if that name is taken. |
| M113 | error | `group by` not followed by `aggregate`, or grouping by an expression. |
| M114 | error | Invalid `native_sql` block. |
| M115 | warning | A dataset contains DuckDB-specific SQL (`native_sql`), or a `duckdb.*` function is used (reported once per function). |
| M116 | error | `drop` removes every column. |
| M117 | error | `sort` by an expression (derive it first). |
| M118 | error | `limit` without `sort` would keep arbitrary rows. |
| M119 | error | `union` inputs have different columns or incompatible types. |
| M120 | error | The same relation is joined twice without an alias. |
| M121 | warning | A join condition does not mention the joined relation (`on true` is exempt: it is an explicit cross join). |
| M122 | warning | `sum` / `mean` over float values: the parallel summation order is not fixed, so the last digits can differ between runs. Use decimals. |
| M123 | warning | A `duckdb.*` function returns different values on every call (e.g. `random`), or depends on when the run happens (e.g. `now`, `current_date`; `--today` does not pin it, use `today()`); results depending on it are not reproducible. |
| M124 | error | `derive`, `drop`, `rename`, `normalize`, `join` or `union` over a `native_sql` result without `schema`: its columns are unknown and would be dropped. Declare a `schema { ... }` or `select` the columns first. |

## Sources

| Code | Severity | Meaning |
|---|---|---|
| M200 | error | A source cannot be read or loaded (missing or empty file, driver error, unset environment variable, an ODBC `user`/`password` containing `;`, `{` or `}` next to a `connection_string`, a whole CSV file that mixes kinds of line breaks (CRLF, LF, CR), which the message says with the count of each; a fixed-width record of another length than the others or shorter than its layout, a field that is not valid text in the source's `encoding`, or a layout without `field`/`start`/`length` columns, with overlapping or duplicate fields or an unknown type code). Messages never name MAGI's temporary copy of a CSV section; they say `section N of <file>`. |
| M201 | warning / note | Something the reader noticed: mixed-type Excel column, ignored rows below the data, header that looks like a title block (also a title touching the table in an Excel `section:`), numbers stored as text (read as numbers next to number cells; a column of only numbers stored as text stays text, with the numeric type to declare), error cells, merged cells overlapping the table; CSV dates or times not in ISO form (with the format to declare), values that are not numbers or not booleans, codes with leading zeros, integers beyond 64 bits and times with more than 6 decimal places of seconds kept as text; a CSV `section: N` whose next section has the table's number of fields on every line (the table may continue past a blank or delimiter-only line). Notes about how a column's type was inferred are left out when the program declares that column's type. |
| M202 | error | A declared column is not in the source. |
| M203 | warning | A declared type conflicts with the type the source reports (for CSV and Excel: the type every value converts to exactly, including the integer digits a declared decimal needs), e.g. `int` over decimals, `date` over timestamps or `decimal(5,2)` over `123456.7`; values that do not convert exactly will be listed in `<source>.rejects`. |
| M204 | note | Columns of a SQL source are not checked (no declared schema, source not contacted). |
| M205 | error | An identity column does not exist. |
| M206 | warning | An identity column is declared (or known to be) nullable. |
| M207 | note | A database source has no identity and no stable row order. |
| M208 | warning | Values that could not be converted exactly to the column type became null (including an `int` with a fraction, a `date` with a time part other than midnight, a `time`/`timestamp` with a UTC offset, or any time with more than 6 decimal places of seconds); see `<source>.rejects`. |
| M209 | warning | Decimal values that the declared scale changes (more significant decimal places) were rounded. |
| M210 | error | A declared non-null column has missing values (for CSV and Excel, whitespace-only values are missing, `string` columns included); the run stops. |
| M211 | error | A source identity has duplicates or nulls (whitespace-only CSV and Excel values are null); the run stops. |
| M212 | error | A CSV file (or its `section:`) cannot be read as one table: lines with different numbers of fields (with or without a header, including one extra empty field at the end), leading lines DuckDB would skip, or a single column named after a whole line; with `ragged: true`, lines with more fields than the header. A whole file that blank lines (empty, whitespace-only or delimiter-only lines) split into sections of different shapes (a preamble, the table, a trailer) is explained by its sections, with their lines, field counts and delimiter, and `section:` suggested; sections that all start with the table's number of fields are one table, whose misshapen lines are named. Also a `section:` whose end depends on whether `\"` escapes a quote inside a quoted field. The message names line numbers of the file only. |
| M213 | error | A `section: N` beyond the sections of a CSV file (runs of non-blank lines; the message names the delimiter fields were split at) or of an Excel sheet (runs of non-blank rows); the message lists the sections there are, with their lines or rows. |
| M214 | error | `fill_down` or `row_number` on a source other than CSV, Excel or fixed-width (Parquet, DuckDB, SQL): only those are read in file order. |
| M215 | error | A `fill_down` column is not in the source. |

## Reconciliation

| Code | Severity | Meaning |
|---|---|---|
| M300 | error | Invalid inputs (unknown columns, both sides with the same name). |
| M301 | error | Invalid clause or policy value, missing tiers, flag without condition at reconcile level. |
| M302 | error | A blocking key does not compare one input with the other. |
| M303 | error | On a grouped side, blocking keys must be group keys. |
| M304 | error | Invalid rollup shape (`many a` without `group a by`, grouping a `one` side, ...). |
| M305 | warning | A tier has no blocking keys and no `block` clause (`block by none`, in the tier or for the reconciliation, marks comparing every pair as intended), or a tier accepts any pair (no blocking keys and no `require`). |
| M306 | warning | A side has no row identity; synthetic row numbers are used. |
| M307 | error | Column name conflicts in reconciliation outputs, or evidence defined inconsistently across tiers. |
| M308 | error | A rank key cannot be ordered. |
| M310 | error | At run time: a declared identity does not identify rows (duplicates or nulls). |
| M311 | note | With `consume none`, `a` or `b`, rows stay available to later tiers, so a row can be matched once in each tier (a pair already matched is not matched again). |
| M312 | warning | `rank by` has no effect with `cardinality many_to_many` (every candidate is matched). |
| M313 | error | Invalid subset tier: `max_items` not from 1 to 16, `max_subsets` not positive, a second `subset` clause, a side that is not `a` or `b`, a grouped subset side, a shape line that disagrees with `subset` and `group` (`one a to many b`, or `many a to many b` when `a` is grouped, for `subset b`), or a cardinality other than `one_to_one`. |
| M314 | error | In a subset tier, a column of the subset side is read outside an aggregate where the value must be one per subset: a `require` that also aggregates over the subset side (split the per-row part into its own `require`), `rank by`, tier evidence or a tier flag. |
| M315 | error | At run time: a subset tier has more subsets than its `max_subsets`. Without a sum target, the run counts them before enumerating (the sum over units of C(n, 1) + ... + C(n, max_items) for n members) and stops before enumerating. With a subset-level `sum(x) == t` (exact `x`), it counts the subsets each size examines while enumerating and stops as soon as the total exceeds the limit. Names the tier, the number of units and the largest member count; writes no output. |

## Validation and run time

| Code | Severity | Meaning |
|---|---|---|
| M400 | error | A step failed while executing, or an internal consistency check failed. DuckDB errors about values (conversion, invalid input, out of range, constraint, CSV) keep only the text before the first quoted value or number, followed by `(details withheld: they quote data)`; other DuckDB errors (binder, catalog, parser) are shown in full, except CSV `Original Line:` echoes. |
| M401 | note | A `not null` check cannot fail because the expression is never null. |
| M402 | error | A `require` check failed (an aggregate check also fails when its aggregate is null: no rows, or only nulls). |
| M403 | error | An `expect` check failed (outputs are still written; the run exits with an error). |
| M404 | warning | A `warn` check failed. |
| M405 | error | The run stopped because a `require` check failed (use `--keep-going` to write outputs anyway). |

## Exports

| Code | Severity | Meaning |
|---|---|---|
| M501 | error | Unknown output format (file extension). |
| M502 | error | Several relations in a format that holds one, or wrong entry kind (`sheet` vs `table`). |
| M503 | error | Invalid or duplicate sheet/table name. |
| M504 | error | Two exports write the same file (paths compared after resolving `.`, `..` and existing links). |
| M505 | error | An output file cannot be written (no output file is changed). |
| M506 | warning | An XLSX export wrote values as text because an Excel cell cannot hold them exactly (integers beyond ±2^53, e.g. UBIGINT/HUGEINT, decimals with more than 15 significant digits, times and timestamps finer than a millisecond); names the column and count. |

## BI models

| Code | Severity | Meaning |
|---|---|---|
| M601 | error | A model table is not exported to a Parquet file Power BI can import (Parquet keeps column types and nulls). |
| M602 | error | Invalid relationship (unqualified column, same table on both ends, key types differ), or a relationship declared twice. |
| M603 | error | A dimension name collides with another column of its table, a metric has the same name as a column anywhere in the model, or a metric is defined twice (names compared ignoring case, as in Power BI). |
| M604 | error | A metric has no `value`, reads no table, or cannot be expressed as an equivalent DAX measure (e.g. `min`/`max` over a boolean column, which DAX rejects). |
| M605 | warning | A relationship would add a second active filter path between two tables: a second relationship between the same tables, a triangle such as sales→customers→regions next to sales→regions, or a cycle. Power BI allows one active path, so relationships are activated in declaration order and later ones are written inactive (use `USERELATIONSHIP` in DAX for a second relationship between the same tables). |
| M606 | error | A model table has a column whose type MAGI does not know (e.g. the result of a `duckdb.*` call); Power BI needs every column's type. Cast it in the dataset (`to_float`, `to_decimal`, `to_int`, `to_string`, `to_date`, `to_bool`). |
| M607 | note | A metric uses `count_distinct`, `min` or `max` over a text column. Power BI (VertiPaq) compares text ignoring case, so values that differ only in case ("abc", "ABC") are one value there and two in MAGI; normalise them in the dataset (e.g. `lower`) if the measure must match. |

## Tests

| Code | Severity | Meaning |
|---|---|---|
| M701 | error | Two tests of a program have the same name (test names are their own namespace, compared exactly). `magi test` runs none of the file's tests. |
| M702 | error | A test's `given` names no source of the program, gives the same source twice, or gives a path for a `sql(...)` source (give a whole declaration: `given x = csv("...")`). |
| M703 | error | A test's `today:` is not a date written `YYYY-MM-DD`, or is given twice. |
| M704 | error | An `expect` names a relation the program does not have. |
| M705 | error | An `expect`'s file cannot be read: missing, empty (no header line), an empty header name, a line with another number of fields than the header, or invalid quoting. |
| M706 | error | An `expect`'s file names a column the relation does not have, or one column twice (names compared ignoring case). |
| M707 | error | A test's program still reads a `sql(...)` source; tests read only files, so replace it with `given x = csv("...")`. |
