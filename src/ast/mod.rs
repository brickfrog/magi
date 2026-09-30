//! Syntax tree: exactly what the analyst wrote, with spans. No execution decisions live here.

use crate::syntax::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
    /// Written as a backtick-quoted identifier.
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrLit {
    pub value: String,
    pub span: Span,
    pub block: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Import(ImportDecl),
    Connection(ConnectionDecl),
    Source(SourceDecl),
    Mapping(MappingDecl),
    Dataset(DatasetDecl),
    Validate(ValidateDecl),
    Reconcile(ReconcileDecl),
    Export(ExportDecl),
    Runtime(RuntimeDecl),
    Model(ModelDecl),
    Test(TestDecl),
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::Import(d) => d.span,
            Statement::Connection(d) => d.span,
            Statement::Source(d) => d.span,
            Statement::Mapping(d) => d.span,
            Statement::Dataset(d) => d.span,
            Statement::Validate(d) => d.span,
            Statement::Reconcile(d) => d.span,
            Statement::Export(d) => d.span,
            Statement::Runtime(d) => d.span,
            Statement::Model(d) => d.span,
            Statement::Test(d) => d.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDecl {
    pub path: StrLit,
    pub span: Span,
}

/// `key: value` inside a block.
#[derive(Debug, Clone, PartialEq)]
pub struct Opt {
    pub key: Ident,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionDecl {
    pub name: Ident,
    pub kind: Ident,
    pub options: Vec<Opt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeDecl {
    pub options: Vec<Opt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceDecl {
    pub name: Ident,
    /// `csv`, `parquet`, `excel`, `duckdb`, `sql`.
    pub kind: Ident,
    pub args: Vec<Expr>,
    pub options: Vec<Opt>,
    pub schema: Option<SchemaBlock>,
    pub identity: Option<Vec<Ident>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaBlock {
    pub fields: Vec<SchemaField>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaField {
    pub name: Ident,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeParam {
    Int(u32),
    Str(String),
}

/// `decimal(18,2)?`, `date("%m/%d/%Y")?`, `string`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeExpr {
    pub name: Ident,
    pub params: Vec<TypeParam>,
    pub nullable: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MappingDecl {
    pub name: Ident,
    pub arms: Vec<MapArm>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapPattern {
    /// One or more literal values: `"Gun"` or `["Gun", "Rifle"]`.
    Values(Vec<Expr>),
    Otherwise,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapValue {
    Expr(Expr),
    /// `otherwise => original`: keep the input value.
    Original,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapArm {
    pub pattern: MapPattern,
    pub value: MapValue,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatasetDecl {
    pub name: Ident,
    pub pipeline: Pipeline,
    pub span: Span,
}

/// `name` or `name.part` (reconciliation / validation outputs, source rejects).
#[derive(Debug, Clone, PartialEq)]
pub struct RelRef {
    pub name: Ident,
    pub part: Option<Ident>,
    pub span: Span,
}

impl RelRef {
    pub fn display(&self) -> String {
        match &self.part {
            Some(p) => format!("{}.{}", self.name.name, p.name),
            None => self.name.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineHead {
    Rel(RelRef),
    /// `native_sql { dialect: duckdb  query: """...""" }` escape hatch.
    NativeSql {
        options: Vec<Opt>,
        schema: Option<SchemaBlock>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub head: PipelineHead,
    pub steps: Vec<Step>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub kind: StepKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinOn {
    /// `on key1, key2`: equal-named columns on both sides, merged in the output.
    Keys(Vec<Ident>),
    /// `on a.x == b.y and ...`.
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assign {
    pub name: Ident,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Column(ColumnName),
    Assign(Assign),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rename {
    pub from: ColumnName,
    pub to: Ident,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MappingRef {
    Inline(Vec<MapArm>),
    Named(Ident),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepKind {
    Select(Vec<SelectItem>),
    Drop(Vec<ColumnName>),
    Rename(Vec<Rename>),
    Derive(Vec<Assign>),
    Filter(Expr),
    Join {
        kind: JoinKind,
        rel: RelRef,
        alias: Option<Ident>,
        on: JoinOn,
    },
    Group(Vec<Expr>),
    Aggregate(Vec<Assign>),
    Sort(Vec<SortKey>),
    Distinct,
    Union(RelRef),
    Limit(u64),
    Normalize {
        column: ColumnName,
        mapping: MappingRef,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckSeverity {
    /// Failing aborts the run before any export.
    Require,
    /// Failing is an error; outputs are still written and the run exits non-zero.
    Expect,
    /// Failing is reported as a warning.
    Warn,
}

impl CheckSeverity {
    pub fn keyword(self) -> &'static str {
        match self {
            CheckSeverity::Require => "require",
            CheckSeverity::Expect => "expect",
            CheckSeverity::Warn => "warn",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckKind {
    /// Row predicate, or aggregate predicate when it contains aggregate functions.
    Predicate(Expr),
    /// `require id not null`.
    NotNull(Expr),
    /// `expect unique(id, event_date)`.
    Unique(Vec<ColumnName>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub severity: CheckSeverity,
    pub kind: CheckKind,
    /// `as "label"`.
    pub label: Option<StrLit>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidateDecl {
    pub target: RelRef,
    pub checks: Vec<Check>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKey {
    /// Same column name on both sides.
    Same(Ident),
    /// `a.x == b.y` (either order).
    Pair(Expr, Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockSpec {
    Keys(Vec<BlockKey>),
    /// `block by none`: every remaining pair is a candidate.
    None(Span),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub name: Ident,
    /// `None` = the flag is raised for every match of the tier.
    pub when: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TierItem {
    Require(Expr),
    Rank(Vec<SortKey>, Span),
    Block(BlockSpec, Span),
    /// `many a to one b`, `many a to many b`, `one a to many b`.
    Shape {
        a_many: bool,
        b_many: bool,
        a_side: Ident,
        b_side: Ident,
        span: Span,
    },
    Group {
        side: Ident,
        keys: Vec<Ident>,
        span: Span,
    },
    /// `subset b max_items 5 [max_subsets 100000]`: bounded subset matching.
    Subset {
        side: Ident,
        max_items: i64,
        max_subsets: Option<i64>,
        span: Span,
    },
    Compare(Vec<Expr>, Span),
    Evidence(Vec<Assign>, Span),
    Flag(Flag),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TierDecl {
    pub name: Ident,
    pub items: Vec<TierItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileItem {
    Block(BlockSpec, Span),
    /// `cardinality one_to_one` etc.
    Cardinality(Ident, Span),
    /// `consume both|a|b|none`.
    Consume(Ident, Span),
    /// `identity a: a_ref` / `identity b: [x, y]`.
    Identity {
        side: Ident,
        cols: Vec<Ident>,
        span: Span,
    },
    /// `ambiguity hold|continue`.
    Ambiguity(Ident, Span),
    /// `duplicates hold|continue`.
    Duplicates(Ident, Span),
    Tier(TierDecl),
    Evidence(Vec<Assign>, Span),
    Flag(Flag),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileDecl {
    pub name: Ident,
    pub a: RelRef,
    pub a_alias: Option<Ident>,
    pub b: RelRef,
    pub b_alias: Option<Ident>,
    pub items: Vec<ReconcileItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportPart {
    /// Sheet (xlsx) or table (duckdb) name.
    pub name: StrLit,
    pub rel: RelRef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportTarget {
    Single(RelRef),
    /// `export to "report.xlsx" { sheet "Matches" = result.matches ... }`.
    Multi(Vec<ExportPart>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDecl {
    pub target: ExportTarget,
    pub path: StrLit,
    pub options: Vec<Opt>,
    pub span: Span,
}

// ---------------------------------------------------------------------------------------------
// expressions

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnName {
    pub qualifier: Option<Ident>,
    pub name: Ident,
    pub span: Span,
}

impl ColumnName {
    pub fn display(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{}.{}", q.name, self.name.name),
            None => self.name.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    /// Decimal literal as written (`0.02`).
    Decimal(String),
    Str(String),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Or => "or",
            BinaryOp::And => "and",
            BinaryOp::Eq => "==",
            BinaryOp::NotEq => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
        }
    }
    /// Binding power; higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::And => 2,
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => 4,
            BinaryOp::Add | BinaryOp::Sub => 5,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
        }
    }
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseArm {
    pub when: Expr,
    pub then: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Literal(Literal),
    Column(ColumnName),
    /// `f(x)` or namespaced `duckdb.f(x)`.
    Call {
        namespace: Option<Ident>,
        name: Ident,
        args: Vec<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `case { cond => value ... otherwise => value }`.
    Case {
        arms: Vec<CaseArm>,
        otherwise: Option<Box<Expr>>,
    },
    /// `[a, b, c]` (only valid as a function argument, e.g. format lists).
    List(Vec<Expr>),
    /// Parenthesised expression (kept so `fmt` can preserve grouping).
    Paren(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    /// Visit this expression and all sub-expressions.
    pub fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        f(self);
        match &self.kind {
            ExprKind::Literal(_) | ExprKind::Column(_) => {}
            ExprKind::Call { args, .. } | ExprKind::List(args) => {
                args.iter().for_each(|a| a.walk(f));
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::IsNull { expr, .. }
            | ExprKind::Paren(expr) => expr.walk(f),
            ExprKind::Binary { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            ExprKind::InList { expr, list, .. } => {
                expr.walk(f);
                list.iter().for_each(|a| a.walk(f));
            }
            ExprKind::Case { arms, otherwise } => {
                for arm in arms {
                    arm.when.walk(f);
                    arm.then.walk(f);
                }
                if let Some(o) = otherwise {
                    o.walk(f);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// BI semantic models

#[derive(Debug, Clone, PartialEq)]
pub enum ModelItem {
    /// `table sales { description: "..." }`: include a relation as a model table.
    Table {
        rel: Ident,
        options: Vec<Opt>,
        span: Span,
    },
    /// `relationship sales.customer_id -> customers.id` (many side -> one side).
    Relationship {
        from: ColumnName,
        to: ColumnName,
        span: Span,
    },
    /// `dimension region = customers.region { description: "..." }`.
    Dimension {
        name: Ident,
        column: ColumnName,
        options: Vec<Opt>,
        span: Span,
    },
    /// `metric revenue { value: sum(sales.amount)  format: currency  description: "..." }`.
    Metric {
        name: Ident,
        options: Vec<Opt>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelDecl {
    pub name: Ident,
    pub items: Vec<ModelItem>,
    pub span: Span,
}

// ---------------------------------------------------------------------------------------------
// tests (`magi test`)

#[derive(Debug, Clone, PartialEq)]
pub enum TestItem {
    /// `today: "2026-07-01"`: the date `today()` returns in this test.
    Today { date: StrLit, span: Span },
    /// `given bank = "cases/bank.csv"`: source `bank` as declared, read from another file.
    GivenPath {
        name: Ident,
        path: StrLit,
        span: Span,
    },
    /// `given offices = csv("cases/offices.csv") { ... }`: a whole new declaration of source
    /// `offices` (its span starts at `given`).
    GivenSource(SourceDecl),
    /// `expect r.matches == "cases/matches.csv"` (`file`) or `expect r.unmatched_a is empty`
    /// (no `file`).
    Expect {
        rel: RelRef,
        file: Option<StrLit>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestDecl {
    pub name: Ident,
    pub items: Vec<TestItem>,
    pub span: Span,
}
