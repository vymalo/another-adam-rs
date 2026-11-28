//! A scripted language server, for the tests of `file_diagnostics`, `find_definition` and
//! `find_references` (ADR 0044 slice 17). **Not shipped**: the coder image builds the named binaries
//! `adam-coder`, `adam-agent` and the helpers, and none of them is this one.
//!
//! It speaks the protocol on stdin and stdout (`Content-Length` frames) and does what the JSON script
//! says, named by the variable `ADAM_LSP_SCRIPT` (a path). Every key is optional:
//!
//! | Key | Does |
//! |---|---|
//! | `log` | a file every message it receives is appended to, one JSON line each: `{"method", "params"}` |
//! | `log_argv` | its arguments are the first line of `log`: `{"argv": [..]}` (what the launch line gave it) |
//! | `stderr` | text written to stderr at start |
//! | `pid_file` | its pid is written there (and the child's pid beside it with `.child`, and the grandchild's with `.grandchild`, for `spawn_tree`) |
//! | `spawn_tree` | starts a child that starts a grandchild, in its process group |
//! | `initialize_delay_ms`, `initialize_never` | answers `initialize` late, or never |
//! | `position_encoding` | `utf-8`: says so when the client offers it |
//! | `pull` | offers `diagnosticProvider` and answers `textDocument/diagnostic` |
//! | `progress_ms` | after `initialized`, a `$/progress` that begins and ends after that long |
//! | `progress_delay_ms` | the begin is sent that long after `initialized` (a client that has not read it yet), the end `progress_ms` after the begin |
//! | `status_ms` | says it reports `experimental/serverStatus`: not quiescent after `initialized`, quiescent after that long |
//! | `status_delay_ms` | the "not quiescent" is sent that long after `initialized`, the quiescent `status_ms` after it |
//! | `diagnostics` | `[{severity, message, line, character}]` for every opened file |
//! | `echo` | each diagnostic message ends with ` \| held: ` and the text it holds for the file |
//! | `silent_when_clean` | publishes nothing for a file with no diagnostics |
//! | `results` | the result of a method, by name; `$FILE` and `$ROOT` in a string are the document's and the root's URI |
//! | `references_count`, `references_uri` | `textDocument/references` answers this many locations, in the document or in `references_uri` |
//! | `never_answer` | methods it does not answer |
//! | `crash_on_request` | the n-th request (not `initialize`) ends the process with status 3, after `crash_stderr` |
//! | `bad_framing_on_request`, `oversize_on_request` | the n-th request is answered with garbage, or with a `Content-Length` of 99,999,999 |
//! | `server_requests` | requests it makes after `initialized`: `[{method, params}]`; their answers are logged as `{"answer_to", "result"}` |
//! | `write_on_request` | `{"n", "path", "content"}`: writes a file (relative to the root) before it answers the n-th request |
//! | `answer_delay_ms` | every answer to a request but `initialize` waits this long |
//! | `uri_spelling` | `lower_hex`: the `publishDiagnostics` of a file name it with every byte but letters, digits and `/` percent-encoded in lower case (a spelling of the same path) |
//! | `garbage_after_ms` | this long after `initialized`, bytes that are not a message are written on stdout |
//! | `error_on` | `{method: code}`: that request is answered with a JSON-RPC error of that code |
//! | `stall_after_initialized_ms` | after `initialized` the script reads nothing for this long (a server that does not read its input) |

#![allow(clippy::unwrap_used, clippy::expect_used)] // a test double

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

type Out = Arc<Mutex<std::io::Stdout>>;

fn send(out: &Out, body: &Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let mut out = out.lock().unwrap();
    write!(out, "Content-Length: {}\r\n\r\n", bytes.len()).unwrap();
    out.write_all(&bytes).unwrap();
    out.flush().unwrap();
}

fn raw(out: &Out, bytes: &[u8]) {
    let mut out = out.lock().unwrap();
    out.write_all(bytes).unwrap();
    out.flush().unwrap();
}

fn substitute(value: &mut Value, file: &str, root: &str) {
    match value {
        Value::String(s) => {
            *s = s.replace("$FILE", file).replace("$ROOT", root);
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, file, root)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute(v, file, root)),
        _ => {}
    }
}

