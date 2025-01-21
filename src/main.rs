use std::path::Path;
use std::error::Error;
use std::process::Command;

use parking_lot::RwLock;
use serde_json::from_value;
use serde::Deserialize;
use regex::Regex;
use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::*;

lazy_static::lazy_static! {
    static ref RANGE1: Regex = Regex::new(r"line (\d+), column (\d+) - (\d+)").unwrap();
    static ref RANGE2: Regex = Regex::new(r"line (\d+), column (\d+) - line (\d+), column (\d+)").unwrap();
    static ref RANGE3: Regex = Regex::new(r"line (\d+)").unwrap();
    static ref COL: Regex = Regex::new(r"\(char (\d+)\)").unwrap();
}

#[derive(Default, Debug, Deserialize)]
struct Config {
    #[serde(default, alias = "ottFlags")]
    ott_flags: Vec<String>,
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "--version" {
            println!(env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }

    // Create the transport, run the server
    let (connection, io_threads) = Connection::stdio();
    let server_capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::FULL),
            save: Some(SaveOptions::default().into()),
            ..Default::default()
        })),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: None,
            file_operations: None,
        }),
        ..Default::default()
    })?;

    let config = RwLock::new(Config::default());
    connection.initialize(server_capabilities)?;

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }

                match req.method.as_str() {
                    "textDocument/documentSymbol" => {
                        connection.sender.send(Message::Response( Response {
                            id: req.id,
                            result: Some(serde_json::Value::Array(vec![])),
                            error: None,
                        }))?;
                    }
                    _ => {}
                }
            }
            Message::Response(_resp) => {}
            Message::Notification(not) => {
                match not.method.as_str() {
                    "workspace/didChangeConfiguration" => {
                        let params: DidChangeConfigurationParams = from_value(not.params)?;
                        if let Ok(new_config) = serde_json::from_value(params.settings) {
                            *config.write() = new_config;
                        }
                    }
                    "textDocument/didOpen" => {
                        let params: DidOpenTextDocumentParams = from_value(not.params)?;
                        let uri = &params.text_document.uri;
                        check_ott_file(&*config.read(), uri.path().as_str(), uri, &connection)?;
                    }
                    "textDocument/didSave" => {
                        let params: DidSaveTextDocumentParams = from_value(not.params)?;
                        let uri = &params.text_document.uri;
                        check_ott_file(&*config.read(), uri.path().as_str(), uri, &connection)?;
                    }
                    _ => {}
                }
            }
        }
    }

    io_threads.join()?;
    Ok(())
}

fn publish_diagnostics(
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
    connection: &Connection,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let params = PublishDiagnosticsParams { uri, diagnostics, version: None, };
    let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);
    connection.sender.send(Message::Notification(notification))?;
    Ok(())
}

fn check_ott_file(
    config: &Config,
    file_path: &str,
    uri: &Uri,
    connection: &Connection,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    if !Path::new(file_path).is_file() {
        let warning = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::INFORMATION),
            message: format!("file path {file_path} is not a file"),
            ..Default::default()
        };

        return publish_diagnostics(uri.clone(), vec![warning], &connection);
    }

    let output = Command::new("ott")
        .arg("-signal_parse_errors")
        .arg("true")
        .arg("-colour")
        .arg("false")
        .args(&config.ott_flags)
        .arg(file_path)
        .output()?;

    let mut diagnostics = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("File") {
            // Start of an error or warning block
            let mut line_start = None;
            let mut line_end = None;
            let mut column_start = None;
            let mut column_end = None;
            let mut message = Vec::new();
            let mut severity = None;

            // Parse line and column numbers using regex
            if let Some(caps) = RANGE1.captures(line) {
                line_start = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
                column_start = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
                column_end = caps.get(3).and_then(|m| m.as_str().parse::<u32>().ok());
            } else if let Some(caps) = RANGE2.captures(line) {
                line_start = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
                column_start = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
                line_end = caps.get(3).and_then(|m| m.as_str().parse::<u32>().ok());
                column_end = caps.get(4).and_then(|m| m.as_str().parse::<u32>().ok());
            } else if let Some(caps) = RANGE3.captures(line) {
                line_start = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
            }

            // Collect message until we hit a blank line or next "File" line
            while let Some(current_line) = lines.peek() {
                if current_line.starts_with("File") {
                    break;
                }

                if let Some(msg) = current_line.strip_prefix("Error:") {
                    severity = Some(DiagnosticSeverity::ERROR);
                    let trimmed = msg.trim();
                    if !trimmed.is_empty() {
                        message.push(trimmed);
                    }
                } else if let Some(msg) = current_line.strip_prefix("Warning:") {
                    severity = Some(DiagnosticSeverity::WARNING);
                    let trimmed = msg.trim();
                    if !trimmed.is_empty() {
                        message.push(trimmed);
                    }
                } else if let Some(caps) = COL.captures(current_line) {
                    if column_start.is_none() {
                        column_start = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
                    }
                } else if !current_line.starts_with("Definition rule") {
                    message.push(current_line.trim());
                }

                lines.next();
            }

            let message = message.is_empty()
                .then(|| "unknown ott diagnostic message".into())
                .unwrap_or(message.join(" "));

            // Create diagnostic range
            let line_start = line_start.map(|l| l - 1).unwrap_or(0);
            let line_end = line_end.map(|l| l - 1).unwrap_or(line_start);
            let range = match (column_start, column_end) {
                (Some(col_start), Some(col_end)) => Range::new(
                    Position::new(line_start, col_start),
                    Position::new(line_end, col_end),
                ),
                (Some(col), None) => Range::new(
                    Position::new(line_start, col),
                    Position::new(line_end, col + message.len() as u32),
                ),
                (None, _) => Range::new(
                    Position::new(line_start, 0),
                    Position::new(line_end, 0),
                ),
            };

            diagnostics.push(Diagnostic { range, severity, message, ..Default::default() });
        }
    }

    // emit a general error if no specific errors/warnings were found
    if diagnostics.is_empty() && !output.status.success() {
        diagnostics.push(Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "ott processing failed".to_string(),
            ..Default::default()
        });
    }

    publish_diagnostics(uri.clone(), diagnostics, &connection)
}
