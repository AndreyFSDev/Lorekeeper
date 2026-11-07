//! Streaming a chat model's reply from Ollama `/api/chat` (NDJSON).
//!
//! Cancellation goes through `watch`: waiting for the next line and the cancel
//! signal sit in one `select!`, so "Stop" works even while the model is quietly
//! computing the prompt. A dropped reply closes the HTTP connection, and Ollama
//! stops generating on its own.
//!
//! Execution: async, tokio runtime (a Tauri async command).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::watch;

/// How long to wait for the next line of the stream. The first line arrives after
/// the model loads and the whole prompt is counted: for a 9B at 32K context that is tens of seconds.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, PartialEq)]
pub enum StreamLine {
    /// A chunk of text; `last` — the last line of the stream may carry text too.
    Delta { text: String, last: bool },
    Done,
    Error(String),
    Ignored,
}

pub fn parse_line(line: &str) -> StreamLine {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return StreamLine::Ignored;
    };
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return StreamLine::Error(error.to_string());
    }
    let done = value.get("done").and_then(Value::as_bool) == Some(true);
    match value["message"]["content"].as_str() {
        Some(text) if !text.is_empty() => StreamLine::Delta { text: text.to_string(), last: done },
        _ if done => StreamLine::Done,
        _ => StreamLine::Ignored,
    }
}

#[derive(Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    pub cancelled: bool,
}

/// Read the line stream until `done`, an error, cancellation or idling.
/// Partial text is returned on cancellation: the user stopped the reply, but
/// what is already shown must not disappear.
pub async fn drive<S>(
    mut lines: S,
    cancel: &mut watch::Receiver<bool>,
    idle: Duration,
    mut on_delta: impl FnMut(&str),
) -> Result<Reply, String>
where
    S: Stream<Item = String> + Unpin,
{
    let mut text = String::new();
    loop {
        if *cancel.borrow() {
            return Ok(Reply { text, cancelled: true });
        }
        let next = tokio::select! {
            // Both `true` and a dropped sender mean cancellation.
            _ = cancel.changed() => return Ok(Reply { text, cancelled: true }),
            line = tokio::time::timeout(idle, lines.next()) => line,
        };
        let line = match next {
            Err(_) => return Err(format!("модель молчит дольше {} с", idle.as_secs())),
            Ok(None) => {
                return Err(if text.is_empty() {
                    "Ollama закрыла поток без ответа".to_string()
                } else {
                    "поток ответа оборвался".to_string()
                })
            }
            Ok(Some(line)) => line,
        };
        match parse_line(&line) {
            StreamLine::Delta { text: delta, last } => {
                on_delta(&delta);
                text.push_str(&delta);
                if last {
                    return Ok(Reply { text, cancelled: false });
                }
            }
            StreamLine::Done => return Ok(Reply { text, cancelled: false }),
            StreamLine::Error(e) => return Err(format!("Ollama: {e}")),
            StreamLine::Ignored => {}
        }
    }
}

/// Send a request to Ollama and read the reply as a stream.
pub async fn stream_chat(
    client: &reqwest::Client,
    ollama_port: u16,
    body: &Value,
    cancel: &mut watch::Receiver<bool>,
    on_delta: impl FnMut(&str),
) -> Result<Reply, String> {
    let request = client.post(format!("http://127.0.0.1:{ollama_port}/api/chat")).json(body).send();
    let resp = tokio::select! {
        _ = cancel.changed() => return Ok(Reply { text: String::new(), cancelled: true }),
        resp = request => resp.map_err(|e| format!("Ollama недоступна: {e}"))?,
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(text);
        return Err(format!("Ollama ответила {status}: {detail}"));
    }
    let error_slot = Arc::new(std::sync::Mutex::new(None));
    let lines = Box::pin(crate::gateway::lines_from_bytes(Box::pin(resp.bytes_stream()), error_slot.clone()));
    let result = drive(lines, cancel, IDLE_TIMEOUT, on_delta).await;
    match result {
        Err(e) => {
            let transport = error_slot.lock().ok().and_then(|slot| slot.clone());
            Err(match transport {
                Some(cause) => format!("{e} ({cause})"),
                None => e,
            })
        }
        ok => ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(content: &str, done: bool) -> String {
        serde_json::json!({ "message": { "role": "assistant", "content": content }, "done": done }).to_string()
    }

    fn lines(items: Vec<String>) -> impl Stream<Item = String> + Unpin {
        futures_util::stream::iter(items)
    }

    #[test]
    fn test_parse_line_kinds() {
        assert_eq!(parse_line(&line("При", false)), StreamLine::Delta { text: "При".into(), last: false });
        assert_eq!(parse_line(&line("!", true)), StreamLine::Delta { text: "!".into(), last: true });
        assert_eq!(parse_line(&line("", true)), StreamLine::Done);
        assert_eq!(parse_line(r#"{"error":"model not found"}"#), StreamLine::Error("model not found".into()));
        assert_eq!(parse_line("not json"), StreamLine::Ignored);
        assert_eq!(parse_line(&line("", false)), StreamLine::Ignored);
    }

    #[tokio::test]
    async fn test_drive_collects_until_done() {
        let (_tx, mut rx) = watch::channel(false);
        let mut seen = Vec::new();
        let reply = drive(
            lines(vec![line("При", false), line("вет", false), line("", true), line("лишнее", false)]),
            &mut rx,
            Duration::from_secs(1),
            |d| seen.push(d.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(reply, Reply { text: "Привет".into(), cancelled: false });
        assert_eq!(seen, vec!["При", "вет"]);
    }

    #[tokio::test]
    async fn test_text_on_final_line_is_kept() {
        let (_tx, mut rx) = watch::channel(false);
        let reply = drive(lines(vec![line("А", false), line("Б", true)]), &mut rx, Duration::from_secs(1), |_| {})
            .await
            .unwrap();
        assert_eq!(reply.text, "АБ");
    }

    #[tokio::test]
    async fn test_error_inside_stream_is_reported() {
        let (_tx, mut rx) = watch::channel(false);
        let err = drive(
            lines(vec![line("А", false), r#"{"error":"out of memory"}"#.to_string()]),
            &mut rx,
            Duration::from_secs(1),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("out of memory"), "{err}");
    }

    #[tokio::test]
    async fn test_stream_without_done_is_an_error() {
        let (_tx, mut rx) = watch::channel(false);
        let err = drive(lines(vec![line("А", false)]), &mut rx, Duration::from_secs(1), |_| {}).await.unwrap_err();
        assert!(err.contains("оборвался"), "{err}");
    }

    #[tokio::test]
    async fn test_cancel_while_model_is_silent_returns_partial() {
        let (tx, mut rx) = watch::channel(false);
        // The first chunk arrives, then the model "goes silent" — the stream never ends.
        let source = futures_util::stream::iter(vec![line("Начало", false)]).chain(futures_util::stream::pending());
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tx.send(true).unwrap();
            tx
        });
        let reply = drive(Box::pin(source), &mut rx, Duration::from_secs(5), |_| {}).await.unwrap();
        assert_eq!(reply, Reply { text: "Начало".into(), cancelled: true });
        drop(canceller.await.unwrap());
    }

    #[tokio::test]
    async fn test_idle_timeout() {
        let (_tx, mut rx) = watch::channel(false);
        let source = futures_util::stream::pending::<String>();
        let err = drive(Box::pin(source), &mut rx, Duration::from_millis(30), |_| {}).await.unwrap_err();
        assert!(err.contains("молчит"), "{err}");
    }
}
