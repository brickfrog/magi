//! Command-line interface.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::ast;
use crate::backend::{duckdb as lower, run, sql};
use crate::diagnostic::{self, Diagnostic, Diagnostics, Severity};
use crate::plan::physical::{self, Step};
use crate::reconcile::model::Consume;
use crate::semantic::hir::{Hir, Node, RelKind};
use crate::semantic::load::{self, Loaded};
use crate::semantic::resolve::{self, Options};
use crate::semantic::schema::SchemaProvider;
use crate::source::Reader;
use crate::testing;

#[derive(Parser)]
#[command(
    name = "magi",
    version,
    about = "A language for reproducible business-data analysis and reconciliation"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// `--today`, accepted by every command that analyses a program so that `plan`, `sql` and
/// `explain` show exactly what `run` executes.
#[derive(Args)]
struct Today {
    /// Date that `today()` returns; defaults to the current UTC date
    #[arg(long = "today", value_name = "YYYY-MM-DD", value_parser = resolve::parse_date)]
    date: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Parse, resolve and type-check a program without executing it
    Check {
        file: PathBuf,
        /// Also connect to SQL sources to check their columns
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Show the execution plan
    Plan {
        file: PathBuf,
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Execute the program and write its exports
    Run {
        file: PathBuf,
        /// Write outputs even if a `require` check fails (the run still exits with an error)
        #[arg(long)]
        keep_going: bool,
        #[command(flatten)]
        today: Today,
        /// Only print diagnostics
        #[arg(long, short)]
        quiet: bool,
    },
    /// Run the `test` declarations of programs (`.magi` files, or directories searched
    /// recursively) and compare their relations with expected files
    Test {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Only run the tests whose name contains this text
        #[arg(long)]
        filter: Option<String>,
        #[command(flatten)]
        today: Today,
    },
    /// Print the generated DuckDB SQL (for one relation, or everything)
    Sql {
        file: PathBuf,
        target: Option<String>,
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Explain how a relation is computed
    Explain {
        file: PathBuf,
        target: String,
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Show the columns of a source or relation
    Schema {
        file: PathBuf,
        relation: String,
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Show where a column's values come from (`relation.column`)
    Trace {
        file: PathBuf,
        column: String,
        #[arg(long)]
        sources: bool,
        #[command(flatten)]
        today: Today,
    },
    /// Format programs in canonical style (`.magi` files, or directories searched recursively)
    Fmt {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Exit with an error if a file is not formatted, listing each one (do not write)
        #[arg(long)]
        check: bool,
        /// Print the formatted program instead of rewriting the file (one file only)
        #[arg(long)]
        stdout: bool,
    },
    /// Generate a BI semantic model from the program's `model` declarations
    Compile {
        file: PathBuf,
        /// Output format (only `tmdl`: a Power BI semantic model folder)
        #[arg(long, default_value = "tmdl")]
        target: String,
        /// Directory to write into (default: `powerbi/` next to the program)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Folder Power BI reads the model's data files from (default: where the program
        /// exports them)
        #[arg(long)]
        data_folder: Option<String>,
    },
}

fn color() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn print_diagnostics(loaded: &Loaded, diags: &[Diagnostic], min: Severity) {
    let color = color();
    for d in diags.iter().filter(|d| d.severity >= min) {
        eprint!("{}", diagnostic::render(d, &loaded.sources, color));
    }
}

/// Current UTC date as `YYYY-MM-DD`.
fn utc_today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

struct Analysed {
    loaded: Loaded,
    hir: Hir,
    diags: Diagnostics,
    reader: Reader,
}

fn analyse(
    file: &Path,
    contact_external: bool,
    today: Option<String>,
) -> Result<Analysed, ExitCode> {
    let loaded = load::load(file).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::from(2)
    })?;
    let mut reader = Reader::new(contact_external).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::from(2)
    })?;
    let options = Options {
        today: today.unwrap_or_else(utc_today),
    };
    let (hir, diags) = resolve::analyze(&loaded, &mut reader as &mut dyn SchemaProvider, &options);
    Ok(Analysed {
        loaded,
        hir,
        diags,
        reader,
    })
}

/// Analyse and stop on errors (printing warnings and errors).
fn analyse_ok(
    file: &Path,
    contact_external: bool,
    today: Option<String>,
    show: Severity,
) -> Result<Analysed, ExitCode> {
    let a = analyse(file, contact_external, today)?;
    print_diagnostics(&a.loaded, &a.diags.list, show);
    if a.diags.has_errors() {
        eprintln!(
            "{} error(s); nothing was executed",
            a.diags.count(Severity::Error)
        );
        return Err(ExitCode::from(1));
    }
    Ok(a)
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check {
            file,
            sources,
            today,
        } => check(&file, sources, today.date),
        Command::Plan {
            file,
            sources,
            today,
        } => plan(&file, sources, today.date),
        Command::Run {
            file,
            keep_going,
            today,
            quiet,
        } => run_cmd(&file, keep_going, today.date, quiet),
        Command::Sql {
            file,
            target,
            sources,
            today,
        } => sql_cmd(&file, target.as_deref(), sources, today.date),
        Command::Explain {
            file,
            target,
            sources,
            today,
        } => explain(&file, &target, sources, today.date),
        Command::Schema {
            file,
            relation,
            sources,
            today,
        } => schema(&file, &relation, sources, today.date),
        Command::Trace {
            file,
            column,
            sources,
            today,
        } => trace(&file, &column, sources, today.date),
        Command::Test {
            paths,
            filter,
            today,
        } => test_cmd(&paths, filter.as_deref(), today.date),
        Command::Fmt {
            paths,
            check,
            stdout,
        } => fmt(&paths, check, stdout),
        Command::Compile {
            file,
            target,
            out,
            data_folder,
        } => compile(&file, &target, out, data_folder.as_deref()),
    };
    result.unwrap_or_else(|code| code)
}

