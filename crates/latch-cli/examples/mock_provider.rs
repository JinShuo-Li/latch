//! Deterministic OpenAI-compatible streaming provider for harnesses.
//!
//! Serves scripted turns over a loopback HTTP endpoint so the real `latch`
//! binary can run end-to-end without a live model. It is the process-level
//! counterpart of the mock provider embedded in `crates/latch-cli/tests/cli.rs`:
//! every request consumes the next scripted turn, and once the script is
//! exhausted the last turn repeats.
//!
//! ```sh
//! cat > script.json <<'JSON'
//! [
//!   {"tool_call": {"name": "write", "arguments": {"path": "a.txt", "content": "hi"}}},
//!   {"text": "done"}
//! ]
//! JSON
//! cargo run -p latch-cli --example mock_provider -- --script script.json --port 8731
//! ```
//!
//! The first stdout line is always `MOCK_PROVIDER_READY <base-url>`, so a
//! harness can wait for readiness without guessing.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Parser)]
#[command(
    name = "latch-mock-provider",
    version,
    about = "Deterministic OpenAI-compatible SSE provider for Latch harnesses"
)]
struct Args {
    /// JSON file: an array of `{"text": "..."}` or `{"tool_call": {...}}` turns.
    #[arg(long, value_name = "PATH")]
    script: PathBuf,
    /// Loopback port to listen on.
    #[arg(long, default_value_t = 8731)]
    port: u16,
    /// Address to bind. Keep the loopback default; the harness needs no more.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[derive(Clone)]
enum Turn {
    Text(String),
    ToolCall { name: String, arguments: Value },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.script)
        .with_context(|| format!("read mock script {}", args.script.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse mock script {}", args.script.display()))?;
    let turns = Arc::new(parse_turns(&value)?);

    let listener = TcpListener::bind((args.host.as_str(), args.port))
        .with_context(|| format!("bind {}:{}", args.host, args.port))?;
    let addr = listener.local_addr()?;
    println!("MOCK_PROVIDER_READY http://{addr}/v1");
    std::io::stdout().flush()?;

    let counter = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let turns = Arc::clone(&turns);
        let counter = Arc::clone(&counter);
        std::thread::spawn(move || {
            if let Err(error) = respond(&mut stream, &turns, &counter) {
                eprintln!("mock provider connection error: {error}");
            }
        });
    }
    Ok(())
}

fn parse_turns(raw: &Value) -> Result<Vec<Turn>> {
    let array = raw
        .as_array()
        .context("mock script must be a JSON array of turns")?;
    if array.is_empty() {
        bail!("mock script must contain at least one turn");
    }
    array
        .iter()
        .map(|entry| {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                return Ok(Turn::Text(text.to_owned()));
            }
            if let Some(call) = entry.get("tool_call") {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .context("tool_call.name must be a string")?;
                let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
                return Ok(Turn::ToolCall {
                    name: name.to_owned(),
                    arguments,
                });
            }
            bail!("each turn needs a \"text\" or \"tool_call\" field")
        })
        .collect()
}

fn respond(stream: &mut TcpStream, turns: &[Turn], counter: &AtomicUsize) -> std::io::Result<()> {
    // Drain the request so the client can finish writing before we reply.
    let _ = read_request_body(stream)?;
    let index = counter.fetch_add(1, Ordering::SeqCst);
    let turn = &turns[index.min(turns.len() - 1)];
    let response = sse_response(turn, index);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn read_request_body(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn sse_response(turn: &Turn, index: usize) -> String {
    let mut events = Vec::new();
    let finish = match turn {
        Turn::Text(text) => {
            events.push(json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}));
            "stop"
        }
        Turn::ToolCall { name, arguments } => {
            events.push(json!({"choices":[{"delta":{
                "tool_calls":[{
                    "index":0,
                    "id":format!("call_{index}"),
                    "type":"function",
                    "function":{"name":name,"arguments":arguments.to_string()}
                }]
            },"finish_reason":null}]}));
            "tool_calls"
        }
    };
    events.push(json!({"choices":[{"delta":{},"finish_reason":finish}]}));
    let mut body = String::new();
    for event in &events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_parse_text_and_tool_calls() {
        let value: Value = serde_json::from_str(
            r#"[{"text":"hi"},{"tool_call":{"name":"write","arguments":{"path":"a"}}}]"#,
        )
        .unwrap();
        let turns = parse_turns(&value).unwrap();
        assert_eq!(turns.len(), 2);
        assert!(matches!(&turns[0], Turn::Text(text) if text == "hi"));
        assert!(matches!(
            &turns[1],
            Turn::ToolCall { name, .. } if name == "write"
        ));
    }

    #[test]
    fn empty_scripts_are_rejected() {
        assert!(parse_turns(&json!([])).is_err());
        assert!(parse_turns(&json!([{"unknown": true}])).is_err());
    }

    #[test]
    fn text_and_tool_turns_stream_valid_sse() {
        let text = sse_response(&Turn::Text("hello".into()), 0);
        assert!(text.contains("data: ") && text.ends_with("data: [DONE]\n\n"));
        assert!(text.contains("\"content\":\"hello\""));
        let tool = sse_response(
            &Turn::ToolCall {
                name: "write".into(),
                arguments: json!({"path":"a"}),
            },
            3,
        );
        assert!(tool.contains("\"finish_reason\":\"tool_calls\""));
        assert!(tool.contains("\"id\":\"call_3\""));
    }
}
