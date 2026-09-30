//! `magi lsp`: the language server driven over stdio the way an editor drives it (JSON-RPC with
//! `Content-Length` framing).
//!
//! The fixture program is split like the examples: `main.magi` imports `lib.magi` (the sources)
//! and `report.magi` (an export of `rec`, which only `main.magi` declares).

#[macro_use]
#[allow(dead_code)]
mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use lsp_types::Url;
use serde_json::{Value, json};
use support::Fixture;

/// A generous bound: the first analysis in a debug build starts DuckDB.
const TIMEOUT: Duration = Duration::from_secs(120);

struct Client {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    /// Notifications received while waiting for something else, oldest first.
    pending: Vec<Value>,
    next_id: i64,
}

impl Client {
    /// Start `magi lsp` in the fixture directory and complete the `initialize` handshake;
    /// returns the client and the server's capabilities.
    fn start(fx: &Fixture) -> (Client, Value) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_magi"))
            .arg("lsp")
            .current_dir(fx.file(""))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start magi lsp");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length:") {
                        length = Some(v.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.expect("Content-Length header")];
                stdout.read_exact(&mut body).unwrap();
                if tx.send(serde_json::from_slice(&body).unwrap()).is_err() {
                    return;
                }
            }
        });
        let mut client = Client {
            child,
            stdin,
            messages: rx,
            pending: Vec::new(),
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        );
        client.notify("initialized", json!({}));
        (client, init["capabilities"].clone())
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&mut self) -> Value {
        self.messages
            .recv_timeout(TIMEOUT)
            .expect("the server answers in time")
    }

    /// Send a request and return its whole response (`result` or `error`).
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let msg = self.next();
            if msg["id"] == json!(id) {
                return msg;
            }
            self.pending.push(msg);
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn open(&mut self, uri: &Url, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "magi", "version": 1, "text": text } }),
        );
    }

    fn change(&mut self, uri: &Url, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// The next diagnostics published for `uri`.
    fn diagnostics(&mut self, uri: &Url) -> Vec<Value> {
        let is_for = |m: &Value| {
            m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri.as_str()
        };
        let msg = match self.pending.iter().position(is_for) {
            Some(i) => self.pending.remove(i),
            None => loop {
                let msg = self.next();
                if is_for(&msg) {
                    break msg;
                }
                self.pending.push(msg);
            },
        };
        msg["params"]["diagnostics"].as_array().unwrap().clone()
    }

    /// `publishDiagnostics` notifications not consumed by [`Client::diagnostics`] yet. Call it
    /// after a request: the server answers in order, so everything sent before has arrived.
    fn unread_diagnostics(&self) -> Vec<&Value> {
        self.pending
            .iter()
            .filter(|m| m["method"] == "textDocument/publishDiagnostics")
            .collect()
    }

    fn at(&mut self, method: &str, uri: &Url, line: u32, character: u32) -> Value {
        self.request(
            method,
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
    }

    /// `shutdown`, then `exit`; the process's exit code.
    fn stop(mut self) -> i32 {
        self.request("shutdown", Value::Null);
        self.exit()
    }

    fn exit(mut self) -> i32 {
        self.notify("exit", Value::Null);
        drop(self.stdin);
        self.child.wait().unwrap().code().unwrap()
    }
}

fn uri(fx: &Fixture, name: &str) -> Url {
    Url::from_file_path(fx.file(name)).unwrap()
}

fn range(r: &Value) -> [u64; 4] {
    [
        &r["start"]["line"],
        &r["start"]["character"],
        &r["end"]["line"],
        &r["end"]["character"],
    ]
    .map(|v| v.as_u64().unwrap())
}

/// The location a definition response points at: file name and range.
fn location(v: &Value) -> (String, [u64; 4]) {
    let uri = Url::parse(v["uri"].as_str().unwrap()).unwrap();
    let path = uri.to_file_path().unwrap();
    (
        path.file_name().unwrap().to_string_lossy().into_owned(),
        range(&v["range"]),
    )
}

/// The server advertises exactly the features it implements, and ends with code 0 after
/// `shutdown` and `exit`.
#[test]
fn initialize_advertises_the_implemented_features() {
    let fx = Fixture::new("lsp");
    let (client, caps) = Client::start(&fx);
    let mut keys: Vec<&str> = caps
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "completionProvider",
            "definitionProvider",
            "documentFormattingProvider",
            "hoverProvider",
            "semanticTokensProvider",
            "textDocumentSync",
        ]
    );
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["textDocumentSync"]["change"], 1, "full document sync");
    assert_eq!(caps["semanticTokensProvider"]["full"], true);
    let types = &caps["semanticTokensProvider"]["legend"]["tokenTypes"];
    for t in ["keyword", "comment", "string", "number"] {
        assert!(types.as_array().unwrap().contains(&json!(t)), "{types}");
    }
    assert_eq!(client.stop(), 0);
}

