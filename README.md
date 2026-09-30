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
magi fmt     analysis.magi              # canonical formatting
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

## Language

Statements may appear in any order; names are global to a program and its imports. Keywords
are contextual, so columns may be called `type`, `group` or `sort`; any other name can be
written in backticks (`` `Unit price` ``). Names are case-insensitive, as in DuckDB: `Sales`
and `sales` are the same name (M003). Declared names cannot contain `.` (M014), and names
starting with `__magi` in any case are reserved (M013). Comments start with `#`.

### Sources and source contracts

```magi
source cases = csv("cases.csv") {
    delimiter: ";"            # default: sniffed
    header: true              # false: columns are named column_1, column_2, ... (as for Excel)
    all_text: false           # true: every column is text; parse explicitly
    section: 2                # read only the 2nd run of non-blank lines (skips a preamble/trailer)
    ragged: false             # true: lines with fewer fields are padded with nulls
    fill_down: region         # or [region, office]: a blank takes the value above it
    row_number: row_no        # adds `row_no: int`, the 1-based data row
    identity case_id          # or identity [case_id, event_date]
    schema {                  # declared schema (source contract)
        case_id: string
        event_date: date("%d/%m/%Y")?     # parse text with these formats
        amount: decimal(18, 2)?
    }
}

source claims  = excel("claims.xlsx") { sheet: "Data"  range: "A3:L1184" }   # or header_row: 3, section: 2
source roll    = fixed_width("roll.txt") { layout: "roll_layout.csv"  encoding: "cp1252"  record: "D" }
source history = parquet("history.parquet")
source ledger  = duckdb("ledger.duckdb") { table: "entries" }                # or query: """..."""

connection warehouse = odbc { connection_string: env("MAGI_WAREHOUSE") }    # or dsn:, user:, password:
source offices = sql(warehouse) {
    query: """
        SELECT office_id, state, region FROM dim_office
    """
    identity office_id
}
```

Types: `bool int decimal(p,s) float string date time timestamp timestamp_tz binary json`; a
trailing `?` means nullable. Date, time and timestamp columns may carry parse formats; the
declared formats are tried first, and ISO text is always accepted too.

Every source is staged in two steps: the raw rows (as text for CSV, Excel and ODBC), then the
typed relation. A declared type wins over what the source reports, and MAGI warns when they
conflict (M203; for CSV the reported type comes from every value). Values that do not convert
exactly become null **and are listed** in `<source>.rejects` (row, column, expected type, raw
value) with warning M208: an `int` rejects fractions, a `date` rejects a time part other than
midnight, a `time` or `timestamp` rejects text with a UTC offset (declare `timestamp_tz`; a format
reading `%z`/`%Z` is accepted only for `timestamp_tz`, M101) and any time with more than 6 decimal
places of seconds — nothing is rounded or cut silently. A trailing `;` in a `query:` is ignored.
A declared non-null column that ends up null stops the run (M210);
decimal values that the declared scale changes are reported (M209); a declared identity must be
unique and present (M211), so its columns are never null in the source relation.

CSV column types come from every value, not a sample: plain decimals become `decimal`, only
exponent notation becomes `float`; codes with leading zeros and integers beyond 64 bits stay
text; booleans, dates, times and timestamps are inferred only when every value is one (dates
not in ISO form stay text with a note naming the format to declare; times with more than 6
decimal places of seconds stay text) (M201); a column DuckDB's sample showed as text or blanks
still gets the type all its values share. A file that is not one table — lines with different
numbers of fields (with or without `header: false`), or a delimiter DuckDB cannot find — is an
error naming the lines (M212). File paths are literal: `*`, `?` and `[` are not patterns. Where
MAGI itself splits lines into fields (sections, field counts in messages), it uses the declared
`delimiter:`, else the one of `,` `;` tab `|` that gives the most lines of the file's first
megabyte the same number of fields, so a title line with stray commas does not decide it.