/// `uri` with every byte of its path but letters, digits and `/` written `%xx` in lower case: the
/// same file, spelled as some servers spell it.
fn lower_hex(uri: &str) -> String {
    let Some(path) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let mut out = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b == b'/' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

fn path_of(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_owned()
}

struct Script {
    json: Value,
}

impl Script {
    fn bool(&self, key: &str) -> bool {
        self.json.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    fn u64(&self, key: &str) -> Option<u64> {
        self.json.get(key).and_then(Value::as_u64)
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.json.get(key).and_then(Value::as_str)
    }
}

fn log(script: &Script, line: &Value) {
    if let Some(path) = script.str("log") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        // One write, so that a reader polling the file never sees half a line.
        file.write_all(format!("{line}\n").as_bytes()).unwrap();
    }
}

fn diagnostics_for(script: &Script, text: &str) -> Vec<Value> {
    let Some(items) = script.json.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|d| {
            let mut message = d["message"].as_str().unwrap_or("a problem").to_owned();
            if script.bool("echo") {
                message.push_str(" | held: ");
                message.push_str(text);
            }
            let line = d["line"].as_u64().unwrap_or(0);
            let character = d["character"].as_u64().unwrap_or(0);
            let mut out = json!({
                "range": {
                    "start": {"line": line, "character": character},
                    "end": {"line": line, "character": character + 1},
                },
                "message": message,
            });
            if let Some(severity) = d.get("severity") {
                out["severity"] = severity.clone();
            }
            if let Some(code) = d.get("code") {
                out["code"] = code.clone();
            }
            if let Some(source) = d.get("source") {
                out["source"] = source.clone();
            }
            out
        })
        .collect()
}

fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut length = 0_usize;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0_u8; length];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn main() {
    let script = Script {
        json: std::env::var("ADAM_LSP_SCRIPT")
            .ok()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({})),
    };
    if script.bool("log_argv") {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        log(&script, &json!({ "argv": argv }));
    }
    if let Some(text) = script.str("stderr") {
        eprintln!("{text}");
    }
    if let Some(path) = script.str("pid_file") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    if script.bool("spawn_tree") {
        // A child that starts a grandchild: a stop that kills the leader only leaves two behind.
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 631 & echo $! > \"$0\"; wait")
            .arg(format!(
                "{}.grandchild",
                script.str("pid_file").unwrap_or("/dev/null")
            ))
            .spawn()
            .unwrap();
        if let Some(path) = script.str("pid_file") {
            std::fs::write(format!("{path}.child"), child.id().to_string()).unwrap();
        }
        std::mem::forget(child);
    }

    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut held: HashMap<String, String> = HashMap::new();
    let mut root = String::new();
    let mut requests = 0_u64;
    let never: Vec<String> = script
        .json
        .get("never_answer")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut asked = 0_u64;

    while let Some(message) = read_message(&mut input) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = method else {
            // The answer to one of ours.
            if message.get("result").is_some() || message.get("error").is_some() {
                log(
                    &script,
                    &json!({"answer_to": message.get("id"), "result": message.get("result"),
                            "error": message.get("error")}),
                );
            }
            continue;
        };
        log(&script, &json!({"method": method, "params": params}));
        match (method.as_str(), id) {
            ("initialize", Some(id)) => {
                if script.bool("initialize_never") {
                    continue;
                }
                if let Some(ms) = script.u64("initialize_delay_ms") {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                root = params["rootUri"].as_str().unwrap_or_default().to_owned();
                let offered = params
                    .pointer("/capabilities/general/positionEncodings")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|v| v == "utf-8"));
                let mut capabilities = json!({"textDocumentSync": 1});
                if script.str("position_encoding") == Some("utf-8") && offered {
                    capabilities["positionEncoding"] = json!("utf-8");
                }
                if script.u64("status_ms").is_some() {
                    capabilities["experimental"] = json!({"serverStatusNotification": true});
                }
                if script.bool("pull") {
                    capabilities["diagnosticProvider"] =
                        json!({"interFileDependencies": false, "workspaceDiagnostics": false});
                }
                send(
                    &out,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {
                        "capabilities": capabilities,
                        "serverInfo": {"name": "adam-lsp-script"},
                    }}),
                );
            }
            ("initialized", None) => {
                if let Some(ms) = script.u64("garbage_after_ms") {
                    let out = out.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(ms));
                        raw(&out, b"this is not a header\r\n\r\n");
                    });
                }
                if let Some(ms) = script.u64("progress_ms") {
                    let out = out.clone();
                    let delay = script.u64("progress_delay_ms").unwrap_or(0);
                    let begin = json!({"jsonrpc": "2.0", "method": "$/progress", "params": {
                        "token": "index", "value": {"kind": "begin", "title": "Indexing"}}});
                    let end = json!({"jsonrpc": "2.0", "method": "$/progress", "params": {
                        "token": "index", "value": {"kind": "end"}}});
                    if delay == 0 {
                        send(&out, &begin);
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            send(&out, &end);
                        });
                    } else {
                        // A client that has not read the begin yet: it arrives after the delay, as
                        // a busy server's does on a loaded machine.
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(delay));
                            send(&out, &begin);
                            std::thread::sleep(Duration::from_millis(ms));
                            send(&out, &end);
                        });
                    }
                }
                if let Some(ms) = script.u64("status_ms") {
                    let out = out.clone();
                    let delay = script.u64("status_delay_ms").unwrap_or(0);
                    let status = |quiescent: bool| {
                        json!({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                               "params": {"health": "ok", "quiescent": quiescent}})
                    };
                    if delay == 0 {
                        send(&out, &status(false));
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            send(&out, &status(true));
                        });
                    } else {
                        // The same, late: the client has not read "not quiescent" when it asks.
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(delay));
                            send(&out, &status(false));
                            std::thread::sleep(Duration::from_millis(ms));
                            send(&out, &status(true));
                        });
                    }
                }
                if let Some(list) = script.json.get("server_requests").and_then(Value::as_array) {
                    for item in list {
                        asked += 1;
                        send(
                            &out,
                            &json!({"jsonrpc": "2.0", "id": 9000 + asked,
                                    "method": item["method"], "params": item["params"]}),
                        );
                    }
                }
                if let Some(ms) = script.u64("stall_after_initialized_ms") {
                    std::thread::sleep(Duration::from_millis(ms));
                }
            }
            ("textDocument/didOpen", None) => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let text = params["textDocument"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                held.insert(uri.clone(), text.clone());
                publish(&script, &out, &uri, &text);
            }
            ("textDocument/didChange", None) => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let text = params["contentChanges"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                held.insert(uri.clone(), text.clone());
                publish(&script, &out, &uri, &text);
            }
            ("textDocument/didClose", None) => {
                if let Some(uri) = params["textDocument"]["uri"].as_str() {
                    held.remove(uri);
                }
            }
            ("shutdown", Some(id)) => {
                send(&out, &json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
            ("exit", None) => std::process::exit(0),
            (_, Some(id)) => {
                requests += 1;
                if script.u64("crash_on_request") == Some(requests) {
                    if let Some(text) = script.str("crash_stderr") {
                        eprintln!("{text}");
                    }
                    std::process::exit(3);
                }
                if script.u64("bad_framing_on_request") == Some(requests) {
                    raw(&out, b"Content-Type: nonsense\r\n\r\ngarbage");
                    continue;
                }
                if script.u64("oversize_on_request") == Some(requests) {
                    raw(&out, b"Content-Length: 99999999\r\n\r\n");
                    continue;
                }
                if let Some(write) = script.json.get("write_on_request")
                    && write["n"].as_u64() == Some(requests)
                {
                    let path = format!(
                        "{}/{}",
                        path_of(&root),
                        write["path"]
                            .as_str()
                            .unwrap_or("written-by-the-server.txt")
                    );
                    std::fs::write(path, write["content"].as_str().unwrap_or("x")).unwrap();
                }
                if never.contains(&method) {
                    continue;
                }
                if let Some(code) = script
                    .json
                    .get("error_on")
                    .and_then(|e| e.get(&method))
                    .and_then(Value::as_i64)
                {
                    send(
                        &out,
                        &json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": code, "message": "scripted error"}}),
                    );
                    continue;
                }
                if let Some(ms) = script.u64("answer_delay_ms") {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let result = answer(&script, &method, &uri, &root, &held);
                send(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            _ => {}
        }
    }
}

/// The notification `publishDiagnostics` for `uri`, unless the script is silent about a clean file.
fn publish(script: &Script, out: &Out, uri: &str, text: &str) {
    if script.bool("pull") {
        return;
    }
    let items = diagnostics_for(script, text);
    if items.is_empty() && script.bool("silent_when_clean") {
        return;
    }
    let uri = if script.str("uri_spelling") == Some("lower_hex") {
        lower_hex(uri)
    } else {
        uri.to_owned()
    };
    send(
        out,
        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": items}}),
    );
}

fn answer(
    script: &Script,
    method: &str,
    uri: &str,
    root: &str,
    held: &HashMap<String, String>,
) -> Value {
    if method == "textDocument/diagnostic" {
        let text = held.get(uri).map_or("", String::as_str);
        return json!({"kind": "full", "items": diagnostics_for(script, text)});
    }
    if method == "textDocument/references"
        && let Some(count) = script.u64("references_count")
    {
        return Value::Array(
            (0..count)
                .map(|i| {
                    json!({"uri": script.str("references_uri").unwrap_or(uri), "range": {
                        "start": {"line": i % 1000, "character": i / 1000},
                        "end": {"line": i % 1000, "character": i / 1000 + 1}}})
                })
                .collect(),
        );
    }
    let mut result = script
        .json
        .get("results")
        .and_then(|r| r.get(method))
        .cloned()
        .unwrap_or(Value::Null);
    substitute(&mut result, uri, root);
    result
}