/// `exit` without `shutdown` first ends the server with code 1, as the protocol specifies.
#[test]
fn exit_without_shutdown_is_an_error() {
    let fx = Fixture::new("lsp");
    let (client, _) = Client::start(&fx);
    assert_eq!(client.exit(), 1);
}

/// A type error in the editor's buffer (not on disk) is published with its code and range; the
/// line has `ü` before the error, which is one UTF-16 unit but two bytes. Fixing the buffer
/// clears the diagnostic.
#[test]
fn type_error_is_published_with_utf16_range_and_cleared_when_fixed() {
    let fx = Fixture::new("lsp");
    let main = uri(&fx, "main.magi");
    let text = fx.read("main.magi");
    let bad = text.replace(
        r#"filter region == "Zürich""#,
        r#"filter region == "Zürich" and amount == "x""#,
    );
    assert_ne!(bad, text);
    let (mut client, _) = Client::start(&fx);
    client.open(&main, &bad);
    let diags = client.diagnostics(&main);
    assert_eq!(diags.len(), 1, "{diags:?}");
    let d = &diags[0];
    assert_eq!(d["code"], "M105");
    assert_eq!(d["severity"], 1);
    assert_eq!(d["source"], "magi");
    // `amount` on line 6 starts at byte 57 but UTF-16 column 56
    assert_eq!(range(&d["range"]), [5, 56, 5, 62]);
    assert!(
        d["message"].as_str().unwrap().contains("help: "),
        "help is part of the message: {d}"
    );
    let related = &d["relatedInformation"][0];
    assert_eq!(range(&related["location"]["range"]), [5, 66, 5, 69]);
    assert_eq!(related["location"]["uri"], main.as_str());

    client.change(&main, 2, &text);
    assert_eq!(client.diagnostics(&main), Vec::<Value>::new());
    assert_eq!(client.stop(), 0);
}

/// An imported file opened on its own is analysed as part of the program that imports it:
/// `report.magi` uses `rec` from `main.magi` without an error. Its own errors are published to
/// it and cleared when fixed, and its names lead to their declarations in the importing file.
#[test]
fn imported_file_is_analysed_within_its_program() {
    let fx = Fixture::new("lsp");
    let report = uri(&fx, "report.magi");
    let text = fx.read("report.magi");
    let (mut client, _) = Client::start(&fx);
    client.open(&report, &text);
    let def = client.at("textDocument/definition", &report, 2, 8);
    assert_eq!(location(&def), ("main.magi".into(), [7, 10, 7, 13]));

    client.change(&report, 2, &text.replace("unmatched_a", "unmatched_x"));
    let diags = client.diagnostics(&report);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "M002");
    assert_eq!(diags[0]["range"]["start"]["line"], 2);

    client.change(&report, 3, &text);
    assert_eq!(client.diagnostics(&report), Vec::<Value>::new());
    // nothing else was published: the valid file and its program never had errors
    client.request("shutdown", Value::Null);
    assert_eq!(client.unread_diagnostics(), Vec::<&Value>::new());
    assert_eq!(client.exit(), 0);
}

