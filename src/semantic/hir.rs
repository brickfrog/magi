//! Typed semantic representation (HIR). Names are resolved, expressions are typed, and every
//! relation has a known (or explicitly open) schema. Execution decisions still do not live here.

use std::collections::HashMap;

use crate::ast::{BinaryOp, CheckSeverity, UnaryOp};
use crate::semantic::types::{ColType, Type};
use crate::source::excel::ExcelOptions;
use crate::source::fixed::FixedWidthOptions;
use crate::syntax::span::Span;

// ---------------------------------------------------------------------------------------------
// expressions

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Int(i64),
    /// Decimal literal text, e.g. `0.02`.
    Decimal(String),
    Str(String),
    Bool(bool),
    Null,
    /// ISO date `YYYY-MM-DD`.
    Date(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Sum,
    Mean,
    Min,
    Max,
    Count,
    CountDistinct,
    /// Fraction of rows whose value is null or blank text.
    Missing,
    Any,
    All,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Sum => "sum",
            AggFunc::Mean => "mean",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Count => "count",
            AggFunc::CountDistinct => "count_distinct",
            AggFunc::Missing => "missing",
            AggFunc::Any => "any",
            AggFunc::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinFunc {
    RowNumber,
    Rank,
    DenseRank,
    Count,
    Max,
    Min,
    /// The argument on the next row in window order (null on the last).
    Lead,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: TExpr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TExprKind {
    /// Column of input `slot` (0 for single-input operators; 0 = left / 1 = right for joins).
    Column {
        slot: u8,
        name: String,
    },
    Literal(Lit),
    /// Portable MAGI scalar function (lowered per backend).
    Call {
        func: &'static str,
        args: Vec<TExpr>,
    },
    Agg {
        func: AggFunc,
        arg: Option<Box<TExpr>>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<TExpr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<TExpr>,
        right: Box<TExpr>,
    },
    IsNull {
        expr: Box<TExpr>,
        negated: bool,
    },
    InList {
        expr: Box<TExpr>,
        list: Vec<TExpr>,
        negated: bool,
    },
    Case {
        arms: Vec<(TExpr, TExpr)>,
        otherwise: Option<Box<TExpr>>,
    },
    /// Backend-specific escape hatch: `duckdb.name(args)`.
    Native {
        name: String,
        args: Vec<TExpr>,
    },
    /// Lossless-or-null conversion.
    TryCast {
        expr: Box<TExpr>,
        ty: Type,
    },
    /// Window function (internal: reconciliation lowering).
    Window {
        func: WinFunc,
        arg: Option<Box<TExpr>>,
        partition: Vec<TExpr>,
        order: Vec<OrderKey>,
        filter: Option<Box<TExpr>>,
    },
    /// Constant list, e.g. date formats.
    List(Vec<TExpr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TExpr {
    pub kind: TExprKind,
    pub ty: ColType,
}

impl TExpr {
    pub fn new(kind: TExprKind, ty: ColType) -> Self {
        Self { kind, ty }
    }
    pub fn col(slot: u8, name: impl Into<String>, ty: ColType) -> Self {
        Self::new(
            TExprKind::Column {
                slot,
                name: name.into(),
            },
            ty,
        )
    }
    pub fn lit(l: Lit) -> Self {
        let ty = match &l {
            Lit::Int(_) => ColType::required(Type::Int),
            Lit::Decimal(d) => ColType::required(decimal_literal_type(d)),
            Lit::Str(_) => ColType::required(Type::String),
            Lit::Bool(_) => ColType::required(Type::Bool),
            Lit::Null => ColType::nullable(Type::Null),
            Lit::Date(_) => ColType::required(Type::Date),
        };
        Self::new(TExprKind::Literal(l), ty)
    }

    pub fn walk<'a>(&'a self, f: &mut impl FnMut(&'a TExpr)) {
        f(self);
        match &self.kind {
            TExprKind::Column { .. } | TExprKind::Literal(_) => {}
            TExprKind::Call { args, .. }
            | TExprKind::Native { args, .. }
            | TExprKind::List(args) => args.iter().for_each(|a| a.walk(f)),
            TExprKind::Agg { arg, .. } => {
                if let Some(a) = arg {
                    a.walk(f)
                }
            }
            TExprKind::Unary { expr, .. }
            | TExprKind::IsNull { expr, .. }
            | TExprKind::TryCast { expr, .. } => expr.walk(f),
            TExprKind::Binary { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            TExprKind::InList { expr, list, .. } => {
                expr.walk(f);
                list.iter().for_each(|a| a.walk(f));
            }
            TExprKind::Case { arms, otherwise } => {
                for (w, t) in arms {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(o) = otherwise {
                    o.walk(f);
                }
            }
            TExprKind::Window {
                arg,
                partition,
                order,
                filter,
                ..
            } => {
                if let Some(a) = arg {
                    a.walk(f);
                }
                partition.iter().for_each(|p| p.walk(f));
                order.iter().for_each(|o| o.expr.walk(f));
                if let Some(x) = filter {
                    x.walk(f);
                }
            }
        }
    }

    /// Rewrite every column reference.
    pub fn map_columns(self, f: &mut impl FnMut(u8, String, ColType) -> TExpr) -> TExpr {
        let ty = self.ty;
        let kind = match self.kind {
            TExprKind::Column { slot, name } => return f(slot, name, ty),
            TExprKind::Literal(l) => TExprKind::Literal(l),
            TExprKind::Call { func, args } => TExprKind::Call {
                func,
                args: args.into_iter().map(|a| a.map_columns(f)).collect(),
            },
            TExprKind::Native { name, args } => TExprKind::Native {
                name,
                args: args.into_iter().map(|a| a.map_columns(f)).collect(),
            },
            TExprKind::List(args) => {
                TExprKind::List(args.into_iter().map(|a| a.map_columns(f)).collect())
            }
            TExprKind::Agg { func, arg } => TExprKind::Agg {
                func,
                arg: arg.map(|a| Box::new(a.map_columns(f))),
            },
            TExprKind::Unary { op, expr } => TExprKind::Unary {
                op,
                expr: Box::new(expr.map_columns(f)),
            },
            TExprKind::IsNull { expr, negated } => TExprKind::IsNull {
                expr: Box::new(expr.map_columns(f)),
                negated,
            },
            TExprKind::TryCast { expr, ty } => TExprKind::TryCast {
                expr: Box::new(expr.map_columns(f)),
                ty,
            },
            TExprKind::Binary { op, left, right } => TExprKind::Binary {
                op,
                left: Box::new(left.map_columns(f)),
                right: Box::new(right.map_columns(f)),
            },
            TExprKind::InList {
                expr,
                list,
                negated,
            } => TExprKind::InList {
                expr: Box::new(expr.map_columns(f)),
                list: list.into_iter().map(|a| a.map_columns(f)).collect(),
                negated,
            },
            TExprKind::Case { arms, otherwise } => TExprKind::Case {
                arms: arms
                    .into_iter()
                    .map(|(w, t)| (w.map_columns(f), t.map_columns(f)))
                    .collect(),
                otherwise: otherwise.map(|o| Box::new(o.map_columns(f))),
            },
            TExprKind::Window {
                func,
                arg,
                partition,
                order,
                filter,
            } => TExprKind::Window {
                func,
                arg: arg.map(|a| Box::new(a.map_columns(f))),
                partition: partition.into_iter().map(|p| p.map_columns(f)).collect(),
                order: order
                    .into_iter()
                    .map(|o| OrderKey {
                        expr: o.expr.map_columns(f),
                        desc: o.desc,
                    })
                    .collect(),
                filter: filter.map(|x| Box::new(x.map_columns(f))),
            },
        };
        TExpr { kind, ty }
    }

    /// Post-order rewrite: children first, then `f` on the rebuilt node.
    pub fn transform(self, f: &mut impl FnMut(TExpr) -> TExpr) -> TExpr {
        let ty = self.ty;
        let kind = match self.kind {
            k @ (TExprKind::Column { .. } | TExprKind::Literal(_)) => k,
            TExprKind::Call { func, args } => TExprKind::Call {
                func,
                args: args.into_iter().map(|a| a.transform(f)).collect(),
            },
            TExprKind::Native { name, args } => TExprKind::Native {
                name,
                args: args.into_iter().map(|a| a.transform(f)).collect(),
            },
            TExprKind::List(args) => {
                TExprKind::List(args.into_iter().map(|a| a.transform(f)).collect())
            }
            TExprKind::Agg { func, arg } => TExprKind::Agg {
                func,
                arg: arg.map(|a| Box::new(a.transform(f))),
            },
            TExprKind::Unary { op, expr } => TExprKind::Unary {
                op,
                expr: Box::new(expr.transform(f)),
            },
            TExprKind::IsNull { expr, negated } => TExprKind::IsNull {
                expr: Box::new(expr.transform(f)),
                negated,
            },
            TExprKind::TryCast { expr, ty } => TExprKind::TryCast {
                expr: Box::new(expr.transform(f)),
                ty,
            },
            TExprKind::Binary { op, left, right } => TExprKind::Binary {
                op,
                left: Box::new(left.transform(f)),
                right: Box::new(right.transform(f)),
            },
            TExprKind::InList {
                expr,
                list,
                negated,
            } => TExprKind::InList {
                expr: Box::new(expr.transform(f)),
                list: list.into_iter().map(|a| a.transform(f)).collect(),
                negated,
            },
            TExprKind::Case { arms, otherwise } => TExprKind::Case {
                arms: arms
                    .into_iter()
                    .map(|(w, t)| (w.transform(f), t.transform(f)))
                    .collect(),
                otherwise: otherwise.map(|o| Box::new(o.transform(f))),
            },
            TExprKind::Window {
                func,
                arg,
                partition,
                order,
                filter,
            } => TExprKind::Window {
                func,
                arg: arg.map(|a| Box::new(a.transform(f))),
                partition: partition.into_iter().map(|p| p.transform(f)).collect(),
                order: order
                    .into_iter()
                    .map(|o| OrderKey {
                        expr: o.expr.transform(f),
                        desc: o.desc,
                    })
                    .collect(),
                filter: filter.map(|x| Box::new(x.transform(f))),
            },
        };
        f(TExpr { kind, ty })
    }

    pub fn contains_agg(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e.kind, TExprKind::Agg { .. }));
        found
    }

    /// Column names referenced from input `slot`.
    pub fn columns_of(&self, slot: u8) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let TExprKind::Column { slot: s, name } = &e.kind
                && *s == slot
                && !out.contains(name)
            {
                out.push(name.clone());
            }
        });
        out
    }
}

pub fn decimal_literal_type(text: &str) -> Type {
    let digits = text.trim_start_matches('-');
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let scale = frac.len().min(38) as u8;
    // DuckDB counts significant integer digits only: `0.001` is DECIMAL(3,3), `12.5` DECIMAL(3,1)
    let precision = (int.trim_start_matches('0').len() + frac.len()).clamp(1, 38) as u8;
    Type::Decimal(precision.max(scale), scale)
}

// ---------------------------------------------------------------------------------------------
// lineage

pub type LineageId = usize;

#[derive(Debug, Clone, PartialEq)]
pub struct LineageNode {
    /// e.g. `summary.a_total = sum(clean_a.amount)` or `source a.amount (a.xlsx, sheet Data)`.
    pub label: String,
    pub deps: Vec<LineageId>,
}

#[derive(Debug, Default, Clone)]
pub struct Lineage {
    pub nodes: Vec<LineageNode>,
}

impl Lineage {
    pub fn add(&mut self, label: impl Into<String>, deps: Vec<LineageId>) -> LineageId {
        self.nodes.push(LineageNode {
            label: label.into(),
            deps,
        });
        self.nodes.len() - 1
    }
}

// ---------------------------------------------------------------------------------------------
// relations

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    pub lineage: LineageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    Source,
    SourceRejects,
    Dataset,
    ReconcilePart,
    ValidationPart,
}

