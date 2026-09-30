//! Small structured SQL AST and renderer. All generated SQL goes through this
//! module: identifiers are always quoted and literals escaped, so no string concatenation of SQL
//! happens elsewhere. It covers exactly what the DuckDB backend emits.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Int(i64),
    Decimal(String),
    Str(String),
    Bool(bool),
    Null,
    Date(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Col {
        table: Option<String>,
        name: String,
    },
    Star {
        table: Option<String>,
    },
    Lit(Lit),
    Func {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        filter: Option<Box<Expr>>,
    },
    Bin {
        op: &'static str,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Case {
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        ty: String,
        try_: bool,
    },
    Window {
        func: Box<Expr>,
        partition: Vec<Expr>,
        order: Vec<OrderBy>,
    },
    /// DuckDB list literal `[a, b]`.
    List(Vec<Expr>),
    /// `lambda x: body`
    Lambda {
        param: String,
        body: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Named(String),
    /// Two-part name `catalog.table` (attached databases).
    Qualified(String, String),
    Sub(Box<Query>),
    /// Table function call, e.g. `read_csv('x', header = true)`.
    Func {
        name: String,
        args: Vec<Expr>,
        named: Vec<(String, Expr)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// `JOIN`, `LEFT JOIN`, `SEMI JOIN`, ...
    pub kind: &'static str,
    pub table: TableRef,
    pub alias: Option<String>,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<(Expr, Option<String>)>,
    pub from: Option<(TableRef, Option<String>)>,
    pub joins: Vec<Join>,
    pub where_: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Select(Box<Select>),
    /// `UNION ALL BY NAME` of the parts.
    UnionAllByName(Vec<Query>),
    /// Verbatim SQL (only the `native_sql` escape hatch).
    Raw(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub ctes: Vec<(String, Query)>,
    pub body: Body,
}

impl Query {
    pub fn select(s: Select) -> Query {
        Query {
            ctes: Vec::new(),
            body: Body::Select(Box::new(s)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    CreateTable {
        name: String,
        query: Query,
        temp: bool,
    },
    CreateView {
        name: String,
        query: Query,
    },
    CreateEmptyTable {
        name: String,
        columns: Vec<(String, String)>,
    },
    Insert {
        table: String,
        query: Query,
    },
    Drop {
        name: String,
        view: bool,
    },
    Copy {
        query: Query,
        path: String,
        options: Vec<(String, Expr)>,
    },
    Attach {
        path: String,
        alias: String,
        read_only: bool,
    },
    Detach {
        alias: String,
    },
    Use {
        database: String,
    },
    /// Setting e.g. `SET threads = 4`.
    Set {
        name: String,
        value: Expr,
    },
}

// ---------------------------------------------------------------------------------------------
// constructors

pub fn col(name: &str) -> Expr {
    Expr::Col {
        table: None,
        name: name.to_string(),
    }
}
pub fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func {
        name: name.to_string(),
        args,
        distinct: false,
        filter: None,
    }
}
pub fn str_lit(s: &str) -> Expr {
    Expr::Lit(Lit::Str(s.to_string()))
}
pub fn int(v: i64) -> Expr {
    Expr::Lit(Lit::Int(v))
}
pub fn bin(op: &'static str, l: Expr, r: Expr) -> Expr {
    Expr::Bin {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}

// ---------------------------------------------------------------------------------------------
// rendering

pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn is_plain_fn(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn render_expr(e: &Expr) -> String {
    let mut s = String::new();
    expr_into(&mut s, e);
    s
}

fn precedence(op: &str) -> u8 {
    match op {
        "OR" => 1,
        "AND" => 2,
        "=" | "<>" | "<" | "<=" | ">" | ">=" | "IS NOT DISTINCT FROM" => 4,
        "+" | "-" | "||" => 5,
        "*" | "/" | "%" => 6,
        _ => 7,
    }
}

fn operand(out: &mut String, e: &Expr, parent: u8) {
    let needs = match e {
        Expr::Bin { op, .. } => precedence(op) <= parent,
        Expr::Not(_) | Expr::IsNull { .. } | Expr::InList { .. } => true,
        _ => false,
    };
    if needs {
        out.push('(');
        expr_into(out, e);
        out.push(')');
    } else {
        expr_into(out, e);
    }
}

fn list(out: &mut String, xs: &[Expr]) {
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        expr_into(out, x);
    }
}

fn order_list(out: &mut String, keys: &[OrderBy]) {
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        expr_into(out, &k.expr);
        out.push_str(if k.desc {
            " DESC NULLS LAST"
        } else {
            " ASC NULLS LAST"
        });
    }
}

fn expr_into(out: &mut String, e: &Expr) {
    match e {
        Expr::Col { table, name } => {
            if let Some(t) = table {
                out.push_str(&ident(t));
                out.push('.');
            }
            out.push_str(&ident(name));
        }
        Expr::Star { table } => {
            if let Some(t) = table {
                out.push_str(&ident(t));
                out.push('.');
            }
            out.push('*');
        }
        Expr::Lit(l) => match l {
            Lit::Int(v) => {
                let _ = write!(out, "{v}");
            }
            Lit::Decimal(d) => out.push_str(d),
            Lit::Str(s) => out.push_str(&string(s)),
            Lit::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
            Lit::Null => out.push_str("NULL"),
            Lit::Date(d) => {
                out.push_str("DATE ");
                out.push_str(&string(d));
            }
        },
        Expr::Func {
            name,
            args,
            distinct,
            filter,
        } => {
            if is_plain_fn(name) {
                out.push_str(name);
            } else {
                out.push_str(&ident(name));
            }
            out.push('(');
            if *distinct {
                out.push_str("DISTINCT ");
            }
            list(out, args);
            out.push(')');
            if let Some(f) = filter {
                out.push_str(" FILTER (WHERE ");
                expr_into(out, f);
                out.push(')');
            }
        }
        Expr::Bin { op, left, right } => {
            let p = precedence(op);
            // comparisons do not associate: `(a > 0) = b` keeps its parentheses
            let left_parent = if p == precedence("=") { p } else { p - 1 };
            operand(out, left, left_parent);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            operand(out, right, p);
        }
        Expr::Not(x) => {
            out.push_str("NOT ");
            operand(out, x, 3);
        }
        Expr::Neg(x) => {
            // only an atom follows `-` bare: `--x` would start a comment
            let atomic = match x.as_ref() {
                Expr::Lit(Lit::Int(v)) => *v >= 0,
                Expr::Lit(Lit::Decimal(d)) => !d.starts_with('-'),
                Expr::Lit(_)
                | Expr::Col { .. }
                | Expr::Func { .. }
                | Expr::Cast { .. }
                | Expr::Window { .. }
                | Expr::List(_) => true,
                _ => false,
            };
            out.push('-');
            if atomic {
                expr_into(out, x);
            } else {
                out.push('(');
                expr_into(out, x);
                out.push(')');
            }
        }
        Expr::IsNull { expr, negated } => {
            operand(out, expr, 4);
            out.push_str(if *negated { " IS NOT NULL" } else { " IS NULL" });
        }
        Expr::InList {
            expr,
            list: items,
            negated,
        } => {
            operand(out, expr, 4);
            out.push_str(if *negated { " NOT IN (" } else { " IN (" });
            list(out, items);
            out.push(')');
        }
        Expr::Case { whens, otherwise } => {
            out.push_str("CASE");
            for (w, t) in whens {
                out.push_str(" WHEN ");
                expr_into(out, w);
                out.push_str(" THEN ");
                expr_into(out, t);
            }
            if let Some(o) = otherwise {
                out.push_str(" ELSE ");
                expr_into(out, o);
            }
            out.push_str(" END");
        }
        Expr::Cast { expr, ty, try_ } => {
            out.push_str(if *try_ { "TRY_CAST(" } else { "CAST(" });
            expr_into(out, expr);
            out.push_str(" AS ");
            out.push_str(ty);
            out.push(')');
        }
        Expr::Window {
            func,
            partition,
            order,
        } => {
            expr_into(out, func);
            out.push_str(" OVER (");
            if !partition.is_empty() {
                out.push_str("PARTITION BY ");
                list(out, partition);
            }
            if !order.is_empty() {
                if !partition.is_empty() {
                    out.push(' ');
                }
                out.push_str("ORDER BY ");
                order_list(out, order);
            }
            out.push(')');
        }
        Expr::List(items) => {
            out.push('[');
            list(out, items);
            out.push(']');
        }
        Expr::Lambda { param, body } => {
            out.push_str("lambda ");
            out.push_str(param);
            out.push_str(": ");
            expr_into(out, body);
        }
    }
}

fn table_ref(out: &mut String, t: &TableRef, indent: usize) {
    match t {
        TableRef::Named(n) => out.push_str(&ident(n)),
        TableRef::Qualified(c, n) => {
            out.push_str(&ident(c));
            out.push('.');
            out.push_str(&ident(n));
        }
        TableRef::Sub(q) => {
            out.push_str("(\n");
            query_into(out, q, indent + 1);
            out.push('\n');
            out.push_str(&"  ".repeat(indent));
            out.push(')');
        }
        TableRef::Func { name, args, named } => {
            out.push_str(name);
            out.push('(');
            list(out, args);
            for (k, v) in named {
                out.push_str(", ");
                out.push_str(k);
                out.push_str(" = ");
                expr_into(out, v);
            }
            out.push(')');
        }
    }
}

fn select_into(out: &mut String, s: &Select, indent: usize) {
    let pad = "  ".repeat(indent);
    out.push_str(&pad);
    out.push_str(if s.distinct {
        "SELECT DISTINCT "
    } else {
        "SELECT "
    });
    let multiline = s.items.len() > 4;
    for (i, (e, alias)) in s.items.iter().enumerate() {
        if i > 0 {
            if multiline {
                out.push_str(",\n");
                out.push_str(&pad);
                out.push_str("       ");
            } else {
                out.push_str(", ");
            }
        }
        expr_into(out, e);
        if let Some(a) = alias {
            let redundant = matches!(e, Expr::Col { name, .. } if name == a);
            if !redundant {
                out.push_str(" AS ");
                out.push_str(&ident(a));
            }
        }
    }
    if let Some((t, alias)) = &s.from {
        out.push('\n');
        out.push_str(&pad);
        out.push_str("FROM ");
        table_ref(out, t, indent);
        if let Some(a) = alias {
            out.push_str(" AS ");
            out.push_str(&ident(a));
        }
    }
    for j in &s.joins {
        out.push('\n');
        out.push_str(&pad);
        out.push_str(j.kind);
        out.push(' ');
        table_ref(out, &j.table, indent);
        if let Some(a) = &j.alias {
            out.push_str(" AS ");
            out.push_str(&ident(a));
        }
        if let Some(on) = &j.on {
            out.push_str(" ON ");
            expr_into(out, on);
        }
    }
    if let Some(w) = &s.where_ {
        out.push('\n');
        out.push_str(&pad);
        out.push_str("WHERE ");
        expr_into(out, w);
    }
    if !s.group_by.is_empty() {
        out.push('\n');
        out.push_str(&pad);
        out.push_str("GROUP BY ");
        list(out, &s.group_by);
    }
    if !s.order_by.is_empty() {
        out.push('\n');
        out.push_str(&pad);
        out.push_str("ORDER BY ");
        order_list(out, &s.order_by);
    }
    if let Some(n) = s.limit {
        out.push('\n');
        out.push_str(&pad);
        let _ = write!(out, "LIMIT {n}");
    }
}

fn query_into(out: &mut String, q: &Query, indent: usize) {
    let pad = "  ".repeat(indent);
    if !q.ctes.is_empty() {
        out.push_str(&pad);
        out.push_str("WITH ");
        for (i, (name, cte)) in q.ctes.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
                out.push_str(&pad);
            }
            out.push_str(&ident(name));
            out.push_str(" AS (\n");
            query_into(out, cte, indent + 1);
            out.push('\n');
            out.push_str(&pad);
            out.push(')');
        }
        out.push('\n');
    }
    match &q.body {
        Body::Select(s) => select_into(out, s, indent),
        Body::UnionAllByName(parts) => {
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    out.push_str(&pad);
                    out.push_str("UNION ALL BY NAME\n");
                }
                if p.ctes.is_empty() && matches!(p.body, Body::Select(_)) {
                    query_into(out, p, indent);
                } else {
                    out.push_str(&pad);
                    out.push_str("SELECT * FROM (\n");
                    query_into(out, p, indent + 1);
                    out.push('\n');
                    out.push_str(&pad);
                    out.push(')');
                }
            }
        }
        // verbatim: re-indenting would change multi-line string literals in the user's SQL
        Body::Raw(sql) => {
            out.push_str(&pad);
            out.push_str(sql.trim());
        }
    }
}

pub fn render_query(q: &Query) -> String {
    let mut s = String::new();
    query_into(&mut s, q, 0);
    s
}

pub fn render(stmt: &Stmt) -> String {
    let mut s = String::new();
    match stmt {
        Stmt::CreateTable { name, query, temp } => {
            let _ = writeln!(
                s,
                "CREATE OR REPLACE {}TABLE {} AS",
                if *temp { "TEMP " } else { "" },
                ident(name)
            );
            query_into(&mut s, query, 0);
        }
        Stmt::CreateView { name, query } => {
            let _ = writeln!(s, "CREATE OR REPLACE TEMP VIEW {} AS", ident(name));
            query_into(&mut s, query, 0);
        }
        Stmt::CreateEmptyTable { name, columns } => {
            let cols = columns
                .iter()
                .map(|(n, t)| format!("{} {t}", ident(n)))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(s, "CREATE OR REPLACE TEMP TABLE {} ({cols})", ident(name));
        }
        Stmt::Insert { table, query } => {
            let _ = writeln!(s, "INSERT INTO {} BY NAME", ident(table));
            query_into(&mut s, query, 0);
        }
        Stmt::Drop { name, view } => {
            let _ = write!(
                s,
                "DROP {} IF EXISTS {}",
                if *view { "VIEW" } else { "TABLE" },
                ident(name)
            );
        }
        Stmt::Copy {
            query,
            path,
            options,
        } => {
            s.push_str("COPY (\n");
            query_into(&mut s, query, 1);
            let _ = write!(s, "\n) TO {}", string(path));
            if !options.is_empty() {
                s.push_str(" (");
                for (i, (k, v)) in options.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(k);
                    s.push(' ');
                    expr_into(&mut s, v);
                }
                s.push(')');
            }
        }
        Stmt::Attach {
            path,
            alias,
            read_only,
        } => {
            let _ = write!(
                s,
                "ATTACH {} AS {}{}",
                string(path),
                ident(alias),
                if *read_only { " (READ_ONLY)" } else { "" }
            );
        }
        Stmt::Detach { alias } => {
            let _ = write!(s, "DETACH {}", ident(alias));
        }
        Stmt::Use { database } => {
            let _ = write!(s, "USE {}", ident(database));
        }
        Stmt::Set { name, value } => {
            let _ = write!(s, "SET {name} = ");
            expr_into(&mut s, value);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers_and_strings() {
        let e = bin("=", col("we\"ird"), str_lit("it's"));
        assert_eq!(render_expr(&e), "\"we\"\"ird\" = 'it''s'");
    }

    #[test]
    fn parenthesises_by_precedence() {
        let e = bin("*", bin("+", col("a"), col("b")), col("c"));
        assert_eq!(render_expr(&e), "(\"a\" + \"b\") * \"c\"");
        let e = bin("-", col("a"), bin("-", col("b"), col("c")));
        assert_eq!(render_expr(&e), "\"a\" - (\"b\" - \"c\")");
        let e = bin(
            "AND",
            Expr::Not(Box::new(col("x"))),
            bin("OR", col("y"), col("z")),
        );
        assert_eq!(render_expr(&e), "(NOT \"x\") AND (\"y\" OR \"z\")");
    }

    #[test]
    fn negation_never_renders_a_comment_marker() {
        let neg = |e: Expr| Expr::Neg(Box::new(e));
        assert_eq!(render_expr(&neg(neg(col("x")))), "-(-\"x\")");
        assert_eq!(render_expr(&neg(int(-5))), "-(-5)");
        assert_eq!(
            render_expr(&neg(Expr::Lit(Lit::Decimal("-1.5".into())))),
            "-(-1.5)"
        );
        assert_eq!(
            render_expr(&neg(bin("+", col("a"), int(1)))),
            "-(\"a\" + 1)"
        );
        assert_eq!(render_expr(&neg(col("x"))), "-\"x\"");
    }

    #[test]
    fn nested_comparisons_keep_their_parentheses() {
        let e = bin("=", bin(">", col("a"), int(0)), col("b"));
        assert_eq!(render_expr(&e), "(\"a\" > 0) = \"b\"");
        let e = bin("=", col("b"), bin(">", col("a"), int(0)));
        assert_eq!(render_expr(&e), "\"b\" = (\"a\" > 0)");
        // arithmetic still associates to the left without parentheses
        let e = bin("-", bin("-", col("a"), col("b")), col("c"));
        assert_eq!(render_expr(&e), "\"a\" - \"b\" - \"c\"");
    }

    #[test]
    fn raw_sql_is_rendered_verbatim_inside_a_subquery() {
        let raw = Query {
            ctes: Vec::new(),
            body: Body::Raw("SELECT 'a\nb' AS v".into()),
        };
        let q = Query::select(Select {
            items: vec![(Expr::Star { table: None }, None)],
            from: Some((TableRef::Sub(Box::new(raw)), None)),
            ..Select::default()
        });
        assert!(
            render_query(&q).contains("SELECT 'a\nb' AS v"),
            "{}",
            render_query(&q)
        );
    }
}