/// Formatting returns the whole document in canonical style as one edit, and no edit when the
/// document is already formatted or does not parse.
#[test]
fn formatting_returns_the_canonical_text() {
    let fx = Fixture::new("lsp");
    let main = uri(&fx, "main.magi");
    let text = fx.read("main.magi");
    let messy = text.replace(
        "dataset zurich = sales |> filter",
        "dataset   zurich=sales\n|>filter",
    );
    std::fs::write(fx.file("messy.magi"), &messy).unwrap();
    let canonical = fx.magi(&["fmt", "--stdout", "messy.magi"]);
    canonical.assert_code(0);
    let format = |client: &mut Client| {
        client.request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": main },
                "options": { "tabSize": 4, "insertSpaces": true },
            }),
        )
    };
    let (mut client, _) = Client::start(&fx);
    client.open(&main, &messy);
    let edits = format(&mut client);
    let edits = edits.as_array().unwrap();
    assert_eq!(edits.len(), 1, "{edits:?}");
    let lines = messy.matches('\n').count() as u64;
    assert_eq!(range(&edits[0]["range"]), [0, 0, lines, 0]);
    assert_eq!(edits[0]["newText"], canonical.stdout.as_str());

    client.change(&main, 2, &text);
    assert_eq!(format(&mut client), json!([]));
    client.change(&main, 3, "dataset = \n");
    assert_eq!(format(&mut client), json!([]));
    assert_eq!(client.stop(), 0);
}

/// Definition of a relation name used in another statement: a dataset in the same file, a
/// source in an imported file, and a reconcile output (`rec.matches` leads to `rec`). A column
/// name is not a declaration.
#[test]
fn definition_finds_declarations_across_imports() {
    let fx = Fixture::new("lsp");
    let main = uri(&fx, "main.magi");
    let text = fx.read("main.magi");
    let (mut client, _) = Client::start(&fx);
    client.open(&main, &text);
    // `zurich` in `reconcile rec = zurich as s ...`
    let def = client.at("textDocument/definition", &main, 7, 18);
    assert_eq!(location(&def), ("main.magi".into(), [5, 8, 5, 14]));
    // `sales` in `dataset zurich = sales`, declared in lib.magi
    let def = client.at("textDocument/definition", &main, 5, 19);
    assert_eq!(location(&def), ("lib.magi".into(), [2, 7, 2, 12]));
    // `matches` in `export rec.matches`
    let def = client.at("textDocument/definition", &main, 15, 14);
    assert_eq!(location(&def), ("main.magi".into(), [7, 10, 7, 13]));
    // the column `region` in the filter
    let def = client.at("textDocument/definition", &main, 5, 35);
    assert_eq!(def, Value::Null);
    assert_eq!(client.stop(), 0);
}

/// Hover shows a relation's columns with their types, and a reconcile's outputs; completion
/// after `rec.` offers those outputs.
#[test]
fn hover_lists_columns_and_outputs() {
    let fx = Fixture::new("lsp");
    let main = uri(&fx, "main.magi");
    let text = fx.read("main.magi");
    let (mut client, _) = Client::start(&fx);
    client.open(&main, &text);
    let hover = client.at("textDocument/hover", &main, 7, 18);
    let shown = hover["contents"]["value"].as_str().unwrap();
    assert!(shown.starts_with("dataset `zurich`"), "{shown}");
    let columns: Vec<Vec<&str>> = shown
        .lines()
        .skip_while(|l| !l.starts_with("```"))
        .skip(1)
        .take_while(|l| !l.starts_with("```"))
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        columns,
        [
            ["id", "int"],
            ["region", "string"],
            ["amount", "decimal(10,2)"]
        ]
    );
    assert_eq!(range(&hover["range"]), [7, 16, 7, 22]);

    let hover = client.at("textDocument/hover", &main, 15, 8);
    let shown = hover["contents"]["value"].as_str().unwrap();
    assert!(shown.starts_with("reconcile `rec`"), "{shown}");
    assert!(shown.contains("`rec.matches`") && shown.contains("`rec.unmatched_a`"));

    let items = client.at("textDocument/completion", &main, 15, 11);
    let labels: Vec<&str> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"matches") && labels.contains(&"unmatched_b"),
        "{labels:?}"
    );
    assert_eq!(client.stop(), 0);
}
