//! `magi lsp`: a language server over stdio for editors.
//!
//! Open documents are analysed like `magi check` (SQL sources are not contacted) on every open,
//! change and save; imported files are read from the editor's buffers when open, else from disk.
//! A document is analysed as part of its program: the file whose imports reach it (see
//! [`analysis::entry`]). Diagnostics go to the file each one points at, imported files
//! included, and are cleared once they no longer occur.
//!
//! Everything runs on one thread; a panic while handling a message is caught and logged to
//! stderr, and the server keeps serving.

mod analysis;
mod convert;
mod highlight;
mod navigate;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, Formatting, GotoDefinition, HoverRequest, Request as _, SemanticTokensFullRequest,
};
use lsp_types::{
    CompletionOptions, CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, Location, MarkupContent, MarkupKind, OneOf, PublishDiagnosticsParams,
    SaveOptions, SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TextEdit, Url,
};
use serde_json::{Value, from_value, json};

use analysis::Analysis;
use convert::{LineIndex, key, path_of, uri_of};

/// Run the server on stdin/stdout until the client says `exit`: exit code 0 after a `shutdown`
/// request, 1 otherwise (as the protocol specifies).
pub fn main() -> ExitCode {
    let (conn, io) = Connection::stdio();
    let code = match serve(conn) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("magi lsp: {e}");
            ExitCode::from(1)
        }
    };
    if let Err(e) = io.join() {
        eprintln!("magi lsp: {e}");
    }
    code
}

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        document_formatting_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: highlight::LEGEND.to_vec(),
                    token_modifiers: Vec::new(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: None,
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

fn serve(conn: Connection) -> Result<ExitCode, String> {
    let (id, _params) = conn.initialize_start().map_err(|e| e.to_string())?;
    let result = json!({
        "capabilities": capabilities(),
        "serverInfo": { "name": "magi", "version": env!("CARGO_PKG_VERSION") },
    });
    conn.initialize_finish(id, result)
        .map_err(|e| e.to_string())?;
    let mut server = Server {
        conn,
        docs: HashMap::new(),
        analyses: HashMap::new(),
        published: BTreeMap::new(),
    };
    let mut shutdown = false;
    while let Ok(msg) = server.conn.receiver.recv() {
        match msg {
            Message::Request(req) if req.method == "shutdown" => {
                shutdown = true;
                server.send(Response::new_ok(req.id, Value::Null));
            }
            Message::Request(req) if shutdown => server.send(Response::new_err(
                req.id,
                ErrorCode::InvalidRequest as i32,
                "the server is shutting down".into(),
            )),
            Message::Request(req) => {
                let id = req.id.clone();
                let response =
                    guarded(&req.method.clone(), || server.request(req)).unwrap_or_else(|| {
                        Response::new_err(
                            id,
                            ErrorCode::InternalError as i32,
                            "internal error (logged on stderr)".into(),
                        )
                    });
                server.send(response);
            }
            Message::Notification(n) if n.method == "exit" => {
                return Ok(ExitCode::from(if shutdown { 0 } else { 1 }));
            }
            Message::Notification(n) if !shutdown => {
                guarded(&n.method.clone(), || server.notification(n));
            }
            Message::Notification(_) | Message::Response(_) => {}
        }
    }
    // the client went away without `exit`
    Ok(ExitCode::from(1))
}

/// Run `f`, logging a panic to stderr instead of ending the server.
fn guarded<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            eprintln!("magi lsp: internal error handling {what}: {message}");
            None
        }
    }
}

struct Document {
    /// The URI the client names the document by (reused for its diagnostics and locations).
    uri: Url,
    text: String,
    /// The entry file of the program the document is analysed in ([`analysis::entry`]).
    entry: PathBuf,
}

/// A file's text: the editor's buffer when the file is open, else the disk.
fn read_file(docs: &HashMap<PathBuf, Document>, path: &Path) -> std::io::Result<String> {
    match docs.get(&key(path)) {
        Some(d) => Ok(d.text.clone()),
        None => std::fs::read_to_string(path),
    }
}

struct Server {
    conn: Connection,
    /// Open documents by file identity ([`key`]).
    docs: HashMap<PathBuf, Document>,
    /// The latest analysis of each open document's program, by entry file.
    analyses: HashMap<PathBuf, Analysis>,
    /// Diagnostics last sent, by document; a document without diagnostics has no entry.
    published: BTreeMap<Url, Vec<lsp_types::Diagnostic>>,
}