Bank and ERP exports often wrap the table in other lines. A *section* is a run of non-blank lines,
numbered from 1; a blank line is empty or holds only whitespace, or only delimiters and whitespace
(`,,,,`, what a spreadsheet writes for an empty row). Lines end in LF, CRLF or a lone CR (classic
Mac files), as DuckDB reads them. Quoting follows DuckDB's reader: `"` starts
a quoted field only as the first character of a field (`PIPE 12" STEEL` is plain text), and a
line break inside a quoted field does not end a line. A file that writes quotes inside quoted
fields as `\"` instead of `""` is read the same way unless that changes where the chosen section
ends (a `\"` followed by a blank line inside the value): then `section:` is refused (M212)
rather than guessed. `section: N` reads only section N as the
CSV, with everything above (a header line, type inference, rejects) applying to it. When section
N+1 has exactly the table's number of fields on every line, MAGI warns that the table may
continue there (M201): an emptied record (`,,,`) inside a table ends its section. MAGI copies
the section to a temporary file for DuckDB, with its line breaks made LF (so a file that mixes
CRLF, LF and CR lines reads fine; breaks inside quoted fields are kept). The copy is made once per
run, in a new directory with a random name (`__magi_scratch-` and 12 letters or digits) in the
system's temporary directory (`TMPDIR`), readable only by its owner (0700, copies 0600), and
removed when the command ends. A run that is killed leaves its directory behind; the next MAGI
command that copies a section removes such directories, but only ones with that exact name,
owned by the same user, more than a minute old, holding only MAGI's files, and whose lock file no
running MAGI process holds (each run keeps an exclusive lock on its directory's `magi.lock`).
A whole file that fails the one-table check and whose sections differ in shape (a preamble or
trailer with other field counts than the table) gets an M212 listing them (lines, field counts,
delimiter) and suggesting `section:`; when every section starts with the table's number of
fields it is one table with a blank or emptied line in it, and the M212 names the misshapen
lines as for any file. A section the file does not have is M213 (naming the delimiter the
fields were split at); a whole file DuckDB cannot read because it mixes kinds of line breaks
gets an M200 saying so. Line numbers in messages are always lines of the file, and messages
never name the temporary copy; row numbers (`<source>.rejects`, `failures.source_row`,
`row_number:`) count the section's data rows from 1. `magi sql` says which lines a section
reads, but its `read_csv('<section N of file>')` names the temporary copy, so that SQL does not
run as printed. `ragged: true` pads lines that have fewer fields than the header (with
`header: false`: than the widest line) with nulls; a line with more fields than the header is
still M212. A preamble or trailer of `key:,value` lines reads as
`section: 1  header: false  ragged: true`, with columns `column_1`, `column_2`, ...

A blank value, in CSV as in Excel, is empty or holds only Unicode whitespace (tab, no-break space
and the like, not only spaces): it is null when typed, in `string` columns too, so a
whitespace-only identity is missing (M211) and a declared non-null column fails (M210); `fill_down`
fills it, and an Excel cell holding only whitespace counts as empty for type inference and for
finding blank rows. Numbers, dates and other typed values are read without the whitespace around
them; other `string` values are kept exactly as written (`A1` and `A1 ` are different values).

CSV, Excel and fixed-width sources are read in file order, so they also take two row options
(other sources reject them, M214). `fill_down: col` (or `[a, b]`) gives a blank value (empty,
null or whitespace-only) the nearest non-blank value above it in the same column, for account or
group labels printed once per block; it applies to the raw text before typing, so declared
types, rejects, non-null and identity checks see the filled values, and blanks above the first
value stay null. A `fill_down` column the source does not have is M215. `row_number: name` adds
a non-null `int` column numbering the data rows 1, 2, 3, ... in file order (blank rows an Excel
`range` skips and records `record:` leaves out are not counted); for CSV it is the row
`<source>.rejects` cites, while Excel rejects cite worksheet rows and fixed-width rejects file
lines. Its name follows the column name rules (M003, M013, M014).

Excel means rectangular tables in `.xlsx` worksheets. MAGI reads cells with their Excel types
(dates stay dates, not serial numbers) and infers column types from **all** rows. A column
that mixes kinds (dates and text, numbers and text) is read as text with a warning naming the
cells; numbers stored as text keep their exact text (a column holding only such cells stays
`string`, with a note naming the numeric type to declare; next to number cells they are read as
numbers); data ends at the first blank row (all cells empty or whitespace-only), and
non-empty rows below it are reported; a header row that looks like a title block is reported
with the likely `header_row`/`range`; merged cells overlapping the table are reported (M201).
A column with a declared type gets none of the notes about its inferred type: the declaration
decides how its cells are read, and cells that do not convert are listed in the rejects (M208).
MAGI does not guess layouts. Instead of `range`/`header_row` (M007 next to either), an Excel
source can say `section: N`: sections are runs of non-blank rows of the sheet (a row is blank
when its cells are all empty or whitespace-only), and section N is the table, its first row the
header, its columns those with a value in it. It follows the table as a monthly export grows
(`range: "A6:P1541"` would cut it), and rows above and below it (a title block, a report total)
are not read or reported; a title row touching the table (no blank row between them) is part of
the section, and is reported like a title in a table without `range` (M201); a section the sheet
does not have is M213, listing the sections with their rows.

`fixed_width(path)` reads a mainframe-style export: one record per line (LF or CRLF), each field
at fixed byte positions given by `layout:`, a CSV with the columns `field`, `start` (1-based
byte), `length` (bytes) and optionally `type`: `A` text (the default; the padding blanks around
it are removed), `N` a whole number (`000123`, read as `int`), `D` a `YYYYMMDD` date
(`00000000` is none), `M` cents with the decimal point implied (`01500` is `15.00`, a decimal
with 2 places). A value that does not fit its type is listed in the rejects (M208), and a
declared type decides how a field is read (`tag: string` keeps `000123`). Fields are cut from
the record's bytes before they are decoded with `encoding:` (UTF-8 by default, or a single-byte
encoding such as `cp1252` or `latin1`), so accented letters never shift a position.
`record: "D"` keeps only the records starting with that code, leaving out header and trailer
records; read a trailer with its own layout (`record: "T"`) to check its record count. Every
kept record must have the same length and reach the layout's last byte, and every field must
be valid text in the encoding: otherwise the source cannot be read (M200, naming the line).
Rejects and `failures.source_row` cite file lines. Overlapping or duplicate fields and unknown
type codes in the layout are M200 too.