fn compile(
    file: &Path,
    target: &str,
    out: Option<PathBuf>,
    data_folder: Option<&str>,
) -> Result<ExitCode, ExitCode> {
    if target != "tmdl" {
        eprintln!("error: unknown target `{target}` (supported: tmdl)");
        return Err(ExitCode::from(2));
    }
    let a = analyse_ok(file, false, None, Severity::Warning)?;
    if a.hir.models.is_empty() {
        eprintln!("error: {} declares no `model`", file.display());
        return Ok(ExitCode::from(1));
    }
    let dir = out.unwrap_or_else(|| {
        file.parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
            .join("powerbi")
    });
    for model in &a.hir.models {
        match crate::export::tmdl::write(model, &dir, data_folder) {
            Ok(files) => {
                println!(
                    "model {}: {} files in {}",
                    model.name,
                    files.len(),
                    dir.join(format!("{}.SemanticModel", model.name)).display()
                );
                for t in model.tables.iter().filter(|t| !t.path.exists()) {
                    println!(
                        "  note: {} does not exist yet; run `magi run {}` to export it",
                        t.path.display(),
                        file.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(ExitCode::from(1));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn check(file: &Path, sources: bool, today: Option<String>) -> Result<ExitCode, ExitCode> {
    let today = today.unwrap_or_else(utc_today);
    let a = analyse(file, sources, Some(today.clone()))?;
    print_diagnostics(&a.loaded, &a.diags.list, Severity::Note);
    let (mut e, mut w) = (
        a.diags.count(Severity::Error),
        a.diags.count(Severity::Warning),
    );
    // each test's program, for the problems its `given`s and `expect`s bring
    let mut tested = testing::duplicate_names(&a.loaded);
    let tests = testing::entry_tests(&a.loaded);
    for t in &tests {
        let p = testing::prepare(&a.loaded, t, &today).map_err(|err| {
            eprintln!("error: {err}");
            ExitCode::from(2)
        })?;
        tested.extend(testing::new_diagnostics(&a.diags.list, &p.diags.list));
    }
    let mut seen = Vec::new();
    tested.retain(|d| {
        let new = !seen.contains(d);
        seen.push(d.clone());
        new
    });
    print_diagnostics(&a.loaded, &tested, Severity::Note);
    e += tested
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    w += tested
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    if e > 0 {
        eprintln!("{e} error(s), {w} warning(s)");
        return Ok(ExitCode::from(1));
    }
    let h = &a.hir;
    println!(
        "ok: {} source(s), {} dataset(s), {} validation(s), {} reconciliation(s), {} export(s), {} test(s); {w} warning(s)",
        h.sources.len(),
        h.datasets.len(),
        h.validations.len(),
        h.reconciles.len(),
        h.exports.len(),
        tests.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn plan(file: &Path, sources: bool, today: Option<String>) -> Result<ExitCode, ExitCode> {
    let pinned = today.is_some();
    let a = analyse_ok(file, sources, today, Severity::Warning)?;
    let p = physical::build(&a.hir);
    for (i, line) in physical::describe(&a.hir, &p).iter().enumerate() {
        println!("{:>3}. {line}", i + 1);
    }
    if !p.unused.is_empty() {
        println!(
            "\nnot executed (no export or validation needs them): {}",
            p.unused.join(", ")
        );
    }
    if a.hir.uses_today {
        if pinned {
            println!("\nnote: `today()` is {} (--today)", a.hir.today);
        } else {
            println!(
                "\nnote: `today()` is used; results depend on the run date (pin it with `--today YYYY-MM-DD`)"
            );
        }
    }
    let natives = &a.hir.run_time_natives;
    if !natives.is_empty() {
        let one = natives.len() == 1;
        let names: Vec<String> = natives.iter().map(|n| format!("`duckdb.{n}`")).collect();
        println!(
            "\nnote: {} depend{} on when the run happens; `--today` does not pin {} (use `today()`)",
            names.join(", "),
            if one { "s" } else { "" },
            if one { "it" } else { "them" }
        );
    }
    for d in a.hir.datasets.iter().filter(|d| d.backend_specific) {
        println!("warning: dataset `{}` contains DuckDB-specific SQL", d.name);
    }
    Ok(ExitCode::SUCCESS)
}

fn run_cmd(
    file: &Path,
    keep_going: bool,
    today: Option<String>,
    quiet: bool,
) -> Result<ExitCode, ExitCode> {
    let mut a = analyse_ok(file, true, today, Severity::Warning)?;
    let p = physical::build(&a.hir);
    let mut log = |line: &str| {
        if !quiet && !line.trim_start().starts_with('[') {
            eprintln!("{line}");
        }
    };
    let outcome = run::execute(
        &a.hir,
        &p,
        &mut a.reader,
        &run::RunOptions { keep_going },
        &mut log,
    );
    print_diagnostics(&a.loaded, &outcome.diagnostics, Severity::Note);
    if outcome.failed || outcome.diagnostics.iter().any(Diagnostic::is_error) {
        eprintln!("run failed");
        return Ok(ExitCode::from(1));
    }
    if !quiet {
        eprintln!("done");
    }
    Ok(ExitCode::SUCCESS)
}

fn find_relation<'h>(
    hir: &'h Hir,
    name: &str,
) -> Result<&'h crate::semantic::hir::Relation, ExitCode> {
    hir.relation(name).ok_or_else(|| {
        let names: Vec<&str> = hir.relations.iter().map(|r| r.name.as_str()).collect();
        match diagnostic::did_you_mean(name, names) {
            Some(s) => eprintln!("error: unknown relation `{name}` (did you mean `{s}`?)"),
            None => eprintln!("error: unknown relation `{name}`"),
        }
        ExitCode::from(1)
    })
}

/// The declared spelling of a relation or reconcile named on the command line: names are
/// case-insensitive. An unknown name is returned unchanged (and reported by the caller).
fn declared_name(hir: &Hir, name: &str) -> String {
    hir.relation(name)
        .map(|r| r.name.clone())
        .or_else(|| {
            hir.reconciles
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(name))
                .map(|r| r.name.clone())
        })
        .unwrap_or_else(|| name.to_string())
}

/// Sources with unknown columns (no declared schema, not contacted) that `nodes` read, directly
/// or through other relations. SQL and plans shown for such nodes leave those columns out.
fn unknown_sources<'n>(hir: &Hir, nodes: impl IntoIterator<Item = &'n Node>) -> Vec<String> {
    let prod = physical::producers(hir);
    let mut stack: Vec<Node> = nodes.into_iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while let Some(node) = stack.pop() {
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Node::Source(i) = node {
            let name = &hir.sources[i].name;
            if hir.relation(name).is_some_and(|r| r.open) {
                out.push(name.clone());
            }
        }
        stack.extend(
            physical::inputs(hir, &node)
                .iter()
                .filter_map(|r| prod.get(r).cloned()),
        );
    }
    out.sort();
    out
}

/// Two lines saying that what follows is incomplete because `sources` were not contacted.
fn incomplete_note(sources: &[String]) -> Option<[String; 2]> {
    let (first, rest) = sources.split_first()?;
    let list = rest
        .iter()
        .fold(format!("`{first}`"), |acc, s| acc + &format!(", `{s}`"));
    let (what, their) = if rest.is_empty() {
        (format!("source {list} is"), "its")
    } else {
        (format!("sources {list} are"), "their")
    };
    Some([
        format!(
            "incomplete: the schema of {what} unknown (not declared, and not contacted without --sources);"
        ),
        format!(
            "column lists below leave {their} columns out. Run with --sources to see what `magi run` executes."
        ),
    ])
}

fn node_sql(hir: &Hir, node: &Node, kind: Option<crate::semantic::hir::Materialization>) -> String {
    match node {
        Node::Source(i) => {
            let s = &hir.sources[*i];
            let rel = hir.relation(&s.name).unwrap();
            crate::source::staging_sql(s, rel, &hir.connections)
        }
        Node::Dataset(i) => {
            let d = &hir.datasets[*i];
            let q = lower::query(&d.plan);
            let stmt = match kind.unwrap_or(crate::semantic::hir::Materialization::Table) {
                crate::semantic::hir::Materialization::Table => sql::Stmt::CreateTable {
                    name: d.name.clone(),
                    query: q,
                    temp: true,
                },
                crate::semantic::hir::Materialization::View => sql::Stmt::CreateView {
                    name: d.name.clone(),
                    query: q,
                },
            };
            format!("{};", sql::render(&stmt))
        }
        Node::Reconcile(i) => {
            run::reconcile_sql(&crate::reconcile::lower::lower(&hir.reconciles[*i]))
        }
        Node::Validation(i) => {
            let v = &hir.validations[*i];
            let mut out = format!("-- validate {}\n", v.target);
            if let Some(create) = run::validation_base_sql(hir, v) {
                out += &format!("{create}\n");
            }
            let (base, cols) = run::validation_base(hir, v);
            for c in &v.checks {
                let what = match c.kind {
                    crate::semantic::hir::CheckKind::Aggregate(_) => "fails when `ok` is FALSE",
                    _ => "failing rows",
                };
                out += &format!(
                    "-- {} {}: {what}\n{};\n",
                    c.severity.keyword(),
                    c.label,
                    sql::render_query(&run::check_query(&base, &cols, c))
                );
            }
            out
        }
        Node::Export(i) => {
            let e = &hir.exports[*i];
            let mut out = String::new();
            for p in &e.parts {
                let cols: Vec<String> = hir
                    .relation(&p.relation)
                    .map(|r| r.columns.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
                out += &format!(
                    "-- export {} to {} ({})\n{};\n",
                    p.relation,
                    e.display_path,
                    p.name,
                    sql::render_query(&crate::export::ordered_query(hir, &p.relation, &cols))
                );
            }
            out
        }
    }
}

fn sql_cmd(
    file: &Path,
    target: Option<&str>,
    sources: bool,
    today: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let a = analyse_ok(file, sources, today, Severity::Error)?;
    let hir = &a.hir;
    let p = physical::build(hir);
    let nodes: Vec<(Node, Option<crate::semantic::hir::Materialization>)> = match target {
        None => p
            .steps
            .iter()
            .map(|step| match step {
                Step::Stage { source } => (Node::Source(*source), None),
                Step::Materialize { dataset, kind } => (Node::Dataset(*dataset), Some(*kind)),
                Step::Validate { validation } => (Node::Validation(*validation), None),
                Step::Reconcile { reconcile } => (Node::Reconcile(*reconcile), None),
                Step::Export { export } => (Node::Export(*export), None),
            })
            .collect(),
        Some(t) => {
            let t = &declared_name(hir, t);
            let prod = physical::producers(hir);
            let node = match prod.get(t) {
                Some(n) => n.clone(),
                None => match hir.reconciles.iter().position(|r| &r.name == t) {
                    Some(i) => Node::Reconcile(i),
                    None => {
                        find_relation(hir, t)?;
                        return Err(ExitCode::from(1));
                    }
                },
            };
            let kind = p.steps.iter().find_map(|s| match (s, &node) {
                (Step::Materialize { dataset, kind }, Node::Dataset(i)) if dataset == i => {
                    Some(*kind)
                }
                _ => None,
            });
            vec![(node, kind)]
        }
    };
    if let Some(note) = incomplete_note(&unknown_sources(hir, nodes.iter().map(|(n, _)| n))) {
        for line in note {
            println!("-- {line}");
        }
        println!();
    }
    if target.is_none() {
        println!("-- session settings (`magi run` applies them first)");
        for stmt in run::session_settings(hir) {
            println!("{};", sql::render(&stmt));
        }
        println!();
    }
    let sep = if target.is_some() { "" } else { "\n" };
    for (node, kind) in &nodes {
        println!("{}{sep}", node_sql(hir, node, *kind));
    }
    Ok(ExitCode::SUCCESS)
}

fn explain(
    file: &Path,
    target: &str,
    sources: bool,
    today: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let a = analyse_ok(file, sources, today, Severity::Error)?;
    let hir = &a.hir;
    let target = &declared_name(hir, target);
    let prod = physical::producers(hir);
    let rc_index = hir.reconciles.iter().position(|r| &r.name == target);
    let node = match (prod.get(target), rc_index) {
        (Some(n), _) => n.clone(),
        (None, Some(i)) => Node::Reconcile(i),
        (None, None) => {
            find_relation(hir, target)?;
            return Err(ExitCode::from(1));
        }
    };
    // a source's own schema line says whether its columns are known
    if !matches!(node, Node::Source(_))
        && let Some(note) = incomplete_note(&unknown_sources(hir, [&node]))
    {
        println!("note: {}\n      {}\n", note[0], note[1]);
    }
    match node {
        Node::Source(i) => {
            let s = &hir.sources[i];
            println!("{target} is read from {}", s.kind.describe());
            let schema = if s.declared.is_some() {
                "declared (source contract)"
            } else if hir.relation(&s.name).is_some_and(|r| r.open) {
                "unknown (not contacted; use --sources)"
            } else {
                "inferred from the source"
            };
            println!("schema: {schema}");
            if let Some(ids) = &s.identity {
                println!("identity: {}", ids.join(", "));
            }
        }
        Node::Dataset(i) => {
            let d = &hir.datasets[i];
            println!("dataset {} (reads {})\n", d.name, d.uses.join(", "));
            print!("{}", d.plan.explain());
        }
        Node::Validation(i) => {
            let v = &hir.validations[i];
            println!("{target} is produced by the validation of {}:", v.target);
            for c in &v.checks {
                println!("  {} {}", c.severity.keyword(), c.label);
            }
        }
        Node::Export(_) => unreachable!("exports produce no relation"),
        Node::Reconcile(i) => {
            let rc = &hir.reconciles[i];
            println!(
                "reconcile {} = {} (as {}) with {} (as {})",
                rc.name, rc.a.relation, rc.a.alias, rc.b.relation, rc.b.alias
            );
            println!(
                "  cardinality {}, consume {}, ambiguity {}, duplicates {}",
                rc.cardinality.name(),
                rc.consume.name(),
                hold(rc.ambiguity),
                hold(rc.duplicates)
            );
            for (side, s) in [("a", &rc.a), ("b", &rc.b)] {
                let id = match &s.identity {
                    crate::reconcile::model::Identity::Declared(ids) => ids.join(", "),
                    crate::reconcile::model::Identity::Synthetic => {
                        "synthetic row numbers (not stable across refreshes)".into()
                    }
                };
                println!(
                    "  side {side} identity: {id}; rows are interchangeable when equal on: {}",
                    s.policy_columns.join(", ")
                );
            }
            let consumption = match rc.consume {
                Consume::Both => "rows matched by a tier are not seen by later tiers".to_string(),
                Consume::A | Consume::B => {
                    let (gone, kept) = if rc.consume == Consume::A {
                        ("a", "b")
                    } else {
                        ("b", "a")
                    };
                    format!(
                        "side {gone} rows matched by a tier are not seen by later tiers; side {kept} rows stay available to later tiers"
                    )
                }
                Consume::None => {
                    "every tier sees every row, but a pair matched once is not matched again"
                        .to_string()
                }
            };
            println!("\ntiers, in priority order ({consumption}):");
            for (k, t) in rc.tiers.iter().enumerate() {
                println!("  {}. {}", k + 1, t.description());
                for (name, when, text) in &t.flags {
                    match when {
                        Some(_) => println!("       flag {name} when {text}"),
                        None => println!("       flag {name}"),
                    }
                }
                for e in &t.evidence {
                    println!("       evidence {} = {}", e.name, e.text);
                }
            }
            if !rc.evidence.is_empty() || !rc.flags.is_empty() {
                println!("\nrecorded for every pair:");
                for e in &rc.evidence {
                    println!("  {} = {}", e.name, e.text);
                }
                for f in &rc.flags {
                    println!("  flag {} when {}", f.name, f.text);
                }
            }
            println!(
                "\nselection: candidates are unit pairs (rows, or groups in rollup tiers) that share the blocking keys and pass every `require`."
            );
            println!(
                "  one_to_one: classes of interchangeable rows are paired when each is the other's unique best candidate by `rank by`;"
            );
            println!(
                "  leftover exact duplicates are reported as duplicates, and equally good distinguishable candidates as ambiguous."
            );
            if rc.tiers.iter().any(|t| t.subset.is_some()) {
                println!(
                    "  subset tiers: a unit matches its single best subset of up to `max_items` member rows; units whose best subsets tie or share a row are ambiguous."
                );
            }
            let program = crate::reconcile::lower::lower(rc);
            let part = target.strip_prefix(&format!("{}.", rc.name));
            if let Some(part) = part {
                let table = format!("{}.{part}", rc.name);
                if let Some(plan) = program.outputs.iter().find_map(|op| match op {
                    crate::reconcile::lower::Op::Create { table: t, plan } if *t == table => {
                        Some(plan)
                    }
                    _ => None,
                }) {
                    println!("\nlogical plan of {table}:\n{}", plan.explain());
                }
            } else {
                for t in &program.tiers {
                    println!("\ntier {} {} candidates:", t.index, t.name);
                    let subset_ops = match &t.mode {
                        crate::reconcile::lower::TierMode::Subset(sub) => sub.ops.as_slice(),
                        _ => &[],
                    };
                    for op in t.prepare.iter().chain(subset_ops) {
                        if let crate::reconcile::lower::Op::Create { table, plan } = op
                            && table.ends_with("_cand")
                        {
                            print!("{}", plan.explain());
                        }
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn hold(h: crate::reconcile::model::Hold) -> &'static str {
    match h {
        crate::reconcile::model::Hold::Hold => "hold",
        crate::reconcile::model::Hold::Continue => "continue",
    }
}

fn schema(
    file: &Path,
    name: &str,
    sources: bool,
    today: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let mut a = analyse_ok(file, sources, today, Severity::Error)?;
    let rel = find_relation(&a.hir, name)?.clone();
    let src = a.hir.sources.iter().find(|s| s.name == rel.name).cloned();
    let inferred = match &src {
        Some(s) => a.reader.infer(s, &a.hir.connections).ok().flatten(),
        None => None,
    };
    let kind = match rel.kind {
        RelKind::Source => "source",
        RelKind::SourceRejects => "source rejects",
        RelKind::Dataset => "dataset",
        RelKind::ReconcilePart => "reconciliation output",
        RelKind::ValidationPart => "validation output",
    };
    let file = a.loaded.sources.file(rel.span.file);
    let (line, _) = file.line_col(rel.span.start);
    println!("{name} ({kind}, defined at {}:{line})", file.name);
    if rel.open {
        println!(
            "  columns unknown until the source is contacted (use --sources or declare a schema)"
        );
    }
    let width = rel
        .columns
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(4)
        .max(6);
    for c in &rel.columns {
        let mut notes = Vec::new();
        if let Some(s) = &src {
            let declared = s
                .declared
                .as_ref()
                .and_then(|d| d.iter().find(|x| x.name == c.name));
            match declared {
                Some(d) => {
                    notes.push("declared".to_string());
                    if !d.formats.is_empty() {
                        notes.push(format!("formats {}", d.formats.join(", ")));
                    }
                }
                None => notes.push("inferred".into()),
            }
            if let Some(inf) = &inferred
                && let Some((_, t)) = inf.columns.iter().find(|(n, _)| n == &c.name)
                && *t != c.ty.ty
            {
                notes.push(format!("source reports {t}"));
            }
        }
        if rel
            .identity
            .as_ref()
            .is_some_and(|ids| ids.contains(&c.name))
        {
            notes.push("identity".into());
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join("; "))
        };
        println!("  {:<width$}  {}{notes}", c.name, c.ty);
    }
    Ok(ExitCode::SUCCESS)
}

fn trace(
    file: &Path,
    column: &str,
    sources: bool,
    today: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let a = analyse_ok(file, sources, today, Severity::Error)?;
    let Some((rel_name, col)) = column.rsplit_once('.') else {
        eprintln!("error: expected `relation.column`");
        return Err(ExitCode::from(2));
    };
    let rel = find_relation(&a.hir, rel_name)?;
    let Some(c) = rel.column(col) else {
        let names: Vec<&str> = rel.columns.iter().map(|c| c.name.as_str()).collect();
        match diagnostic::did_you_mean(col, names) {
            Some(s) => eprintln!("error: `{rel_name}` has no column `{col}` (did you mean `{s}`?)"),
            None => eprintln!("error: `{rel_name}` has no column `{col}`"),
        }
        return Err(ExitCode::from(1));
    };
    println!("{rel_name}.{col}  ({})", c.ty);
    let tree = crate::lineage::render(&a.hir.lineage, c.lineage);
    for line in tree.lines().skip(usize::from(
        tree.lines()
            .next()
            .is_some_and(|l| l == format!("{rel_name}.{col}")),
    )) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The `.magi` files `paths` name: files as given, directories searched recursively (entries
/// whose name starts with `.` skipped, symlinked directories not followed), each directory in
/// name order so the output does not depend on the file system.
fn magi_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let err = |e: std::io::Error| format!("cannot read {}: {e}", dir.display());
        let mut entries = std::fs::read_dir(dir)
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = e.path();
            if e.file_type().map_err(err)?.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|x| x == "magi") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let before = files.len();
            walk(p, &mut files)?;
            if files.len() == before {
                return Err(format!("no `.magi` files in {}", p.display()));
            }
        } else {
            files.push(p.clone());
        }
    }
    Ok(files)
}

/// Formats every file and keeps going after a failure; the exit code is the worst outcome:
/// 2 when a file cannot be read or written, 1 when one is not formatted (`--check`) or does not
/// parse, else 0.
fn fmt(paths: &[PathBuf], check: bool, stdout: bool) -> Result<ExitCode, ExitCode> {
    let files = magi_files(paths).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::from(2)
    })?;
    if stdout && files.len() != 1 {
        eprintln!(
            "error: `--stdout` prints one formatted file; {} were given",
            files.len()
        );
        return Err(ExitCode::from(2));
    }
    let mut worst = 0u8;
    for file in &files {
        let code = fmt_file(file, check, stdout);
        worst = worst.max(code);
    }
    Ok(ExitCode::from(worst))
}

fn fmt_file(file: &Path, check: bool, stdout: bool) -> u8 {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", file.display());
            return 2;
        }
    };
    match crate::fmt::format_source(0, &text) {
        Ok(formatted) => {
            if check {
                if formatted != text {
                    eprintln!("{} is not formatted", file.display());
                    return 1;
                }
            } else if stdout {
                print!("{formatted}");
            } else if formatted != text
                && let Err(e) = std::fs::write(file, formatted)
            {
                eprintln!("error: cannot write {}: {e}", file.display());
                return 2;
            }
            0
        }
        Err(diags) => {
            let loaded = load::load_text(file, text);
            print_diagnostics(&loaded, &diags, Severity::Note);
            1
        }
    }
}

/// Runs the tests of every file and keeps going after a failure; the exit code is the worst
/// outcome: 2 when a file cannot be read, a file named explicitly has no tests or nothing is
/// selected, 1 when a test fails or a file's tests cannot run, else 0.
fn test_cmd(
    paths: &[PathBuf],
    filter: Option<&str>,
    today: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let files = magi_files(paths).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::from(2)
    })?;
    let today = today.unwrap_or_else(utc_today);
    let mut worst = 0u8;
    let mut suites = Vec::new();
    for file in files {
        let loaded = match load::load(&file) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: {e}");
                worst = 2;
                continue;
            }
        };
        if testing::entry_tests(&loaded).is_empty() {
            // the entry file is the first file loaded
            let broken = loaded
                .diagnostics
                .iter()
                .any(|d| d.is_error() && d.labels.first().is_some_and(|l| l.span.file == 0));
            if paths.contains(&file) {
                eprintln!("error: {} declares no `test`", file.display());
                worst = 2;
            } else if broken {
                // its tests may be among the statements that did not parse
                print_diagnostics(&loaded, &loaded.diagnostics, Severity::Note);
                worst = worst.max(1);
            }
            continue;
        }
        suites.push((file, loaded));
    }
    let selected = |t: &&ast::TestDecl| filter.is_none_or(|f| t.name.name.contains(f));
    let any = suites
        .iter()
        .any(|(_, l)| testing::entry_tests(l).iter().any(selected));
    if !any {
        match filter {
            Some(f) => eprintln!("error: no test name contains `{f}`"),
            // a file named without tests is already reported
            None if worst == 2 => {}
            None => eprintln!("error: no `test` declarations found"),
        }
        return Err(ExitCode::from(2));
    }
    let (mut passed, mut failed) = (0usize, 0usize);
    let several = suites.len() > 1;
    for (file, loaded) in &suites {
        let tests: Vec<&ast::TestDecl> = testing::entry_tests(loaded)
            .into_iter()
            .filter(selected)
            .collect();
        if tests.is_empty() {
            continue;
        }
        if several {
            println!("{}", file.display());
        }
        // The program without test changes: its warnings and notes are printed once, not per
        // test. Its errors are not (the files a test replaces need not exist); a test whose
        // program has errors prints them.
        let mut reader = Reader::new(false).map_err(|e| {
            eprintln!("error: {e}");
            ExitCode::from(2)
        })?;
        let options = Options {
            today: today.clone(),
        };
        let (_, mut base) =
            resolve::analyze(loaded, &mut reader as &mut dyn SchemaProvider, &options);
        base.list.retain(|d| !d.is_error());
        print_diagnostics(loaded, &base.list, Severity::Note);
        let duplicates = testing::duplicate_names(loaded);
        if !duplicates.is_empty() {
            print_diagnostics(loaded, &duplicates, Severity::Note);
            eprintln!("{}: no test was run", file.display());
            worst = worst.max(1);
            continue;
        }
        for t in tests {
            let mut p = testing::prepare(loaded, t, &today).map_err(|e| {
                eprintln!("error: {e}");
                ExitCode::from(2)
            })?;
            let own = testing::new_diagnostics(&base.list, &p.diags.list);
            print_diagnostics(&p.loaded, &own, Severity::Note);
            let failures = if p.diags.has_errors() {
                vec![format!(
                    "{} error(s) in the test's program; it did not run",
                    p.diags.count(Severity::Error)
                )]
            } else {
                let verdict = testing::run(&mut p);
                print_diagnostics(&p.loaded, &verdict.diagnostics, Severity::Note);
                verdict.failures
            };
            if failures.is_empty() {
                println!("test {} ... ok", t.name.name);
                passed += 1;
            } else {
                println!("test {} ... FAILED", t.name.name);
                for line in failures.iter().flat_map(|f| f.lines()) {
                    println!("  {line}");
                }
                failed += 1;
            }
        }
    }
    let total = passed + failed;
    println!(
        "{total} test{}: {passed} passed, {failed} failed",
        if total == 1 { "" } else { "s" }
    );
    if failed > 0 {
        worst = worst.max(1);
    }
    Ok(ExitCode::from(worst))
}
