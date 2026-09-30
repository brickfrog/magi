//! DuckDB functions reachable through the `duckdb.name(...)` escape hatch.
//! `magi check` looks them up in DuckDB's own catalog, so a misspelt name, or a function that
//! cannot be called per row (an aggregate or table function), fails before a run, and a function
//! whose result depends on the run is reported.

use std::collections::HashMap;
use std::sync::LazyLock;

/// How reproducible a scalar function's result is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stability {
    /// Same arguments, same result.
    Stable,
    /// Fixed within one query but depends on when/where the run happens (`now`, `current_date`).
    PerRun,
    /// Can differ on every call (`random`, `uuid`).
    Volatile,
}

#[derive(Default)]
struct Entry {
    /// Callable per row: a scalar function or a scalar macro.
    scalar: Option<Stability>,
    aggregate: bool,
    table: bool,
}

/// Names that read the clock: SQL keywords (not listed as functions) and functions the catalog
/// marks CONSISTENT although their result depends on the run (`current_localtimestamp`).
const CLOCK: [&str; 7] = [
    "current_timestamp",
    "current_date",
    "current_time",
    "localtimestamp",
    "localtime",
    "current_localtimestamp",
    "current_localtime",
];

static CATALOG: LazyLock<Result<HashMap<String, Entry>, String>> = LazyLock::new(|| {
    let conn = duckdb::Connection::open_in_memory().map_err(|e| e.to_string())?;
    // as in a run (`backend::run::session_settings`): setting the time zone loads ICU, which
    // provides `current_date`, `today`, ...
    conn.execute_batch("SET TimeZone = 'UTC'")
        .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT function_name, function_type, coalesce(stability, ''), coalesce(macro_definition, '') \
             FROM duckdb_functions()",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut catalog: HashMap<String, Entry> = HashMap::new();
    let mut macros = Vec::new();
    for (name, kind, stability, definition) in rows {
        let entry = catalog.entry(name.to_ascii_lowercase()).or_default();
        match kind.as_str() {
            "scalar" => {
                let s = match stability.as_str() {
                    "VOLATILE" => Stability::Volatile,
                    "CONSISTENT_WITHIN_QUERY" => Stability::PerRun,
                    _ => Stability::Stable,
                };
                entry.scalar = Some(entry.scalar.map_or(s, |prev| prev.max(s)));
            }
            "macro" => macros.push((name.to_ascii_lowercase(), definition)),
            "aggregate" => entry.aggregate = true,
            "table" | "table_macro" => entry.table = true,
            _ => {}
        }
    }
    for name in CLOCK {
        if let Some(Entry {
            scalar: Some(s), ..
        }) = catalog.get_mut(name)
        {
            *s = (*s).max(Stability::PerRun);
        }
    }
    // A macro is as reproducible as the functions its body calls.
    let unstable: HashMap<String, Stability> = catalog
        .iter()
        .filter_map(|(n, e)| {
            e.scalar
                .filter(|s| *s != Stability::Stable)
                .map(|s| (n.clone(), s))
        })
        .chain(CLOCK.iter().map(|k| (k.to_string(), Stability::PerRun)))
        .collect();
    for (name, definition) in macros {
        let s = definition
            .to_ascii_lowercase()
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter_map(|word| unstable.get(word).copied())
            .max()
            .unwrap_or(Stability::Stable);
        let entry = catalog.entry(name).or_default();
        entry.scalar = Some(entry.scalar.map_or(s, |prev| prev.max(s)));
    }
    Ok(catalog)
});

pub enum Lookup {
    Missing,
    /// Only an aggregate function: it cannot be called per row.
    Aggregate,
    /// Only a table (or pragma) function: it produces rows, not a value.
    Table,
    Scalar(Stability),
}

pub fn lookup(name: &str) -> Result<Lookup, String> {
    let catalog = CATALOG.as_ref().map_err(Clone::clone)?;
    Ok(match catalog.get(&name.to_ascii_lowercase()) {
        None => Lookup::Missing,
        Some(Entry {
            scalar: Some(s), ..
        }) => Lookup::Scalar(*s),
        Some(Entry {
            aggregate: true, ..
        }) => Lookup::Aggregate,
        Some(Entry { table: true, .. }) => Lookup::Table,
        // pragmas only
        Some(_) => Lookup::Table,
    })
}

/// Names callable as `duckdb.name(...)` (for suggestions).
pub fn names() -> Vec<&'static str> {
    CATALOG
        .as_ref()
        .map(|c| {
            c.iter()
                .filter(|(_, e)| e.scalar.is_some())
                .map(|(n, _)| n.as_str())
                .collect()
        })
        .unwrap_or_default()
}