impl Server {
    fn send(&self, msg: impl Into<Message>) {
        if self.conn.sender.send(msg.into()).is_err() {
            eprintln!("magi lsp: the client connection is closed");
        }
    }

    fn notification(&mut self, n: Notification) {
        let file = match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(p) = from_value::<DidOpenTextDocumentParams>(n.params) else {
                    return;
                };
                let Some(path) = path_of(&p.text_document.uri) else {
                    return;
                };
                let file = key(&path);
                self.docs.insert(
                    file.clone(),
                    Document {
                        uri: p.text_document.uri,
                        text: p.text_document.text,
                        entry: file.clone(),
                    },
                );
                file
            }
            DidChangeTextDocument::METHOD => {
                let Ok(p) = from_value::<DidChangeTextDocumentParams>(n.params) else {
                    return;
                };
                let Some(file) = path_of(&p.text_document.uri).map(|p| key(&p)) else {
                    return;
                };
                // full sync: the last change holds the whole text
                let (Some(doc), Some(change)) = (
                    self.docs.get_mut(&file),
                    p.content_changes.into_iter().last(),
                ) else {
                    return;
                };
                doc.text = change.text;
                file
            }
            DidSaveTextDocument::METHOD => {
                let Ok(p) = from_value::<DidSaveTextDocumentParams>(n.params) else {
                    return;
                };
                let Some(file) = path_of(&p.text_document.uri).map(|p| key(&p)) else {
                    return;
                };
                file
            }
            DidCloseTextDocument::METHOD => {
                let Ok(p) = from_value::<DidCloseTextDocumentParams>(n.params) else {
                    return;
                };
                let Some(file) = path_of(&p.text_document.uri).map(|p| key(&p)) else {
                    return;
                };
                self.docs.remove(&file);
                file
            }
            _ => return,
        };
        self.refresh(&file);
        self.publish();
    }

    /// Re-analyse after `file` changed: every open document's program that reads `file`, and
    /// programs not analysed yet; analyses no open document needs are dropped.
    fn refresh(&mut self, file: &Path) {
        let read = |p: &Path| read_file(&self.docs, p);
        if self.docs.contains_key(file) {
            let entry = guarded("finding the program's entry file", || {
                analysis::entry(file, &read)
            })
            .unwrap_or_else(|| file.to_path_buf());
            if let Some(doc) = self.docs.get_mut(file) {
                doc.entry = entry;
            }
        }
        let docs = &self.docs;
        let read = |p: &Path| read_file(docs, p);
        let entries: HashSet<&PathBuf> = docs.values().map(|d| &d.entry).collect();
        self.analyses.retain(|entry, _| entries.contains(entry));
        for entry in entries {
            let stale = self
                .analyses
                .get(entry)
                .is_none_or(|a| a.file_id(file).is_some());
            if !stale {
                continue;
            }
            match guarded("analysing the program", || analysis::analyse(entry, &read)) {
                Some(Ok(a)) => {
                    self.analyses.insert(entry.clone(), a);
                }
                Some(Err(e)) => {
                    eprintln!("magi lsp: {e}");
                    self.analyses.remove(entry);
                }
                // keep the previous analysis
                None => {}
            }
        }
    }

    /// Send the diagnostics that changed since the last time, and clear those of documents
    /// that no longer have any.
    fn publish(&mut self) {
        let mut current: BTreeMap<Url, Vec<lsp_types::Diagnostic>> = BTreeMap::new();
        for a in self.analyses.values() {
            for (uri, diags) in analysis::diagnostics(a, &|file| self.uri(a, file)) {
                let list = current.entry(uri).or_default();
                for d in diags {
                    if !list.contains(&d) {
                        list.push(d);
                    }
                }
            }
        }
        current.retain(|_, diags| !diags.is_empty());
        let uris: Vec<Url> = current
            .keys()
            .chain(self.published.keys())
            .cloned()
            .collect();
        for uri in uris {
            let now = current.get(&uri);
            if now == self.published.get(&uri) {
                continue;
            }
            let params = PublishDiagnosticsParams::new(uri, now.cloned().unwrap_or_default(), None);
            self.send(Notification::new(PublishDiagnostics::METHOD.into(), params));
        }
        self.published = current;
    }

    fn request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            Formatting::METHOD => from_value(req.params).map(|p| self.format(p)),
            GotoDefinition::METHOD => from_value(req.params).map(|p| self.definition(p)),
            HoverRequest::METHOD => from_value(req.params).map(|p| self.hover(p)),
            Completion::METHOD => from_value(req.params).map(|p| self.completion(p)),
            SemanticTokensFullRequest::METHOD => {
                from_value(req.params).map(|p| self.semantic_tokens(p))
            }
            _ => {
                return Response::new_err(
                    id,
                    ErrorCode::MethodNotFound as i32,
                    format!("`{}` is not supported", req.method),
                );
            }
        };
        match result {
            Ok(value) => Response::new_ok(id, value),
            Err(e) => Response::new_err(id, ErrorCode::InvalidParams as i32, e.to_string()),
        }
    }

    /// The URI of a file of `a`'s program: the client's own for an open document.
    fn uri(&self, a: &Analysis, file: u32) -> Option<Url> {
        let file = &a.keys[file as usize];
        match self.docs.get(file) {
            Some(doc) => Some(doc.uri.clone()),
            None => uri_of(file),
        }
    }

    fn doc(&self, uri: &Url) -> Option<(&PathBuf, &Document)> {
        let file = key(&path_of(uri)?);
        self.docs.get_key_value(&file)
    }

    /// The open document at `at`, its program's analysis and the byte offset of the position.
    fn locate(&self, at: &TextDocumentPositionParams) -> Option<(&Document, &Analysis, usize)> {
        let (file, doc) = self.doc(&at.text_document.uri)?;
        let a = self.analyses.get(&doc.entry)?;
        a.file_id(file)?;
        let offset = LineIndex::new(&doc.text).offset(at.position);
        Some((doc, a, offset))
    }

    fn format(&self, p: DocumentFormattingParams) -> Value {
        let Some((_, doc)) = self.doc(&p.text_document.uri) else {
            return Value::Null;
        };
        let edits = match crate::fmt::format_source(0, &doc.text) {
            Ok(formatted) if formatted != doc.text => vec![TextEdit::new(
                LineIndex::new(&doc.text).range(0, doc.text.len()),
                formatted,
            )],
            // unchanged, or does not parse (the diagnostics say why)
            _ => Vec::new(),
        };
        json!(edits)
    }

    fn definition(&self, p: GotoDefinitionParams) -> Value {
        let Some((doc, a, offset)) = self.locate(&p.text_document_position_params) else {
            return Value::Null;
        };
        let Some((target, _)) = navigate::target_at(a, &doc.text, offset) else {
            return Value::Null;
        };
        let span = target.decl().name.span;
        let Some(uri) = self.uri(a, span.file) else {
            return Value::Null;
        };
        let text = &a.loaded.sources.file(span.file).text;
        let range = LineIndex::new(text).range(span.start as usize, span.end as usize);
        json!(GotoDefinitionResponse::Scalar(Location::new(uri, range)))
    }

    fn hover(&self, p: HoverParams) -> Value {
        let Some((doc, a, offset)) = self.locate(&p.text_document_position_params) else {
            return Value::Null;
        };
        let Some((target, span)) = navigate::target_at(a, &doc.text, offset) else {
            return Value::Null;
        };
        json!(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: navigate::hover(a, &target),
            }),
            range: Some(LineIndex::new(&doc.text).range(span.start as usize, span.end as usize)),
        })
    }

    fn completion(&self, p: lsp_types::CompletionParams) -> Value {
        let at = &p.text_document_position;
        let Some((_, doc)) = self.doc(&at.text_document.uri) else {
            return Value::Null;
        };
        let offset = LineIndex::new(&doc.text).offset(at.position);
        let a = self.locate(at).map(|(_, a, _)| a);
        json!(CompletionResponse::Array(navigate::completions(
            a, &doc.text, offset,
        )))
    }

    fn semantic_tokens(&self, p: SemanticTokensParams) -> Value {
        let Some((_, doc)) = self.doc(&p.text_document.uri) else {
            return Value::Null;
        };
        json!(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: highlight::tokens(&doc.text),
        }))
    }
}
