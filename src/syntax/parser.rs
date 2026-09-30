//! Recursive-descent parser producing the AST. Keywords are contextual: a word is a keyword only
//! where the grammar expects one, so columns may be called `type`, `group` or `sort`.

use super::span::{FileId, Span};
use super::token::{Comment, Tok, Token, lex};
use crate::ast::*;
use crate::diagnostic::Diagnostic;

pub struct Parsed {
    pub program: Program,
    pub comments: Vec<Comment>,
    pub diagnostics: Vec<Diagnostic>,
    /// Names declared by statements that failed to parse (when the parser got as far as the
    /// name). The parse error already explains them, so later stages stay silent about them.
    pub failed_names: Vec<String>,
}

pub fn parse(file: FileId, src: &str) -> Parsed {
    let lexed = lex(file, src);
    let mut p = Parser {
        tokens: lexed.tokens,
        pos: 0,
    };
    let mut program = Program::default();
    let mut diagnostics = lexed.diagnostics;
    let mut failed_names = Vec::new();
    while !p.at_eof() {
        let start = p.pos;
        let result = p.statement();
        // Input the lexer already reported leaves a `Tok::Error`. A statement that consumed one,
        // or stopped at one on its own line, is damaged: it is dropped like a failed statement,
        // and a parse error at the bad token would only repeat the lexer's diagnostic. A
        // statement that ends at a line break before the bad token is complete and stands.
        let at_bad = p.at(&Tok::Error);
        let damaged = p.tokens[start..p.pos].iter().any(|t| t.tok == Tok::Error)
            || (at_bad && !p.tokens[p.pos].newline_before);
        match result {
            Ok(s) if !damaged => program.statements.push(s),
            result => {
                if let Err(d) = result
                    && !at_bad
                {
                    diagnostics.push(d);
                }
                failed_names.extend(p.declared_name(start));
                p.recover(start);
            }
        }
    }
    // Lexer and parser diagnostics in source order.
    diagnostics.sort_by_key(|d| d.labels.first().map(|l| l.span.start));
    Parsed {
        program,
        comments: lexed.comments,
        diagnostics,
        failed_names,
    }
}

/// Parse a standalone expression.
#[cfg(test)]
pub fn parse_expr(file: FileId, src: &str) -> Result<Expr, Diagnostic> {
    let lexed = lex(file, src);
    if let Some(d) = lexed.diagnostics.into_iter().next() {
        return Err(d);
    }
    let mut p = Parser {
        tokens: lexed.tokens,
        pos: 0,
    };
    let e = p.expr()?;
    if !p.at_eof() {
        return Err(p.unexpected("end of expression"));
    }
    Ok(e)
}

const STATEMENT_KEYWORDS: &[&str] = &[
    "import",
    "connection",
    "source",
    "mapping",
    "dataset",
    "validate",
    "reconcile",
    "export",
    "runtime",
    "model",
    "test",
];

/// Statement keywords followed by the name the statement declares.
const DECLARING_KEYWORDS: &[&str] = &[
    "connection",
    "source",
    "mapping",
    "dataset",
    "reconcile",
    "model",
];

