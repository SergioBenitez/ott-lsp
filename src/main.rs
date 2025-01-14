use std::{error::Error, path::Path};
use std::process::Command;

use lsp_types::{Position, Range, *};
use lsp_server::{Connection, Message, Notification, Response};
use regex::Regex;

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "--version" {
            println!(env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }

    // Create the transport
    let (connection, io_threads) = Connection::stdio();

    // Run the server
    let server_capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::FULL),
            save: Some(SaveOptions::default().into()),
            ..Default::default()
        })),
        document_symbol_provider: Some(OneOf::Left(true)),
        ..Default::default()
    })?;

    connection.initialize(server_capabilities)?;
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                match req.method.as_str() {
                    "textDocument/documentSymbol" => {
                        let resp = Response {
                            id: req.id,
                            result: Some(serde_json::Value::Array(vec![])),
                            error: None,
                        };
                        connection.sender.send(Message::Response(resp))?;
                    }
                    _ => {}
                }
            }
            Message::Response(_resp) => {}
            Message::Notification(not) => {
                match not.method.as_str() {
                    "textDocument/didOpen" => {
                        let params: DidOpenTextDocumentParams = serde_json::from_value(not.params)?;
                        let file_path = params.text_document.uri.path();
                        check_ott_file(file_path.as_str(), &params.text_document.uri, &connection)?;
                    }
                    "textDocument/didSave" => {
                        let params: DidSaveTextDocumentParams = serde_json::from_value(not.params)?;
                        let file_path = params.text_document.uri.path();
                        check_ott_file(file_path.as_str(), &params.text_document.uri, &connection)?;
                    }
                    _ => {}
                }
            }
        }
    }

    io_threads.join()?;
    Ok(())
}

lazy_static::lazy_static! {
    static ref RANGE1: Regex = Regex::new(r"line (\d+), column (\d+) - (\d+)").unwrap();
    static ref RANGE2: Regex = Regex::new(r"line (\d+), column (\d+) - line (\d+), column (\d+)").unwrap();
    static ref RANGE3: Regex = Regex::new(r"line (\d+)").unwrap();
}

fn check_ott_file(
    file_path: &str,
    uri: &Uri,
    connection: &Connection,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    if !Path::new(file_path).is_file() {
        let warning = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            message: format!("file path {file_path} is not a file"),
            ..Default::default()
        };

        return publish_diagnostics(uri.clone(), vec![warning], &connection);
    }

    let file_contents = std::fs::read_to_string(file_path)?;
    let file_lines: Vec<&str> = file_contents.lines().collect();

    let output = Command::new("ott")
        .arg("-signal_parse_errors")
        .arg("true")
        .arg("-colour")
        .arg("false")
        .arg(file_path)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut diagnostics = Vec::new();

    if !output.status.success() {
        let mut lines = stdout.lines().peekable();
        while let Some(line) = lines.next() {
            if line.starts_with("File") {
                // Start of an error block
                let mut line_start = None;
                let mut line_end = None;
                let mut column_start = None;
                let mut column_end = None;
                let mut error_msg = Vec::new();

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

                // Collect all lines until we hit a blank line
                while let Some(current_line) = lines.peek() {
                    if current_line.is_empty() {
                        lines.next();  // consume the blank line
                        break;
                    }

                    if current_line.starts_with("Error:") {
                        // Start collecting error message
                        if let Some(msg) = current_line.strip_prefix("Error:") {
                            let trimmed = msg.trim();
                            if !trimmed.is_empty() {
                                error_msg.push(trimmed.to_string());
                            }
                        }
                    } else if current_line.contains("char") {
                        // Parse column from "char" format if we don't already have it
                        if column_start.is_none() {
                            if let Some(char_pos) = current_line.split("char ").nth(1) {
                                if let Some(col) = char_pos.split("):").next() {
                                    column_start = col.trim().parse::<u32>().ok();
                                }
                            }
                        }
                    } else if !current_line.starts_with("File") {
                        // Collect additional error message lines
                        error_msg.push(current_line.trim().to_string());
                    }

                    lines.next();
                }

                let line_start = line_start.map(|l| l - 1).unwrap_or(0);
                let line_end = line_end.map(|l| l - 1).unwrap_or(line_start);

                // Create diagnostic range
                let range = match (column_start, column_end) {
                    (Some(col_start), Some(col_end)) => Range::new(
                        Position::new(line_start, col_start),
                        Position::new(line_end, col_end),
                    ),
                    (Some(col), None) => {
                        let line_idx = line_end as usize;
                        let line_length = if line_idx < file_lines.len() {
                            file_lines[line_idx].len()
                        } else {
                            col as usize + 1
                        };
                        Range::new(
                            Position::new(line_start, col),
                            Position::new(line_end, line_length as u32),
                        )
                    },
                    (None, _) => Range::new(
                        Position::new(line_start, 0),
                        Position::new(line_end, 0),
                    ),
                };

                // Join multi-line error messages
                let error_message = match &*error_msg {
                    &[] => "unknown ott error".to_string(),
                    _ => error_msg.join(" ")
                };

                diagnostics.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: error_message,
                    ..Default::default()
                });
            }
        }

        // If we found no specific errors but the command failed, emit a general error
        if diagnostics.is_empty() {
            diagnostics.push(Diagnostic {
                range: Range::default(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "ott processing failed".to_string(),
                ..Default::default()
            });
        }
    }

    publish_diagnostics(uri.clone(), diagnostics, &connection)
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