/// Anything a pipeline, export or validation can refer to by name.
#[derive(Debug, Clone)]
pub struct Relation {
    pub name: String,
    pub kind: RelKind,
    pub columns: Vec<Column>,
    /// Columns are not known statically (undeclared SQL source during `magi check`).
    pub open: bool,
    /// Columns that identify a row, when known to be preserved from a source identity.
    pub identity: Option<Vec<String>>,
    /// Declared final ordering (`|> sort`), used as the primary export order.
    pub sort: Vec<(String, bool)>,
    pub span: Span,
}

impl Relation {
    /// The column called `name`, ignoring ASCII case (names are case-insensitive, as in DuckDB;
    /// no relation has two columns that differ only in case).
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }
}

// ---------------------------------------------------------------------------------------------
// declarations

#[derive(Debug, Clone)]
pub enum Secret {
    Literal(String),
    /// Resolved from an environment variable at run time; the name is safe to print.
    Env(String),
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub name: String,
    pub kind: ConnectionKind,
}

#[derive(Debug, Clone)]
pub enum ConnectionKind {
    /// Either a DSN (with optional user/password) or a full connection string.
    Odbc {
        dsn: Option<String>,
        connection_string: Option<Secret>,
        user: Option<Secret>,
        password: Option<Secret>,
    },
    /// A DuckDB database file attached read-only.
    DuckDb { path: std::path::PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CsvOptions {
    pub delimiter: Option<String>,
    pub header: bool,
    pub all_text: bool,
    /// 1-based section to read: a section is a maximal run of non-blank lines.
    pub section: Option<u32>,
    /// Lines may have fewer fields than the header (or the widest line); missing fields are null.
    pub ragged: bool,
    /// Columns whose blank values take the nearest non-blank value above them.
    pub fill_down: Vec<String>,
    /// Name of the column holding each row's 1-based data row number.
    pub row_number: Option<String>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: None,
            header: true,
            all_text: false,
            section: None,
            ragged: false,
            fill_down: Vec::new(),
            row_number: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SourceKind {
    Csv {
        path: std::path::PathBuf,
        options: CsvOptions,
    },
    Parquet {
        path: std::path::PathBuf,
    },
    Excel {
        path: std::path::PathBuf,
        options: ExcelOptions,
    },
    /// Records at fixed byte positions, cut by a layout file.
    FixedWidth {
        path: std::path::PathBuf,
        options: FixedWidthOptions,
    },
    /// A table or query in a DuckDB database file.
    DuckDb {
        path: std::path::PathBuf,
        table: Option<String>,
        query: Option<String>,
    },
    /// Query through a named connection.
    Sql {
        connection: String,
        query: String,
    },
}

impl SourceKind {
    pub fn describe(&self) -> String {
        match self {
            SourceKind::Csv { path, options } => match options.section {
                Some(n) => format!("CSV {} / section {n}", path.display()),
                None => format!("CSV {}", path.display()),
            },
            SourceKind::Parquet { path } => format!("Parquet {}", path.display()),
            SourceKind::Excel { path, options } => {
                let mut s = format!("Excel {}", path.display());
                if let Some(sh) = &options.sheet {
                    s += &format!(" / sheet {sh}");
                }
                if let Some(r) = &options.range {
                    s += &format!(" / range {r}");
                }
                if let Some(n) = options.section {
                    s += &format!(" / section {n}");
                }
                s
            }
            SourceKind::FixedWidth { path, options } => {
                let mut s = format!(
                    "fixed-width {} / layout {}",
                    path.display(),
                    options.layout.display()
                );
                if let Some(r) = &options.record {
                    s += &format!(" / records {r}");
                }
                s
            }
            SourceKind::DuckDb { path, table, .. } => match table {
                Some(t) => format!("DuckDB {} / table {t}", path.display()),
                None => format!("DuckDB {} / query", path.display()),
            },
            SourceKind::Sql { connection, .. } => format!("SQL query via connection {connection}"),
        }
    }
    /// Columns filled down before typing (sources read in file order).
    pub fn fill_down(&self) -> &[String] {
        match self {
            SourceKind::Csv { options, .. } => &options.fill_down,
            SourceKind::Excel { options, .. } => &options.fill_down,
            SourceKind::FixedWidth { options, .. } => &options.fill_down,
            _ => &[],
        }
    }
    /// The column holding each row's 1-based data row number (sources read in file order).
    pub fn row_number(&self) -> Option<&str> {
        match self {
            SourceKind::Csv { options, .. } => options.row_number.as_deref(),
            SourceKind::Excel { options, .. } => options.row_number.as_deref(),
            SourceKind::FixedWidth { options, .. } => options.row_number.as_deref(),
            _ => None,
        }
    }
    /// Whether the source's row order is stable (files) or not (database queries).
    pub fn stable_row_order(&self) -> bool {
        !matches!(
            self,
            SourceKind::Sql { .. } | SourceKind::DuckDb { query: Some(_), .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredColumn {
    pub name: String,
    pub ty: ColType,
    /// Parse formats for date/timestamp columns read from text (`date("%m/%d/%Y")`).
    pub formats: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub kind: SourceKind,
    pub declared: Option<Vec<DeclaredColumn>>,
    pub identity: Option<Vec<String>>,
    pub span: Span,
}

impl Source {
    /// The declared column called `name`, ignoring ASCII case (a declaration may spell a
    /// column of the file in another case).
    pub fn declared_column(&self, name: &str) -> Option<&DeclaredColumn> {
        self.declared
            .as_ref()?
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialization {
    Table,
    View,
}

#[derive(Debug, Clone)]
pub struct Dataset {
    pub name: String,
    pub plan: crate::plan::logical::LogicalPlan,
    pub uses: Vec<String>,
    /// Uses `native_sql` or `duckdb.*` functions.
    pub backend_specific: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckKind {
    /// Rows where the predicate is false fail (null = pass, as in SQL CHECK constraints).
    Row(TExpr),
    /// Rows where the expression is null fail.
    NotNull(TExpr),
    /// Rows whose key occurs more than once fail.
    Unique(Vec<String>),
    /// One boolean over the whole relation.
    Aggregate(TExpr),
}

#[derive(Debug, Clone)]
pub struct Check {
    pub label: String,
    pub severity: CheckSeverity,
    pub kind: CheckKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Validation {
    pub target: String,
    pub checks: Vec<Check>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Parquet,
    Xlsx,
    DuckDb,
    Json,
}

#[derive(Debug, Clone)]
pub struct ExportPart {
    /// Sheet or table name (xlsx / duckdb); file stem otherwise.
    pub name: String,
    pub relation: String,
}

#[derive(Debug, Clone)]
pub struct Export {
    pub path: std::path::PathBuf,
    pub display_path: String,
    pub format: ExportFormat,
    pub parts: Vec<ExportPart>,
    pub span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct Runtime {
    pub threads: Option<u32>,
    pub memory_limit: Option<String>,
    /// `temp_storage: memory` (default) or a directory for spilling.
    pub temp_directory: Option<std::path::PathBuf>,
}

/// One executable unit in dependency order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Node {
    Source(usize),
    Dataset(usize),
    Validation(usize),
    Reconcile(usize),
    Export(usize),
}

/// A column of a BI model table.
#[derive(Debug, Clone)]
pub struct ModelColumn {
    /// Column name in the exported file.
    pub source: String,
    /// Name shown in the BI tool (a `dimension` may rename it).
    pub display: String,
    pub ty: Type,
    pub hidden: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelTable {
    /// MAGI relation name, used as the table name.
    pub name: String,
    pub columns: Vec<ModelColumn>,
    /// Parquet file the program exports the relation to.
    pub path: std::path::PathBuf,
    pub description: Option<String>,
}

/// `from` is the many side, `to` the one side.
#[derive(Debug, Clone)]
pub struct ModelRelationship {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

#[derive(Debug, Clone)]
pub struct ModelMetric {
    pub name: String,
    /// Table the measure is attached to.
    pub table: String,
    pub dax: String,
    pub format: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    pub tables: Vec<ModelTable>,
    pub relationships: Vec<ModelRelationship>,
    pub metrics: Vec<ModelMetric>,
}

/// The analysed program.
#[derive(Debug, Default)]
pub struct Hir {
    pub connections: Vec<Connection>,
    pub sources: Vec<Source>,
    pub datasets: Vec<Dataset>,
    pub validations: Vec<Validation>,
    pub reconciles: Vec<crate::reconcile::model::Reconcile>,
    pub exports: Vec<Export>,
    /// BI semantic models (`magi compile --target tmdl`).
    pub models: Vec<Model>,
    pub relations: Vec<Relation>,
    pub relation_index: HashMap<String, usize>,
    pub lineage: Lineage,
    pub runtime: Runtime,
    /// Executable nodes in dependency order.
    pub order: Vec<Node>,
    /// Date `today()` evaluates to.
    pub today: String,
    /// `today()` is used: results depend on the run date.
    pub uses_today: bool,
    /// `duckdb.*` functions whose result depends on when the run happens (`now`,
    /// `current_date`); `--today` does not pin them. Sorted, lower case, no duplicates.
    pub run_time_natives: Vec<String>,
}

impl Hir {
    /// The relation called `name` (`x`, `x.part`), ignoring ASCII case: relation names are
    /// unique regardless of case (M003), so at most one matches.
    pub fn relation(&self, name: &str) -> Option<&Relation> {
        let i = match self.relation_index.get(name) {
            Some(&i) => i,
            None => self
                .relations
                .iter()
                .position(|r| r.name.eq_ignore_ascii_case(name))?,
        };
        Some(&self.relations[i])
    }
    pub fn add_relation(&mut self, rel: Relation) {
        self.relation_index
            .insert(rel.name.clone(), self.relations.len());
        self.relations.push(rel);
    }
}