type PResult<T> = Result<T, Diagnostic>;

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    // ---- token helpers ------------------------------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }
    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }
    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }
    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, tok: &Tok) -> PResult<Span> {
        if self.at(tok) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected(&format!("`{}`", tok.symbol())))
        }
    }
    fn word_at(&self, n: usize) -> Option<&str> {
        match self.peek_at(n) {
            Tok::Ident {
                name,
                quoted: false,
            } => Some(name.as_str()),
            _ => None,
        }
    }
    fn at_word(&self, w: &str) -> bool {
        self.word_at(0) == Some(w)
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, w: &str) -> PResult<Span> {
        if self.at_word(w) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected(&format!("`{w}`")))
        }
    }
    fn unexpected(&self, expected: &str) -> Diagnostic {
        let found = self.peek().describe();
        Diagnostic::error("M001", format!("expected {expected}, found {found}"))
            .label(self.span(), format!("expected {expected}"))
    }
    /// Skip optional separators between block items.
    fn skip_separators(&mut self) {
        while matches!(self.peek(), Tok::Comma | Tok::Semi) {
            self.bump();
        }
    }

    /// After an error in the statement starting at token `start`: skip to the next line that
    /// starts with a statement keyword at brace depth 0.
    fn recover(&mut self, start: usize) {
        let mut depth: i32 = 0;
        self.pos = start;
        self.bump();
        while !self.at_eof() {
            match self.peek() {
                Tok::LBrace => depth += 1,
                Tok::RBrace => depth = (depth - 1).max(0),
                _ => {}
            }
            let t = &self.tokens[self.pos];
            if depth == 0
                && t.newline_before
                && self
                    .word_at(0)
                    .is_some_and(|w| STATEMENT_KEYWORDS.contains(&w))
            {
                return;
            }
            self.bump();
        }
    }

    /// The name declared by the statement starting at token `start` (`source x = ...`), if the
    /// statement is a declaration and has a name.
    fn declared_name(&self, start: usize) -> Option<String> {
        let keyword = match &self.tokens[start].tok {
            Tok::Ident {
                name,
                quoted: false,
            } => name.as_str(),
            _ => return None,
        };
        if !DECLARING_KEYWORDS.contains(&keyword) {
            return None;
        }
        match &self.tokens.get(start + 1)?.tok {
            Tok::Ident { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    fn ident(&mut self) -> PResult<Ident> {
        match self.peek().clone() {
            Tok::Ident { name, quoted } => {
                let span = self.bump().span;
                Ok(Ident { name, span, quoted })
            }
            _ => Err(self.unexpected("a name")),
        }
    }
    fn string(&mut self) -> PResult<StrLit> {
        match self.peek().clone() {
            Tok::Str(value) => Ok(StrLit {
                value,
                span: self.bump().span,
                block: false,
            }),
            Tok::BlockStr(value) => Ok(StrLit {
                value,
                span: self.bump().span,
                block: true,
            }),
            _ => Err(self.unexpected("a string")),
        }
    }
    /// An integer literal; `what` describes it in the error.
    fn integer(&mut self, what: &str) -> PResult<i64> {
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Ok(v)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    // ---- statements ---------------------------------------------------------------------------

    fn statement(&mut self) -> PResult<Statement> {
        let start = self.span();
        let Some(word) = self.word_at(0).map(str::to_string) else {
            return Err(self.unexpected("a statement (`source`, `dataset`, `reconcile`, ...)"));
        };
        let stmt = match word.as_str() {
            "import" => {
                self.bump();
                let path = self.string()?;
                Statement::Import(ImportDecl {
                    span: start.to(path.span),
                    path,
                })
            }
            "connection" => {
                self.bump();
                let name = self.ident()?;
                self.expect(&Tok::Assign)?;
                let kind = self.ident()?;
                let options = self.option_block()?;
                Statement::Connection(ConnectionDecl {
                    name,
                    kind,
                    options,
                    span: start.to(self.prev_span()),
                })
            }
            "runtime" => {
                self.bump();
                let options = self.option_block()?;
                Statement::Runtime(RuntimeDecl {
                    options,
                    span: start.to(self.prev_span()),
                })
            }
            "source" => self.source(start)?,
            "mapping" => {
                self.bump();
                let name = self.ident()?;
                self.expect(&Tok::LBrace)?;
                let arms = self.map_arms()?;
                Statement::Mapping(MappingDecl {
                    name,
                    arms,
                    span: start.to(self.prev_span()),
                })
            }
            "dataset" => {
                self.bump();
                let name = self.ident()?;
                self.expect(&Tok::Assign)?;
                let pipeline = self.pipeline()?;
                Statement::Dataset(DatasetDecl {
                    name,
                    pipeline,
                    span: start.to(self.prev_span()),
                })
            }
            "validate" => self.validate(start)?,
            "reconcile" => self.reconcile(start)?,
            "export" => self.export(start)?,
            "model" => self.model(start)?,
            "test" => self.test(start)?,
            _ => {
                let mut d = self.unexpected("a statement (`source`, `dataset`, `reconcile`, ...)");
                if let Some(s) =
                    crate::diagnostic::did_you_mean(&word, STATEMENT_KEYWORDS.iter().copied())
                {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                return Err(d);
            }
        };
        Ok(stmt)
    }

    fn option(&mut self) -> PResult<Opt> {
        let key = self.ident()?;
        self.expect(&Tok::Colon)?;
        let value = self.expr()?;
        Ok(Opt {
            span: key.span.to(value.span),
            key,
            value,
        })
    }

    /// `{ key: value ... }`
    fn option_block(&mut self) -> PResult<Vec<Opt>> {
        self.expect(&Tok::LBrace)?;
        let mut opts = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                return Ok(opts);
            }
            opts.push(self.option()?);
        }
    }

    fn source(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::Assign)?;
        Ok(Statement::Source(self.source_decl(start, name)?))
    }

    /// A source declaration after `name =`: `kind(args) { options }` (also a test's `given`).
    fn source_decl(&mut self, start: Span, name: Ident) -> PResult<SourceDecl> {
        let kind = self.ident()?;
        self.expect(&Tok::LParen)?;
        let args = self.expr_list(&Tok::RParen).map_err(|d| {
            // `excel("x.xlsx", sheet: "FY26")`: an option written as an argument
            if self.at(&Tok::Colon)
                && matches!(self.tokens[self.pos - 1].tok, Tok::Ident { .. })
            {
                d.help(
                    "options go in a `{ }` block after the parentheses, e.g. `excel(\"cases.xlsx\") { sheet: \"FY26\" }`",
                )
            } else {
                d
            }
        })?;
        let mut options = Vec::new();
        let mut schema = None;
        let mut identity = None;
        if self.eat(&Tok::LBrace) {
            loop {
                self.skip_separators();
                if self.eat(&Tok::RBrace) {
                    break;
                }
                if self.at_word("schema") && self.peek_at(1) == &Tok::LBrace {
                    let s = self.bump().span;
                    schema = Some(self.schema_block(s)?);
                } else if self.at_word("identity") && self.peek_at(1) != &Tok::Colon {
                    self.bump();
                    identity = Some(self.ident_or_list()?);
                } else {
                    options.push(self.option()?);
                }
            }
        }
        Ok(SourceDecl {
            name,
            kind,
            args,
            options,
            schema,
            identity,
            span: start.to(self.prev_span()),
        })
    }

    fn test(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::LBrace)?;
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let s = self.span();
            if self.at_word("today") && self.peek_at(1) == &Tok::Colon {
                self.bump();
                self.bump();
                let date = self.string()?;
                items.push(TestItem::Today {
                    span: s.to(date.span),
                    date,
                });
            } else if self.eat_word("given") {
                let name = self.ident()?;
                self.expect(&Tok::Assign)?;
                if matches!(self.peek(), Tok::Str(_) | Tok::BlockStr(_)) {
                    let path = self.string()?;
                    items.push(TestItem::GivenPath {
                        span: s.to(path.span),
                        name,
                        path,
                    });
                } else {
                    items.push(TestItem::GivenSource(self.source_decl(s, name)?));
                }
            } else if self.eat_word("expect") {
                let rel = self.rel_ref()?;
                let file = if self.eat(&Tok::EqEq) {
                    Some(self.string()?)
                } else {
                    self.expect_word("is")?;
                    self.expect_word("empty")?;
                    None
                };
                items.push(TestItem::Expect {
                    rel,
                    file,
                    span: s.to(self.prev_span()),
                });
            } else {
                return Err(self.unexpected("a test item (`today:`, `given` or `expect`)"));
            }
        }
        Ok(Statement::Test(TestDecl {
            name,
            items,
            span: start.to(self.prev_span()),
        }))
    }

    fn schema_block(&mut self, start: Span) -> PResult<SchemaBlock> {
        self.expect(&Tok::LBrace)?;
        let mut fields = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let name = self.ident()?;
            self.expect(&Tok::Colon)?;
            let ty = self.type_expr()?;
            fields.push(SchemaField {
                span: name.span.to(ty.span),
                name,
                ty,
            });
        }
        Ok(SchemaBlock {
            fields,
            span: start.to(self.prev_span()),
        })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let name = self.ident()?;
        let mut params = Vec::new();
        if self.eat(&Tok::LParen) {
            loop {
                match self.peek().clone() {
                    Tok::Int(v) if v >= 0 && v <= u32::MAX as i64 => {
                        self.bump();
                        params.push(TypeParam::Int(v as u32));
                    }
                    Tok::Str(s) => {
                        self.bump();
                        params.push(TypeParam::Str(s));
                    }
                    _ => return Err(self.unexpected("a type parameter (number or format string)")),
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
        }
        let nullable = self.eat(&Tok::Question);
        Ok(TypeExpr {
            span: name.span.to(self.prev_span()),
            name,
            params,
            nullable,
        })
    }

    fn ident_or_list(&mut self) -> PResult<Vec<Ident>> {
        if self.eat(&Tok::LBracket) {
            let mut out = Vec::new();
            loop {
                out.push(self.ident()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RBracket)?;
            Ok(out)
        } else {
            Ok(vec![self.ident()?])
        }
    }

    fn ident_list(&mut self) -> PResult<Vec<Ident>> {
        let mut out = vec![self.ident()?];
        while self.eat(&Tok::Comma) {
            out.push(self.ident()?);
        }
        Ok(out)
    }

    /// Arms up to and including the closing `}`.
    fn map_arms(&mut self) -> PResult<Vec<MapArm>> {
        let mut arms = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                return Ok(arms);
            }
            let start = self.span();
            let pattern = if self.eat_word("otherwise") {
                MapPattern::Otherwise
            } else if self.at(&Tok::LBracket) {
                self.bump();
                let values = self.expr_list(&Tok::RBracket)?;
                MapPattern::Values(values)
            } else {
                MapPattern::Values(vec![self.primary()?])
            };
            self.expect(&Tok::FatArrow)?;
            let value =
                if self.at_word("original") && !matches!(self.peek_at(1), Tok::LParen | Tok::Dot) {
                    self.bump();
                    MapValue::Original
                } else {
                    MapValue::Expr(self.expr()?)
                };
            arms.push(MapArm {
                pattern,
                value,
                span: start.to(self.prev_span()),
            });
        }
    }

    // ---- pipelines ----------------------------------------------------------------------------

    fn rel_ref(&mut self) -> PResult<RelRef> {
        let name = self.ident()?;
        let part = if self.at(&Tok::Dot) && matches!(self.peek_at(1), Tok::Ident { .. }) {
            self.bump();
            Some(self.ident()?)
        } else {
            None
        };
        let span = part.as_ref().map_or(name.span, |p| name.span.to(p.span));
        Ok(RelRef { name, part, span })
    }

    fn pipeline(&mut self) -> PResult<Pipeline> {
        let start = self.span();
        let head = if self.at_word("native_sql") && self.peek_at(1) == &Tok::LBrace {
            let s = self.bump().span;
            self.expect(&Tok::LBrace)?;
            let mut options = Vec::new();
            let mut schema = None;
            loop {
                self.skip_separators();
                if self.eat(&Tok::RBrace) {
                    break;
                }
                if self.at_word("schema") && self.peek_at(1) == &Tok::LBrace {
                    let ss = self.bump().span;
                    schema = Some(self.schema_block(ss)?);
                } else {
                    options.push(self.option()?);
                }
            }
            PipelineHead::NativeSql {
                options,
                schema,
                span: s.to(self.prev_span()),
            }
        } else {
            PipelineHead::Rel(self.rel_ref()?)
        };
        let mut steps = Vec::new();
        while self.eat(&Tok::Pipe) {
            steps.push(self.step()?);
        }
        Ok(Pipeline {
            head,
            steps,
            span: start.to(self.prev_span()),
        })
    }

    fn step(&mut self) -> PResult<Step> {
        let start = self.span();
        let Some(word) = self.word_at(0).map(str::to_string) else {
            return Err(self.unexpected("a pipeline step (`filter`, `derive`, `join`, ...)"));
        };
        let kind = match word.as_str() {
            "select" => {
                self.bump();
                StepKind::Select(self.select_items()?)
            }
            "drop" => {
                self.bump();
                let mut cols = vec![self.column_name()?];
                while self.eat(&Tok::Comma) {
                    cols.push(self.column_name()?);
                }
                StepKind::Drop(cols)
            }
            "rename" => {
                self.bump();
                let braced = self.eat(&Tok::LBrace);
                let mut renames = Vec::new();
                loop {
                    if braced {
                        self.skip_separators();
                        if self.eat(&Tok::RBrace) {
                            break;
                        }
                    }
                    let from = self.column_name()?;
                    self.expect(&Tok::Arrow)?;
                    let to = self.ident()?;
                    renames.push(Rename {
                        span: from.span.to(to.span),
                        from,
                        to,
                    });
                    if !braced && !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                StepKind::Rename(renames)
            }
            "derive" => {
                self.bump();
                StepKind::Derive(self.assignments()?)
            }
            "filter" => {
                self.bump();
                StepKind::Filter(self.expr()?)
            }
            "join" | "left" | "right" | "full" | "inner" => {
                let kind = match word.as_str() {
                    "left" => JoinKind::Left,
                    "right" => JoinKind::Right,
                    "full" => JoinKind::Full,
                    _ => JoinKind::Inner,
                };
                if word != "join" {
                    self.bump();
                }
                self.expect_word("join")?;
                let rel = self.rel_ref()?;
                let alias = if self.eat_word("as") {
                    Some(self.ident()?)
                } else {
                    None
                };
                self.expect_word("on")?;
                let first = self.expr()?;
                let on = match &first.kind {
                    ExprKind::Column(c) if c.qualifier.is_none() => {
                        let mut keys = vec![c.name.clone()];
                        while self.eat(&Tok::Comma) {
                            keys.push(self.ident()?);
                        }
                        JoinOn::Keys(keys)
                    }
                    _ => JoinOn::Expr(first),
                };
                StepKind::Join {
                    kind,
                    rel,
                    alias,
                    on,
                }
            }
            "group" => {
                self.bump();
                self.expect_word("by")?;
                let mut keys = vec![self.expr()?];
                while self.eat(&Tok::Comma) {
                    keys.push(self.expr()?);
                }
                StepKind::Group(keys)
            }
            "aggregate" => {
                self.bump();
                StepKind::Aggregate(self.assignments()?)
            }
            "sort" => {
                self.bump();
                self.eat_word("by");
                StepKind::Sort(self.sort_keys()?)
            }
            "distinct" => {
                self.bump();
                StepKind::Distinct
            }
            "union" => {
                self.bump();
                StepKind::Union(self.rel_ref()?)
            }
            "limit" => {
                self.bump();
                match self.peek().clone() {
                    Tok::Int(n) if n >= 0 => {
                        self.bump();
                        StepKind::Limit(n as u64)
                    }
                    _ => return Err(self.unexpected("a row count")),
                }
            }
            "normalize" => {
                self.bump();
                let column = self.column_name()?;
                let mapping = if self.eat_word("with") {
                    MappingRef::Named(self.ident()?)
                } else {
                    self.expect(&Tok::LBrace)?;
                    MappingRef::Inline(self.map_arms()?)
                };
                StepKind::Normalize { column, mapping }
            }
            _ => {
                const STEPS: &[&str] = &[
                    "select",
                    "drop",
                    "rename",
                    "derive",
                    "filter",
                    "join",
                    "group",
                    "aggregate",
                    "sort",
                    "distinct",
                    "union",
                    "limit",
                    "normalize",
                ];
                let mut d = self.unexpected("a pipeline step (`filter`, `derive`, `join`, ...)");
                if let Some(s) = crate::diagnostic::did_you_mean(&word, STEPS.iter().copied()) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                return Err(d);
            }
        };
        Ok(Step {
            kind,
            span: start.to(self.prev_span()),
        })
    }

    fn select_items(&mut self) -> PResult<Vec<SelectItem>> {
        let braced = self.eat(&Tok::LBrace);
        let mut items = Vec::new();
        loop {
            if braced {
                self.skip_separators();
                if self.eat(&Tok::RBrace) {
                    break;
                }
            }
            if matches!(self.peek(), Tok::Ident { .. }) && self.peek_at(1) == &Tok::Assign {
                items.push(SelectItem::Assign(self.assignment()?));
            } else {
                items.push(SelectItem::Column(self.column_name()?));
            }
            if !braced && !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn assignment(&mut self) -> PResult<Assign> {
        let name = self.ident()?;
        self.expect(&Tok::Assign)?;
        let expr = self.expr()?;
        Ok(Assign {
            span: name.span.to(expr.span),
            name,
            expr,
        })
    }

    /// `x = e, y = e` or `{ x = e  y = e }`.
    fn assignments(&mut self) -> PResult<Vec<Assign>> {
        let braced = self.eat(&Tok::LBrace);
        let mut out = Vec::new();
        loop {
            if braced {
                self.skip_separators();
                if self.eat(&Tok::RBrace) {
                    break;
                }
            }
            out.push(self.assignment()?);
            if !braced && !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn sort_keys(&mut self) -> PResult<Vec<SortKey>> {
        let mut keys = Vec::new();
        loop {
            let expr = self.expr()?;
            let desc = if self.eat_word("desc") {
                true
            } else {
                self.eat_word("asc");
                false
            };
            keys.push(SortKey {
                span: expr.span.to(self.prev_span()),
                expr,
                desc,
            });
            if !self.eat(&Tok::Comma) {
                return Ok(keys);
            }
        }
    }

    fn column_name(&mut self) -> PResult<ColumnName> {
        let first = self.ident()?;
        if self.at(&Tok::Dot) && matches!(self.peek_at(1), Tok::Ident { .. }) {
            self.bump();
            let name = self.ident()?;
            Ok(ColumnName {
                span: first.span.to(name.span),
                qualifier: Some(first),
                name,
            })
        } else {
            Ok(ColumnName {
                span: first.span,
                qualifier: None,
                name: first,
            })
        }
    }

    // ---- validate -----------------------------------------------------------------------------

    fn validate(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        let target = self.rel_ref()?;
        self.expect(&Tok::LBrace)?;
        let mut checks = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let cstart = self.span();
            let severity = if self.eat_word("require") {
                CheckSeverity::Require
            } else if self.eat_word("expect") {
                CheckSeverity::Expect
            } else if self.eat_word("warn") {
                CheckSeverity::Warn
            } else {
                return Err(self.unexpected("`require`, `expect` or `warn`"));
            };
            let kind = if self.at_word("unique") && self.peek_at(1) == &Tok::LParen {
                self.bump();
                self.bump();
                let mut cols = vec![self.column_name()?];
                while self.eat(&Tok::Comma) {
                    cols.push(self.column_name()?);
                }
                self.expect(&Tok::RParen)?;
                CheckKind::Unique(cols)
            } else {
                let e = self.expr()?;
                if self.at_word("not") && self.word_at(1) == Some("null") {
                    self.bump();
                    self.bump();
                    CheckKind::NotNull(e)
                } else {
                    CheckKind::Predicate(e)
                }
            };
            let label = if self.eat_word("as") {
                Some(self.string()?)
            } else {
                None
            };
            checks.push(Check {
                severity,
                kind,
                label,
                span: cstart.to(self.prev_span()),
            });
        }
        Ok(Statement::Validate(ValidateDecl {
            target,
            checks,
            span: start.to(self.prev_span()),
        }))
    }

    // ---- reconcile ----------------------------------------------------------------------------

    fn reconcile(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::Assign)?;
        let a = self.rel_ref()?;
        let a_alias = if self.eat_word("as") {
            Some(self.ident()?)
        } else {
            None
        };
        self.expect_word("with")?;
        let b = self.rel_ref()?;
        let b_alias = if self.eat_word("as") {
            Some(self.ident()?)
        } else {
            None
        };
        self.expect(&Tok::LBrace)?;
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let istart = self.span();
            let Some(word) = self.word_at(0).map(str::to_string) else {
                return Err(self.unexpected("a reconcile clause (`tier`, `block by`, ...)"));
            };
            let item = match word.as_str() {
                "block" => {
                    self.bump();
                    self.expect_word("by")?;
                    let spec = self.block_spec()?;
                    ReconcileItem::Block(spec, istart.to(self.prev_span()))
                }
                "cardinality" => {
                    self.bump();
                    let v = self.ident()?;
                    ReconcileItem::Cardinality(v, istart.to(self.prev_span()))
                }
                "consume" => {
                    self.bump();
                    let v = self.ident()?;
                    ReconcileItem::Consume(v, istart.to(self.prev_span()))
                }
                "ambiguity" => {
                    self.bump();
                    let v = self.ident()?;
                    ReconcileItem::Ambiguity(v, istart.to(self.prev_span()))
                }
                "duplicates" => {
                    self.bump();
                    let v = self.ident()?;
                    ReconcileItem::Duplicates(v, istart.to(self.prev_span()))
                }
                "identity" => {
                    self.bump();
                    let side = self.ident()?;
                    self.expect(&Tok::Colon)?;
                    let cols = self.ident_or_list()?;
                    ReconcileItem::Identity {
                        side,
                        cols,
                        span: istart.to(self.prev_span()),
                    }
                }
                "evidence" => {
                    self.bump();
                    self.expect(&Tok::LBrace)?;
                    let assigns = self.assign_block()?;
                    ReconcileItem::Evidence(assigns, istart.to(self.prev_span()))
                }
                "flag" => ReconcileItem::Flag(self.flag()?),
                "tier" => ReconcileItem::Tier(self.tier()?),
                _ => {
                    const CLAUSES: &[&str] = &[
                        "block",
                        "cardinality",
                        "consume",
                        "ambiguity",
                        "duplicates",
                        "identity",
                        "evidence",
                        "flag",
                        "tier",
                    ];
                    let mut d = self.unexpected("a reconcile clause (`tier`, `block by`, ...)");
                    if let Some(s) = crate::diagnostic::did_you_mean(&word, CLAUSES.iter().copied())
                    {
                        d = d.help(format!("did you mean `{s}`?"));
                    }
                    return Err(d);
                }
            };
            items.push(item);
        }
        Ok(Statement::Reconcile(ReconcileDecl {
            name,
            a,
            a_alias,
            b,
            b_alias,
            items,
            span: start.to(self.prev_span()),
        }))
    }

    /// Assignments up to and including `}`.
    fn assign_block(&mut self) -> PResult<Vec<Assign>> {
        let mut out = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                return Ok(out);
            }
            out.push(self.assignment()?);
        }
    }

    fn flag(&mut self) -> PResult<Flag> {
        let start = self.expect_word("flag")?;
        let name = self.ident()?;
        let when = if self.eat_word("when") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Flag {
            name,
            when,
            span: start.to(self.prev_span()),
        })
    }

    fn block_spec(&mut self) -> PResult<BlockSpec> {
        if self.at_word("none") {
            let s = self.bump().span;
            return Ok(BlockSpec::None(s));
        }
        let mut keys = Vec::new();
        loop {
            let e = self.expr()?;
            let span = e.span;
            let key = match e.kind {
                ExprKind::Column(c) if c.qualifier.is_none() => BlockKey::Same(c.name),
                ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left,
                    right,
                } => BlockKey::Pair(*left, *right),
                _ => {
                    return Err(Diagnostic::error("M001", "invalid blocking key")
                        .label(span, "expected a column name or `a.x == b.y`"));
                }
            };
            keys.push(key);
            if !self.eat(&Tok::Comma) {
                return Ok(BlockSpec::Keys(keys));
            }
        }
    }

    fn tier(&mut self) -> PResult<TierDecl> {
        let start = self.expect_word("tier")?;
        let name = self.ident()?;
        self.expect(&Tok::LBrace)?;
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let istart = self.span();
            let Some(word) = self.word_at(0).map(str::to_string) else {
                return Err(self.unexpected("a tier clause (`require`, `rank by`, ...)"));
            };
            let item = match word.as_str() {
                "require" => {
                    self.bump();
                    TierItem::Require(self.expr()?)
                }
                "rank" => {
                    self.bump();
                    self.expect_word("by")?;
                    let keys = self.sort_keys()?;
                    TierItem::Rank(keys, istart.to(self.prev_span()))
                }
                "block" => {
                    self.bump();
                    self.expect_word("by")?;
                    let spec = self.block_spec()?;
                    TierItem::Block(spec, istart.to(self.prev_span()))
                }
                "many" | "one" => {
                    let a_many = self.bump().tok
                        == Tok::Ident {
                            name: "many".into(),
                            quoted: false,
                        };
                    let a_side = self.ident()?;
                    self.expect_word("to")?;
                    let b_many = if self.eat_word("many") {
                        true
                    } else {
                        self.expect_word("one")?;
                        false
                    };
                    let b_side = self.ident()?;
                    TierItem::Shape {
                        a_many,
                        b_many,
                        a_side,
                        b_side,
                        span: istart.to(self.prev_span()),
                    }
                }
                "group" => {
                    self.bump();
                    let side = self.ident()?;
                    self.expect_word("by")?;
                    let keys = self.ident_list()?;
                    TierItem::Group {
                        side,
                        keys,
                        span: istart.to(self.prev_span()),
                    }
                }
                "subset" => {
                    self.bump();
                    let side = self.ident()?;
                    self.expect_word("max_items")?;
                    let max_items =
                        self.integer("the largest subset size (an integer from 1 to 16)")?;
                    let max_subsets = if self.eat_word("max_subsets") {
                        Some(self.integer("the number of subsets allowed (a positive integer)")?)
                    } else {
                        None
                    };
                    TierItem::Subset {
                        side,
                        max_items,
                        max_subsets,
                        span: istart.to(self.prev_span()),
                    }
                }
                "compare" => {
                    self.bump();
                    self.expect(&Tok::LBrace)?;
                    let mut exprs = Vec::new();
                    loop {
                        self.skip_separators();
                        if self.eat(&Tok::RBrace) {
                            break;
                        }
                        exprs.push(self.expr()?);
                    }
                    TierItem::Compare(exprs, istart.to(self.prev_span()))
                }
                "evidence" => {
                    self.bump();
                    self.expect(&Tok::LBrace)?;
                    let assigns = self.assign_block()?;
                    TierItem::Evidence(assigns, istart.to(self.prev_span()))
                }
                "flag" => TierItem::Flag(self.flag()?),
                _ => {
                    const CLAUSES: &[&str] = &[
                        "require", "rank", "block", "many", "one", "group", "subset", "compare",
                        "evidence", "flag",
                    ];
                    let mut d = self.unexpected("a tier clause (`require`, `rank by`, ...)");
                    if let Some(s) = crate::diagnostic::did_you_mean(&word, CLAUSES.iter().copied())
                    {
                        d = d.help(format!("did you mean `{s}`?"));
                    }
                    return Err(d);
                }
            };
            items.push(item);
        }
        Ok(TierDecl {
            name,
            items,
            span: start.to(self.prev_span()),
        })
    }

    // ---- export -------------------------------------------------------------------------------

    fn export(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        if self.eat_word("to") {
            let path = self.string()?;
            self.expect(&Tok::LBrace)?;
            let mut parts = Vec::new();
            let mut options = Vec::new();
            loop {
                self.skip_separators();
                if self.eat(&Tok::RBrace) {
                    break;
                }
                if (self.at_word("sheet") || self.at_word("table"))
                    && matches!(self.peek_at(1), Tok::Str(_))
                {
                    let pstart = self.bump().span;
                    let name = self.string()?;
                    self.expect(&Tok::Assign)?;
                    let rel = self.rel_ref()?;
                    parts.push(ExportPart {
                        name,
                        span: pstart.to(rel.span),
                        rel,
                    });
                } else {
                    options.push(self.option()?);
                }
            }
            return Ok(Statement::Export(ExportDecl {
                target: ExportTarget::Multi(parts),
                path,
                options,
                span: start.to(self.prev_span()),
            }));
        }
        let rel = self.rel_ref()?;
        self.expect_word("to")?;
        let path = self.string()?;
        let options = if self.at(&Tok::LBrace) {
            self.option_block()?
        } else {
            Vec::new()
        };
        Ok(Statement::Export(ExportDecl {
            target: ExportTarget::Single(rel),
            path,
            options,
            span: start.to(self.prev_span()),
        }))
    }

    // ---- model --------------------------------------------------------------------------------

    fn model(&mut self, start: Span) -> PResult<Statement> {
        self.bump();
        let name = self.ident()?;
        self.expect(&Tok::LBrace)?;
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            let istart = self.span();
            let Some(word) = self.word_at(0).map(str::to_string) else {
                return Err(self.unexpected("`table`, `relationship`, `dimension` or `metric`"));
            };
            let optional_block = |p: &mut Self| {
                if p.at(&Tok::LBrace) {
                    p.option_block()
                } else {
                    Ok(Vec::new())
                }
            };
            let item = match word.as_str() {
                "table" => {
                    self.bump();
                    let rel = self.ident()?;
                    let options = optional_block(self)?;
                    ModelItem::Table {
                        rel,
                        options,
                        span: istart.to(self.prev_span()),
                    }
                }
                "relationship" => {
                    self.bump();
                    let from = self.column_name()?;
                    self.expect(&Tok::Arrow)?;
                    let to = self.column_name()?;
                    ModelItem::Relationship {
                        from,
                        to,
                        span: istart.to(self.prev_span()),
                    }
                }
                "dimension" => {
                    self.bump();
                    let name = self.ident()?;
                    self.expect(&Tok::Assign)?;
                    let column = self.column_name()?;
                    let options = optional_block(self)?;
                    ModelItem::Dimension {
                        name,
                        column,
                        options,
                        span: istart.to(self.prev_span()),
                    }
                }
                "metric" => {
                    self.bump();
                    let name = self.ident()?;
                    let options = self.option_block()?;
                    ModelItem::Metric {
                        name,
                        options,
                        span: istart.to(self.prev_span()),
                    }
                }
                _ => {
                    const ITEMS: &[&str] = &["table", "relationship", "dimension", "metric"];
                    let mut d = self.unexpected("`table`, `relationship`, `dimension` or `metric`");
                    if let Some(s) = crate::diagnostic::did_you_mean(&word, ITEMS.iter().copied()) {
                        d = d.help(format!("did you mean `{s}`?"));
                    }
                    return Err(d);
                }
            };
            items.push(item);
        }
        Ok(Statement::Model(ModelDecl {
            name,
            items,
            span: start.to(self.prev_span()),
        }))
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr_list(&mut self, close: &Tok) -> PResult<Vec<Expr>> {
        let mut out = Vec::new();
        loop {
            if self.eat(close) {
                return Ok(out);
            }
            out.push(self.expr()?);
            if !self.eat(&Tok::Comma) {
                self.expect(close)?;
                return Ok(out);
            }
        }
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0)
    }

    fn binary_op(&self) -> Option<BinaryOp> {
        Some(match self.peek() {
            Tok::EqEq => BinaryOp::Eq,
            Tok::NotEq => BinaryOp::NotEq,
            Tok::Lt => BinaryOp::Lt,
            Tok::Le => BinaryOp::Le,
            Tok::Gt => BinaryOp::Gt,
            Tok::Ge => BinaryOp::Ge,
            Tok::Plus => BinaryOp::Add,
            Tok::Minus => BinaryOp::Sub,
            Tok::Star => BinaryOp::Mul,
            Tok::Slash => BinaryOp::Div,
            Tok::Percent => BinaryOp::Mod,
            Tok::Ident {
                name,
                quoted: false,
            } if name == "and" => BinaryOp::And,
            Tok::Ident {
                name,
                quoted: false,
            } if name == "or" => BinaryOp::Or,
            _ => return None,
        })
    }

    fn expr_bp(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        loop {
            // postfix predicates bind like comparisons
            const CMP: u8 = 4;
            if CMP >= min_prec {
                if self.at_word("is") {
                    self.bump();
                    let negated = self.eat_word("not");
                    self.expect_word("null")?;
                    lhs = Expr {
                        span: lhs.span.to(self.prev_span()),
                        kind: ExprKind::IsNull {
                            expr: Box::new(lhs),
                            negated,
                        },
                    };
                    continue;
                }
                let negated_in = self.at_word("not") && self.word_at(1) == Some("in");
                if self.at_word("in") || negated_in {
                    if negated_in {
                        self.bump();
                    }
                    self.bump();
                    self.expect(&Tok::LBracket)?;
                    let list = self.expr_list(&Tok::RBracket)?;
                    lhs = Expr {
                        span: lhs.span.to(self.prev_span()),
                        kind: ExprKind::InList {
                            expr: Box::new(lhs),
                            list,
                            negated: negated_in,
                        },
                    };
                    continue;
                }
            }
            let Some(op) = self.binary_op() else { break };
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            let op_span = self.bump().span;
            // comparisons are non-associative: `a < b < c` is an error
            let rhs = self.expr_bp(prec + 1)?;
            if op.is_comparison()
                && let ExprKind::Binary { op: inner, .. } = &lhs.kind
                && inner.is_comparison()
            {
                return Err(Diagnostic::error("M001", "comparisons cannot be chained")
                    .label(op_span, "second comparison")
                    .help("combine them with `and`, e.g. `a < b and b < c`"));
            }
            lhs = Expr {
                span: lhs.span.to(rhs.span),
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                },
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.at_word("not") && self.word_at(1) != Some("null") {
            self.bump();
            let e = self.expr_bp(3)?;
            return Ok(Expr {
                span: start.to(e.span),
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(e),
                },
            });
        }
        if self.eat(&Tok::Minus) {
            let e = self.unary()?;
            // fold negative numeric literals (`- -1.5` folds back to `1.5`, never `--1.5`)
            let kind = match e.kind {
                ExprKind::Literal(Literal::Int(v)) if v.checked_neg().is_some() => {
                    ExprKind::Literal(Literal::Int(-v))
                }
                ExprKind::Literal(Literal::Decimal(d)) => {
                    ExprKind::Literal(Literal::Decimal(match d.strip_prefix('-') {
                        Some(abs) => abs.to_string(),
                        None => format!("-{d}"),
                    }))
                }
                other => ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(Expr {
                        kind: other,
                        span: e.span,
                    }),
                },
            };
            return Ok(Expr {
                span: start.to(e.span),
                kind,
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let lit = |kind| {
            Ok(Expr {
                kind: ExprKind::Literal(kind),
                span: start,
            })
        };
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                lit(Literal::Int(v))
            }
            Tok::Decimal(d) => {
                self.bump();
                lit(Literal::Decimal(d))
            }
            Tok::Str(s) | Tok::BlockStr(s) => {
                self.bump();
                lit(Literal::Str(s))
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(Expr {
                    span: start.to(self.prev_span()),
                    kind: ExprKind::Paren(Box::new(e)),
                })
            }
            Tok::LBracket => {
                self.bump();
                let items = self.expr_list(&Tok::RBracket)?;
                Ok(Expr {
                    span: start.to(self.prev_span()),
                    kind: ExprKind::List(items),
                })
            }
            Tok::Ident { name, quoted } => {
                if !quoted {
                    match name.as_str() {
                        "true" => {
                            self.bump();
                            return lit(Literal::Bool(true));
                        }
                        "false" => {
                            self.bump();
                            return lit(Literal::Bool(false));
                        }
                        "null" => {
                            self.bump();
                            return lit(Literal::Null);
                        }
                        "case" if self.peek_at(1) == &Tok::LBrace => return self.case_expr(),
                        _ => {}
                    }
                }
                let first = self.ident()?;
                if self.at(&Tok::LParen) {
                    self.bump();
                    let args = self.expr_list(&Tok::RParen)?;
                    return Ok(Expr {
                        span: start.to(self.prev_span()),
                        kind: ExprKind::Call {
                            namespace: None,
                            name: first,
                            args,
                        },
                    });
                }
                if self.at(&Tok::Dot) && matches!(self.peek_at(1), Tok::Ident { .. }) {
                    self.bump();
                    let second = self.ident()?;
                    if self.at(&Tok::LParen) {
                        self.bump();
                        let args = self.expr_list(&Tok::RParen)?;
                        return Ok(Expr {
                            span: start.to(self.prev_span()),
                            kind: ExprKind::Call {
                                namespace: Some(first),
                                name: second,
                                args,
                            },
                        });
                    }
                    let span = first.span.to(second.span);
                    return Ok(Expr {
                        span,
                        kind: ExprKind::Column(ColumnName {
                            qualifier: Some(first),
                            name: second,
                            span,
                        }),
                    });
                }
                Ok(Expr {
                    span: first.span,
                    kind: ExprKind::Column(ColumnName {
                        span: first.span,
                        qualifier: None,
                        name: first,
                    }),
                })
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn case_expr(&mut self) -> PResult<Expr> {
        let start = self.bump().span;
        self.expect(&Tok::LBrace)?;
        let mut arms = Vec::new();
        let mut otherwise = None;
        loop {
            self.skip_separators();
            if self.eat(&Tok::RBrace) {
                break;
            }
            if self.at_word("otherwise") && self.peek_at(1) == &Tok::FatArrow {
                let ostart = self.bump().span;
                self.bump();
                if otherwise.is_some() {
                    return Err(Diagnostic::error(
                        "M001",
                        "`case` has more than one `otherwise` arm",
                    )
                    .label(ostart, "second `otherwise`"));
                }
                otherwise = Some(Box::new(self.expr()?));
                continue;
            }
            let when = self.expr()?;
            self.expect(&Tok::FatArrow)?;
            let then = self.expr()?;
            arms.push(CaseArm { when, then });
        }
        if arms.is_empty() {
            return Err(Diagnostic::error(
                "M001",
                "`case` needs at least one `condition => value` arm",
            )
            .label(start.to(self.prev_span()), "empty case"));
        }
        Ok(Expr {
            span: start.to(self.prev_span()),
            kind: ExprKind::Case { arms, otherwise },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Program {
        let p = parse(0, src);
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        p.program
    }

    #[test]
    fn contextual_keywords_are_column_names() {
        let prog =
            parse_ok("dataset x = a |> filter type == \"Foo\" and group > 1 |> sort type desc");
        let Statement::Dataset(d) = &prog.statements[0] else {
            panic!()
        };
        assert_eq!(d.pipeline.steps.len(), 2);
    }

    #[test]
    fn precedence_and_postfix() {
        let e = parse_expr(0, "not a + 1 * 2 >= b and c is not null or d in [1, 2]").unwrap();
        // top-level is `or`
        let ExprKind::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } = e.kind
        else {
            panic!()
        };
        assert!(matches!(
            right.kind,
            ExprKind::InList { negated: false, .. }
        ));
        let ExprKind::Binary {
            op: BinaryOp::And,
            left: not_part,
            right: isnull,
        } = left.kind
        else {
            panic!()
        };
        assert!(matches!(
            isnull.kind,
            ExprKind::IsNull { negated: true, .. }
        ));
        assert!(matches!(
            not_part.kind,
            ExprKind::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
    }

    #[test]
    fn chained_comparison_is_rejected() {
        let err = parse_expr(0, "a < b < c").unwrap_err();
        assert!(err.message.contains("chained"));
    }

    #[test]
    fn double_negation_folds_to_positive_literal() {
        let lit = |src| match parse_expr(0, src).unwrap().kind {
            ExprKind::Literal(l) => l,
            other => panic!("{other:?}"),
        };
        assert_eq!(lit("- -1.5"), Literal::Decimal("1.5".into()));
        assert_eq!(lit("-1.5"), Literal::Decimal("-1.5".into()));
        assert_eq!(lit("- -2"), Literal::Int(2));
    }

    /// The AST of a small program covering sources, pipelines, validation and a reconciliation.
    #[test]
    fn ast_snapshot() {
        let prog = parse_ok(
            r#"source a = csv("a.csv") { identity id  schema { id: string  amount: decimal(18, 2)? } }
dataset clean = a |> derive k = lower(trim(kind)) |> filter amount > 0 and k in ["x", "y"]
validate clean { require id not null  expect unique(id) as "one row per id" }
reconcile r = clean with clean as b {
    block by id
    tier exact { require a.amount == b.amount  rank by similarity(a.k, b.k) desc }
    tier roll { many a to one b  group a by id  compare { sum(a.amount) == b.amount } }
}
export r.matches to "m.csv""#,
        );
        insta::assert_debug_snapshot!(prog);
    }

    #[test]
    fn recovers_and_reports_multiple_errors() {
        let p = parse(
            0,
            "dataset x = a |> fliter y\ndataset z = b |> derive\nsource s = csv(\"x.csv\")",
        );
        assert_eq!(p.diagnostics.len(), 2, "{:?}", p.diagnostics);
        assert_eq!(p.program.statements.len(), 1);
        assert_eq!(
            p.diagnostics[0].help.as_deref(),
            Some("did you mean `filter`?")
        );
    }

    fn declared(p: &Parsed) -> Vec<String> {
        p.program
            .statements
            .iter()
            .filter_map(|s| match s {
                Statement::Source(d) => Some(d.name.name.clone()),
                Statement::Dataset(d) => Some(d.name.name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lex_error_drops_only_its_statement() {
        let p = parse(
            0,
            "source a = csv(\"a.csv\")\n\
             dataset d = a |> filter x > 1 && x < 5\n\
             dataset e = a |> fliter x\n\
             dataset f = a",
        );
        let messages: Vec<&str> = p.diagnostics.iter().map(|d| d.message.as_str()).collect();
        // `&&` is one error, the later parse error is still reported, in source order.
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0], "unexpected character `&`");
        assert_eq!(
            p.diagnostics[1].help.as_deref(),
            Some("did you mean `filter`?")
        );
        assert_eq!(declared(&p), ["a", "f"]);
        assert_eq!(p.failed_names, ["d", "e"]);
    }

    #[test]
    fn unterminated_string_recovers_at_next_line() {
        let p = parse(
            0,
            "dataset d = a |> filter x == \"open\ndataset e = a |> filter y == \"closed\"",
        );
        assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
        assert_eq!(p.diagnostics[0].message, "unterminated string literal");
        assert_eq!(declared(&p), ["e"]);
        assert_eq!(p.failed_names, ["d"]);
    }

    #[test]
    fn statement_complete_before_bad_line_stands() {
        let p = parse(0, "source a = csv(\"a.csv\")\n@ junk\ndataset b = a");
        assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
        assert_eq!(declared(&p), ["a", "b"]);
        assert!(p.failed_names.is_empty());
    }

    #[test]
    fn unterminated_block_string_swallows_rest_of_file() {
        let p = parse(0, "source q = sql(db, \"\"\"select 1\ndataset e = a");
        assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
        assert_eq!(p.diagnostics[0].message, "unterminated block string");
        assert!(p.program.statements.is_empty());
        assert_eq!(p.failed_names, ["q"]);
    }
}
