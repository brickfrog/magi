# Not implemented yet

- Caching: every run loads each source into a new in-memory DuckDB database and discards it
  afterwards. A cache would reuse the sources and relations whose inputs have not changed.
- Pushdown to SQL sources: a `sql(...)` source runs its query as written and stages every row.
  Pushdown would send the program's filters and column selection to the database.