SQL sources are contacted only when a command gets `--sources` (`magi run` always contacts
them); without it, commands use the declared schema (or leave the columns unchecked, M204, and
`magi sql` / `explain` mark their output incomplete). Credentials come from `env("NAME")`;
plans, SQL output and diagnostics never contain them, and ODBC driver messages are redacted.
With `dsn:`, `user:` and `password:` go to the driver as separate `SQLConnect` arguments; next
to a `connection_string:` they are appended as `UID=`/`PWD=`, and values containing `;`, `{` or
`}` are refused (M200) because drivers disagree on quoting. `auth:` accepts only `integrated`
(the driver's own sign-in).

### Datasets

```magi
dataset cleaned = raw
    |> select id, amount, description, day = parse_date(event_date, "%Y-%m-%d")
    |> drop description
    |> rename amount -> amount_kg
    |> derive { year = year(day)  big = amount_kg > 100 }
    |> filter amount_kg > 0 and day is not null
    |> left join offices as o on office_id           # or: on o.office_id == cleaned.office
    |> normalize category { "Gun" => "Firearms"  ["Narcotic", "Drug"] => "Drugs"  otherwise => original }
    |> group by year
    |> aggregate { rows = count()  total = sum(amount_kg) }
    |> sort total desc
    |> limit 10
```

Steps: `select`, `drop`, `rename`, `derive`, `filter`, `[left|right|full] join`, `group by` +
`aggregate`, `sort`, `distinct`, `union` (append, matched by column name), `limit`,
`normalize col { ... }` / `normalize col with mapping`. After a join, a column that exists on
both inputs must be qualified (`clean_a.amount`); in the output, same-named columns (ignoring
case) are renamed `<input>_<column>` (M112; a column made inside the pipeline counts as the
pipeline's input, and `_2`, `_3`, ... is added if that name is taken). A right join's merged key
is the right key; a full join's merged key is null when either key is. `join x on true` is a
cross join (every row with every row, e.g. to attach a one-row dataset of totals); any other
condition that never mentions `x` is reported (M121). `limit` requires a preceding `sort` (M118).

Reusable mappings:

```magi
mapping firearm_types {
    ["FIREARM", "FIREARMS", "GUN"] => "Firearms"
    otherwise => original          # without `otherwise`, unmapped values become null
}
dataset x = raw |> normalize kind with firearm_types |> derive label = map(code, firearm_types)
```

`import "common/categories.magi"` includes another file (paths are relative to the importing
file; cycles are errors).

`native_sql { dialect: duckdb  query: """..."""  inputs: [a, b] }` is an escape hatch for
DuckDB SQL; it is flagged as DuckDB-specific (M115) and MAGI cannot check or trace inside it.
Without `schema { ... }` its columns are unknown, so steps that keep every column (`derive`,
`drop`, `rename`, `normalize`, `join`, `union`) are rejected (M124); `select` the columns first
or declare the schema.

### Expressions and functions

Operators: `+ - * / %`, `== != < <= > >=`, `and or not`, `x is [not] null`, `x [not] in [..]`,
`case { cond => value ... otherwise => value }`. Comparisons do not chain. Text is joined with
`concat(...)`, not `+`. Date literals are `date("2026-01-31")`. Decimal literals stay exact
(`amount * 0.001`); `/` yields a float.

| Group | Functions |
|---|---|
| text | `lower upper trim strip_accents replace concat contains starts_with ends_with length substr lpad rpad regexp_replace regexp_extract regexp_matches replace_words(text, mapping)` |
| logic | `coalesce if nullif is_null least greatest` |
| numbers | `abs round floor ceil parse_number to_decimal(x, p, s) to_int to_float to_string to_bool` |
| dates | `date today year month day quarter day_of_week days_between date_diff add_days parse_date parse_timestamp to_date` |
| similarity | `similarity` (Jaro-Winkler on case/space-normalised text), `token_similarity` (word-set Jaccard), `levenshtein`; `similarity` and `token_similarity` are null when either text is blank |
| mapping | `map(value, mapping)` |
| aggregates | `sum mean min max count count_distinct missing any all` |

`days_between(a, b)` is the absolute number of days; `date_diff(a, b)` is signed (`b - a`).
`parse_number` accepts `1,234.5` thousands separators and returns `decimal(38, 6)` (up to 32
integer digits; more than 6 decimal places are rounded); anything else unreadable is null.
`missing(x)` is the fraction of rows where `x` is null or blank. `today()` makes results depend
on the run date; `magi plan` says so and `--today YYYY-MM-DD` (accepted by `run`, `check`,
`plan`, `sql`, `explain`, `schema`, `trace`) pins it. DuckDB-only scalar functions are available
as `duckdb.name(...)`: `magi check` looks the name up in DuckDB's catalog (M107; aggregate and
table functions are rejected, use MAGI's aggregates), warns that the dataset is DuckDB-specific
(M115), and warns that a volatile function such as `random`, or a function that depends on when
the run happens such as `now` or `current_date` (not pinned by `--today`; use `today()`), makes
results irreproducible (M123); `magi plan` lists the run-dependent ones. Arguments and results
are not type checked.

MAGI checks types and nullability: `amount > "10"` is an error with a hint, and a
`require x not null` check on a column that can never be null is reported (M401).

### Validation

```magi
validate cleaned {
    require id not null
    require amount >= 0
    expect unique(id, event_date)
    warn missing(description) < 0.05 as "descriptions mostly present"
}
```

A row check fails for rows where the condition is false (null passes, as in SQL CHECK
constraints; use `not null` for presence). A check with aggregates is evaluated once for the
relation; it fails when the aggregate is null (no rows, or only nulls), with `measured` empty.
`require` failures stop the run before any export (M402/M405; `--keep-going` writes
outputs anyway), `expect` failures make the run exit with an error but still write outputs
(M403), `warn` failures are warnings (M404). Failing rows are in `cleaned.failures`, per-check
results in `cleaned.checks` (with `measured`: the compared aggregate of an aggregate check, e.g.
`0.25` for `missing(description) < 0.05`); both can be exported like any relation. When the
validated relation is a source, each failing row carries `source_row`, its data row in the file
or sheet (counted from 1, as in `<source>.rejects`). A dataset's failing rows carry the dataset's
columns — include an identity column (or validate the source) to trace them back.

### Reconciliation

```magi
reconcile result = source_a as a with source_b as b {
    block by organization_id          # candidates share these keys (or a.x == b.y; or none)
    cardinality one_to_one            # one_to_many | many_to_one | many_to_many
    consume both                      # a | b | none: which side's matched rows leave later tiers
    ambiguity hold                    # continue: ambiguous rows stay available to later tiers
    duplicates hold                   # continue: see below
    identity a: case_id               # else the source identity, else synthetic row numbers

    evidence {
        date_gap = date_diff(a.date, b.date)
        desc_sim = similarity(a.description, b.description)
    }
    flag late when b.date > a.date

    tier exact {
        require a.date == b.date
        require a.amount == b.amount
        rank by desc_sim desc
    }

    tier strong {
        require days_between(a.date, b.date) <= 2
        require a.amount == b.amount
        rank by days_between(a.date, b.date), desc_sim desc
    }

    tier rollup {
        many a to one b
        group a by organization_id, date, category
        require sum(a.amount) == b.amount
        evidence { a_total = sum(a.amount) }
    }

    tier likely {
        require desc_sim >= 0.9
        flag needs_review
    }
}
```

Tiers run in order. Each tier builds **candidates** — pairs of units (rows, or groups in
rollup tiers) that share the blocking keys and satisfy every `require` — and selects matches.
Reconcile-level evidence can be used by name inside tiers. A tier without blocking keys compares
every remaining pair; MAGI warns about it (M305) unless `block by none` (in the tier or for the
reconciliation) says that this is intended.

Selection (`one_to_one`) is deterministic and never breaks a tie by storage order:

1. Rows equal on every column the policy looks at outside aggregates are interchangeable (a
   *class*). A column read only inside aggregates (`count(b.ref)` in a rollup or subset tier)
   concerns groups and subsets, not single rows, so it does not split a class — except with
   `consume none`: there a matched pair is never matched again, so which row paired with which
   decides what later tiers can do, and such a column splits classes like any other.
2. Two classes match when each is the other's unique best candidate by `rank by`; their rows
   pair in identity order. Rounds repeat until nothing changes.
3. If the classes have different numbers of rows, the leftovers are either exact duplicates
   (equal in every column but identity) — reported with `match_status = 'duplicate'` and
   `duplicate_of` — or distinguishable rows, in which case choosing would be arbitrary and the
   whole pair of classes is reported as **ambiguous**.
4. A class whose best candidates are tied across several classes, each of which has that class
   as its unique best candidate (a *star*), is decided in the same step when choosing is not
   arbitrary: with as many rows on both sides all rows match (the class's rows are
   interchangeable, so any pairing is equivalent; rows pair in identity order); if one side has
   more rows and they are all exact duplicates of each other, the leftovers are `duplicate`s of
   that side's first row, as in 3. Two identical bank lines against two equal receipts from
   different customers match; one bank line against a receipt posted twice matches once plus one
   duplicate; one bank line against receipts from two customers stays ambiguous.
5. When nothing is left to decide that way, a class whose best candidates are tied is reported as
   ambiguous with those candidates: ambiguity is data, not a guess.

Rollups compare explicitly defined groups (`group a by ...`). Every group is a unit, a group of
one row included; rows whose group key is null belong to no group. `many a to many b` groups
both sides. MAGI searches combinations of rows only in subset tiers, and only within a stated
bound.

**Subset tiers** match a unit of one side with a combination of up to `max_items` (1 to 16)
rows of the other side when no column says which rows belong together, such as a bank payout
that settles several sales lines, or a GL deposit that the bank recorded as two lines:

```magi
tier payouts {
    subset b max_items 8                # optional: max_subsets 2000000 (the default)
    require date_diff(b.date, a.date) >= 1 and date_diff(b.date, a.date) <= 3
    require sum(b.amount) == a.amount
    rank by count(b.id)                 # prefer fewer lines
    evidence { lines = count(b.id) }
}
```

The units of the other side are its remaining rows, or its groups with `group a by ...` as in
rollup tiers; the subset side cannot be grouped. A shape line is optional; if written, it is
`one a to many b` (`many a to many b` when `a` is grouped). A `require` without aggregates over
the subset side is checked, like the blocking keys, for each *member*: a pair of a unit and a
remaining row of the subset side. A `require` with such aggregates is checked for each subset,
over the subset's rows; it can read subset-side columns only inside aggregates (M314), and so
can `rank by`, tier evidence and flags. A unit's *candidates* are the subsets of 1 to
`max_items` of its members that pass every subset-level `require`, and its best candidates are
those ranked first by `rank by` (all of them without `rank by`). A unit with exactly one best
subset matches it, unless a row of that subset is also needed by a best subset of another
unit: then every unit involved is ambiguous, as is a unit with several best subsets. A row that
is only in lower-ranked candidates of other units causes no conflict.

Rows that are exact duplicates of each other are *copies*: equal in every column but identity,
and on any identity column the tier reads, except in `count(b.col)` and, for a one-column
identity, `count_distinct(b.id)`. Subsets of one unit that differ only by exchanging copies
count as one candidate. Copies are shared out: the units whose best subsets use copies of a row
take them in unit order, each as many as its best subset needs (copies not matched by an
earlier tier first, then in identity order), so a 300 = 100 + 200 payout and a 100 receipt
both match when the 100 was posted twice. Only when the units need more copies than there are
do they conflict; each of them is then ambiguous with every copy. When a unit matches, copies
that no unit took and no earlier tier matched are reported as `duplicate` of a matched copy,
and `duplicates hold|continue` applies as in other tiers: a payout of 100 + 200 where the 100
was posted twice matches one posting, and the other is a duplicate. With `consume none`, a row
matched by an earlier tier is no copy of other rows.

Matches and ambiguities are recorded per row pair. On the subset side, `a_group`/`b_group`
identifies the subset (candidates are numbered in a fixed order of unit and members); on a
grouped side, it identifies the group. In subset tiers `candidates_a`/`candidates_b` count
candidate subsets: for the unit, how many it has, and for a row of the subset side, how many
contain it. `.candidates` lists each row pair of the candidate subsets once. Subset tiers need
`cardinality one_to_one`.

Rows that add nothing to a subset-level condition can be added to any candidate: with
`require sum(b.amount) == a.amount`, lines of 50 and -50 (or of 0) give a second, larger
candidate for every subset that matches without them, so the unit's best subsets tie and it is
ambiguous. `rank by count(b.id)` prefers the smallest subset in that case.

Without a sum target (below), `magi run` counts the subsets before it enumerates: a unit with n
members has at most C(n, 1) + ... + C(n, max_items). If all units together have more than
`max_subsets`, the run stops (M315) and names the tier, the number of units and the largest
member count. Blocking keys and member-level `require`s make the units smaller; a lower
`max_items` makes the combinations fewer. `magi plan` shows the bound and the largest unit that
fits in it.

With a subset-level `require` that contains `sum(x) == t` (alone or joined by `and`, `x` an int
or decimal, `t` not read from the subset side), the run builds subsets one size at a time and
extends a subset only while the members after its last one (their positive and negative values)
can still bring its sum to `t`. No candidate is lost: a subset that reaches `t` keeps every
smaller subset it grows from. Before each size the run adds the subsets that size examines to a
total and stops (M315) as soon as the total exceeds `max_subsets`. `magi run` prints the counts
per tier. For example, 25 card payouts (the largest with 20 candidate sales, `max_items 10`)
examine 323,014 subsets and keep 86,305, where the upfront bound is 1,620,057.

Each `one_to_one` round costs a pass over the tier's candidates; data where every row's best
candidate prefers someone else (a preference chain) needs one round per link. `magi run` prints
the number of rounds per tier.

With `consume both` a matched row leaves later tiers. With `consume a` (or `b`) only that side's
matched rows leave; with `consume none` every row stays available, so a row can be matched once
in each tier (M311). A pair matched once is never matched again; in a rollup tier a group is not
a candidate for a row it already matched through any of its rows. `many_to_one` / `one_to_many`
let the limited side pick its best counterpart; an exact duplicate it passes over is reported as
`duplicate`, unless another row picks it. `many_to_many` matches every candidate (`rank by` has
no effect, M312).

`ambiguity` and `duplicates` say what happens to rows a tier did not match:

- `ambiguity hold` (the default): the rows of an ambiguity — in `one_to_one` every remaining row
  of the tied classes on both sides, in `many_to_one` / `one_to_many` the row that could not
  choose — are withheld from later tiers, whatever `consume` says, and end as ambiguous.
  `ambiguity continue`: they are still listed in `.ambiguous` but stay available to later tiers
  (not to the rest of the same tier); a row a later tier matches ends as matched, the others as
  ambiguous.
- `duplicates hold` (the default): an exact duplicate left over by a decision — the surplus rows
  of a class pair or star, a row the chooser passed over, a copy a subset left out — is withheld
  from later tiers and later rounds of the tier, and ends as `duplicate` with `duplicate_of`.
  `duplicates continue`: it stays available (in `one_to_one` also to later rounds of the same
  tier); if something matches it later it ends as matched, otherwise as `duplicate`.

On a side that is not consumed, a row an earlier tier matched stays available, but it is never
surplus: among interchangeable rows, the rows no tier matched yet are paired (or chosen) first,
and an already matched row left over is neither withheld nor reported as a duplicate.

Outputs (all exportable):

| Relation | Content |
|---|---|
| `result.matches` | one row per matched row pair: `tier`, identities, evidence, flags (one column each plus a `flags` summary), `candidates_a/_b` (how many candidates each side had; in subset tiers, candidate subsets, see above), `candidate_rank` (the pair's rank among the A unit's candidates) and `rank_values` (the `rank by` values in words), `reason` (the tier's policy in words), then both rows' columns prefixed `a_` / `b_` |
| `result.ambiguous` | every tied pair with an `ambiguity_id` per decision |
| `result.unmatched_a`, `result.unmatched_b` | rows neither matched nor in an ambiguity; `match_status` is `unmatched` or `duplicate` |
| `result.candidates` | every candidate pair considered, with its `outcome`, `candidate_rank` and `rank_values` |
| `result.summary` | `position`, then links and rows per tier; for `ambiguous` the distinct tied pairs and the rows whose final status is ambiguous; then duplicate, unmatched and total rows |

After every run MAGI verifies that each input row has exactly one final status: matched,
ambiguous (in an ambiguity and not matched), duplicate or unmatched, and that no row withheld
as a duplicate is matched in any tier. With `cardinality one_to_one` and `consume both` it also
verifies that a matched row is in exactly one pair or rollup group and never held.

A reconciliation needs row identity to trace results back to sources. Without a declared
identity MAGI numbers rows by their content (reproducible for identical input, not stable across
refreshes) and warns (M306).

### Exports

```magi
export summary to "out/summary.parquet"          # .csv .parquet .json .jsonl .xlsx .duckdb
export summary to "out/summary.xlsx" { sheet: "Summary" }
export to "out/report.xlsx" {
    sheet "Matches" = result.matches
    sheet "Unmatched A" = result.unmatched_a
}
export to "out/model.duckdb" { table "matches" = result.matches }
```

Rows are written in a total order — the relation's `sort` keys, then every column — so the same
program on the same inputs writes byte-identical files, as long as no float `sum` / `mean` is
involved (DuckDB adds floats in parallel, so the last digits can change; `magi check` warns,
M122 — use decimals). Files are written under unique temporary names and moved into place only
after every step has succeeded, all together or none: a failed run changes no output file. Two
exports naming the same file (also via `./` or `..`) are an error (M504). In `.xlsx` files,
numbers an Excel number cannot hold exactly (integers beyond ±2^53, decimals with more than 15
significant digits) are written as text, with warning M506.

### BI models (Power BI)

```magi
export sales to "out/sales.parquet"
export customers to "out/customers.parquet"

model sales_model {
    table sales { description: "One row per sale" }
    relationship sales.customer_id -> customers.id       # many side -> one side
    dimension region = customers.region { description: "Sales region" }

    metric revenue {
        value: sum(sales.amount)
        format: currency                                  # percent, integer, decimal or a format string
        description: "Recognized sales revenue"
    }
    metric average_price {
        value: sum(sales.amount) / sum(sales.qty)
    }
}
```

`magi compile FILE --target tmdl [--out DIR] [--data-folder PATH]` writes
`<DIR>/<model>.SemanticModel/` in Power BI's project format (TMDL): `database.tmdl`,
`model.tmdl`, `relationships.tmdl`, one file per table, and a `DataFolder` parameter (by default
the absolute path of the exported files on this machine; `--data-folder` sets another).
Recompiling replaces the whole `definition/` folder, so tables and relationships removed from the
program disappear; other files Power BI Desktop keeps in the `.SemanticModel` folder (`.pbi/`,
`diagramLayout.json`, `.platform`) are left alone. Tables
import the Parquet files the program exports (a model table must be exported to Parquet, M601:
Parquet keeps column types and nulls), so Power BI shows exactly what `magi run` produced.
Columns get Power BI types; foreign keys on the many side of a relationship are hidden;
`dimension` renames and describes a column, and measures refer to columns by those names.
Relationships are named `<from_table>.<from_col> to <to_table>.<to_col>`. Power BI allows one
active filter path between two tables, so relationships are activated in declaration order, and
one that would add a second path (a second relationship between the same tables, a triangle
such as sales→customers→regions next to sales→regions, or a cycle) is written inactive (M605). A
metric name must differ from every column and every other metric, ignoring case (M603). Every
column of a model table needs a type MAGI knows: cast `duckdb.*` results in the dataset (M606).

Metrics become DAX measures only when the meaning is the same in both engines: `sum`, `mean`,
`min`, `max`, `count()` (`COUNTROWS`), `count(col)` (`COUNTA`: non-null values),
`count_distinct` (`DISTINCTCOUNTNOBLANK`: ignores nulls, as SQL does) over table columns,
combined with `+ - *` and `/` (`DIVIDE`, blank on division by zero). Anything else — `missing`,
expressions inside aggregates, row-level columns — is rejected (M604) rather than translated
approximately. In Power BI a measure is evaluated in the report's filter context, so its value
for a slice equals the MAGI aggregate over the same rows, with one exception: Power BI compares
text ignoring case, so `count_distinct`, `min` and `max` over text treat "abc" and "ABC" as one
value where MAGI sees two (note M607; normalise with `lower` in the dataset if they must match).
`min` and `max` over boolean columns are rejected (M604), because DAX does not accept them. Date
and timestamp measures are formatted as dates. Decimal columns become `double` when Power BI's
fixed decimal cannot hold them (more than 4 decimal places, or more than 14 integer digits).

### Runtime

```magi
runtime {
    threads: 4
    memory_limit: "8GB"
    temp_storage: memory          # or a directory DuckDB may spill to
}
```

All intermediate data lives in an in-memory DuckDB database for the duration of `magi run`
and is discarded afterwards.

## How it works

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

Diagnostic codes are listed in [`docs/diagnostics.md`](docs/diagnostics.md).

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
- Not implemented: `magi snapshot`, caching, pushdown to SQL sources, `magi test`.
