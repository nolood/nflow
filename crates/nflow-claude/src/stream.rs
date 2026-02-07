use serde::Deserialize;
use serde_json::Value;
use tokio::io::{BufReader, Lines};
use tokio::process::ChildStdout;

use crate::error::ClaudeError;

/// A typed event parsed from Claude's stream-json output.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Incremental text output from Claude.
    TextDelta { text: String },
    /// Claude is using a tool.
    ToolUse { name: String, input: Value },
    /// Result of a tool call.
    ToolResult { content: String },
    /// Final result with session ID.
    Result { text: String, session_id: String },
    /// An error reported by Claude.
    Error { message: String },
    /// A line that could not be parsed as valid JSON.
    ParseError { line: String, reason: String },
}

impl StreamEvent {
    /// Returns true if this is an AskUserQuestion tool call.
    pub fn is_ask_user_question(&self) -> bool {
        matches!(self, StreamEvent::ToolUse { name, .. } if name == "AskUserQuestion")
    }

    /// If this is a Result event, returns the session_id.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            StreamEvent::Result { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        }
    }
}

/// Raw JSON structures for deserializing stream-json lines.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: String,
    // stream_event fields
    event: Option<RawStreamEvent>,
    // tool_use fields
    name: Option<String>,
    input: Option<Value>,
    // tool_result fields
    content: Option<Value>,
    // result fields
    result: Option<String>,
    session_id: Option<String>,
    // error fields
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawStreamEvent {
    delta: Option<RawDelta>,
}

#[derive(Deserialize)]
struct RawDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
}

/// Parses a single JSON line into a StreamEvent.
pub fn parse_line(line: &str) -> StreamEvent {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return StreamEvent::ParseError {
            line: line.to_string(),
            reason: "empty line".to_string(),
        };
    }

    let raw: RawEvent = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            return StreamEvent::ParseError {
                line: line.to_string(),
                reason: e.to_string(),
            };
        }
    };

    match raw.event_type.as_str() {
        "stream_event" => {
            if let Some(event) = raw.event {
                if let Some(delta) = event.delta {
                    if delta.delta_type.as_deref() == Some("text_delta") {
                        if let Some(text) = delta.text {
                            return StreamEvent::TextDelta { text };
                        }
                    }
                }
            }
            // stream_event without a text delta — treat as parse error
            StreamEvent::ParseError {
                line: line.to_string(),
                reason: "stream_event missing text_delta".to_string(),
            }
        }
        "tool_use" => {
            let name = raw.name.unwrap_or_default();
            let input = raw.input.unwrap_or(Value::Null);
            StreamEvent::ToolUse { name, input }
        }
        "tool_result" => {
            let content = match raw.content {
                Some(Value::String(s)) => s,
                Some(other) => other.to_string(),
                None => String::new(),
            };
            StreamEvent::ToolResult { content }
        }
        "result" => StreamEvent::Result {
            text: raw.result.unwrap_or_default(),
            session_id: raw.session_id.unwrap_or_default(),
        },
        "error" => StreamEvent::Error {
            message: raw.error.unwrap_or_default(),
        },
        _ => StreamEvent::ParseError {
            line: line.to_string(),
            reason: format!("unknown event type: {}", raw.event_type),
        },
    }
}

/// Reads stream-json lines from a Claude process stdout and emits typed events.
///
/// Processes line-by-line without buffering the entire output in memory.
/// Yields `None` when the stream ends.
pub struct StreamParser {
    lines: Lines<BufReader<ChildStdout>>,
    /// The session_id extracted from the final result event, if seen.
    last_session_id: Option<String>,
}

impl StreamParser {
    /// Create a new StreamParser from a line-buffered stdout reader.
    pub fn new(lines: Lines<BufReader<ChildStdout>>) -> Self {
        Self {
            lines,
            last_session_id: None,
        }
    }

    /// Read the next event from the stream.
    ///
    /// Returns `Ok(Some(event))` for each parsed line, `Ok(None)` at end of stream.
    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ClaudeError> {
        match self.lines.next_line().await? {
            Some(line) => {
                let event = parse_line(&line);
                if let StreamEvent::Result { session_id, .. } = &event {
                    if !session_id.is_empty() {
                        self.last_session_id = Some(session_id.clone());
                    }
                }
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }

    /// Returns the session_id captured from the most recent Result event.
    pub fn session_id(&self) -> Option<&str> {
        self.last_session_id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    // --- parse_line tests ---

    #[test]
    fn parse_text_delta() {
        let line =
            r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Reading"}}}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::TextDelta {
                text: "Reading".to_string()
            }
        );
    }

    #[test]
    fn parse_tool_use() {
        let line = r#"{"type":"tool_use","name":"Read","input":{"file_path":"src/main.rs"}}"#;
        let event = parse_line(line);
        match event {
            StreamEvent::ToolUse { name, input } => {
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "src/main.rs");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_result() {
        let line = r#"{"type":"tool_result","content":"file contents here"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::ToolResult {
                content: "file contents here".to_string()
            }
        );
    }

    #[test]
    fn parse_tool_result_json_content() {
        let line = r#"{"type":"tool_result","content":{"key":"value"}}"#;
        let event = parse_line(line);
        match event {
            StreamEvent::ToolResult { content } => {
                assert!(content.contains("key"));
                assert!(content.contains("value"));
            }
            other => panic!("expected ToolResult, got: {other:?}"),
        }
    }

    #[test]
    fn parse_result() {
        let line = r#"{"type":"result","result":"Task completed.","session_id":"abc-123"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::Result {
                text: "Task completed.".to_string(),
                session_id: "abc-123".to_string()
            }
        );
    }

    #[test]
    fn parse_error() {
        let line = r#"{"type":"error","error":"something went wrong"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::Error {
                message: "something went wrong".to_string()
            }
        );
    }

    #[test]
    fn parse_malformed_json() {
        let line = "not valid json at all";
        let event = parse_line(line);
        match event {
            StreamEvent::ParseError { line: l, reason } => {
                assert_eq!(l, "not valid json at all");
                assert!(!reason.is_empty());
            }
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_empty_line() {
        let event = parse_line("");
        match event {
            StreamEvent::ParseError { reason, .. } => {
                assert_eq!(reason, "empty line");
            }
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_whitespace_only_line() {
        let event = parse_line("   ");
        match event {
            StreamEvent::ParseError { reason, .. } => {
                assert_eq!(reason, "empty line");
            }
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event_type() {
        let line = r#"{"type":"unknown_thing","data":"hello"}"#;
        let event = parse_line(line);
        match event {
            StreamEvent::ParseError { reason, .. } => {
                assert!(reason.contains("unknown event type"));
            }
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_stream_event_missing_delta() {
        let line = r#"{"type":"stream_event","event":{}}"#;
        let event = parse_line(line);
        match event {
            StreamEvent::ParseError { reason, .. } => {
                assert!(reason.contains("missing text_delta"));
            }
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_missing_name() {
        let line = r#"{"type":"tool_use","input":{}}"#;
        let event = parse_line(line);
        match event {
            StreamEvent::ToolUse { name, .. } => {
                assert_eq!(name, "");
            }
            other => panic!("expected ToolUse with empty name, got: {other:?}"),
        }
    }

    #[test]
    fn parse_result_missing_session_id() {
        let line = r#"{"type":"result","result":"done"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::Result {
                text: "done".to_string(),
                session_id: String::new()
            }
        );
    }

    #[test]
    fn parse_line_with_leading_whitespace() {
        let line =
            r#"  {"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hi"}}}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::TextDelta {
                text: "hi".to_string()
            }
        );
    }

    // --- AskUserQuestion detection ---

    #[test]
    fn detect_ask_user_question() {
        let line = r#"{"type":"tool_use","name":"AskUserQuestion","input":{"question":"What language?","options":["Rust","Go"]}}"#;
        let event = parse_line(line);
        assert!(event.is_ask_user_question());
    }

    #[test]
    fn non_ask_user_question_tool() {
        let line = r#"{"type":"tool_use","name":"Read","input":{"file_path":"main.rs"}}"#;
        let event = parse_line(line);
        assert!(!event.is_ask_user_question());
    }

    #[test]
    fn text_delta_is_not_ask_user_question() {
        let event = StreamEvent::TextDelta {
            text: "hello".to_string(),
        };
        assert!(!event.is_ask_user_question());
    }

    // --- session_id extraction ---

    #[test]
    fn session_id_from_result() {
        let event = StreamEvent::Result {
            text: "done".to_string(),
            session_id: "sess-42".to_string(),
        };
        assert_eq!(event.session_id(), Some("sess-42"));
    }

    #[test]
    fn session_id_from_non_result() {
        let event = StreamEvent::TextDelta {
            text: "hi".to_string(),
        };
        assert_eq!(event.session_id(), None);
    }

    // --- StreamParser integration tests ---

    #[tokio::test]
    async fn stream_parser_reads_events() {
        // Simulate Claude output by writing JSON lines to a child process's stdout
        let json_lines = vec![
            r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"tool_use","name":"Read","input":{"file_path":"main.rs"}}"#,
            r#"{"type":"tool_result","content":"fn main() {}"}"#,
            r#"{"type":"result","result":"All done.","session_id":"sess-99"}"#,
        ];
        let input = json_lines.join("\n");

        // Use echo to pipe the JSON lines as stdout
        let mut child = tokio::process::Command::new("printf")
            .arg(format!("{}\n", input))
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("printf should spawn");

        let stdout = child.stdout.take().expect("stdout piped");
        let lines = tokio::io::BufReader::new(stdout).lines();
        let mut parser = StreamParser::new(lines);

        // Event 1: TextDelta
        let event = parser.next_event().await.unwrap().unwrap();
        assert_eq!(
            event,
            StreamEvent::TextDelta {
                text: "Hello".to_string()
            }
        );
        assert!(parser.session_id().is_none());

        // Event 2: ToolUse
        let event = parser.next_event().await.unwrap().unwrap();
        match event {
            StreamEvent::ToolUse { name, .. } => assert_eq!(name, "Read"),
            other => panic!("expected ToolUse, got: {other:?}"),
        }

        // Event 3: ToolResult
        let event = parser.next_event().await.unwrap().unwrap();
        assert_eq!(
            event,
            StreamEvent::ToolResult {
                content: "fn main() {}".to_string()
            }
        );

        // Event 4: Result
        let event = parser.next_event().await.unwrap().unwrap();
        assert_eq!(
            event,
            StreamEvent::Result {
                text: "All done.".to_string(),
                session_id: "sess-99".to_string()
            }
        );
        assert_eq!(parser.session_id(), Some("sess-99"));

        // Stream ends
        let event = parser.next_event().await.unwrap();
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn stream_parser_handles_malformed_lines() {
        let input = "not json\n{\"type\":\"result\",\"result\":\"ok\",\"session_id\":\"s1\"}\n";

        let mut child = tokio::process::Command::new("printf")
            .arg(input)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("printf should spawn");

        let stdout = child.stdout.take().expect("stdout piped");
        let lines = tokio::io::BufReader::new(stdout).lines();
        let mut parser = StreamParser::new(lines);

        // First line: ParseError (does not crash)
        let event = parser.next_event().await.unwrap().unwrap();
        match event {
            StreamEvent::ParseError { .. } => {}
            other => panic!("expected ParseError, got: {other:?}"),
        }

        // Second line: valid Result
        let event = parser.next_event().await.unwrap().unwrap();
        assert_eq!(
            event,
            StreamEvent::Result {
                text: "ok".to_string(),
                session_id: "s1".to_string()
            }
        );
        assert_eq!(parser.session_id(), Some("s1"));
    }

    #[tokio::test]
    async fn stream_parser_detects_ask_user_question() {
        let input = r#"{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Pick one","options":["A","B"]}}"#;
        let full_input = format!("{input}\n");

        let mut child = tokio::process::Command::new("printf")
            .arg(&full_input)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("printf should spawn");

        let stdout = child.stdout.take().expect("stdout piped");
        let lines = tokio::io::BufReader::new(stdout).lines();
        let mut parser = StreamParser::new(lines);

        let event = parser.next_event().await.unwrap().unwrap();
        assert!(event.is_ask_user_question());
    }

    #[tokio::test]
    async fn stream_parser_session_id_tracks_last_result() {
        let input = [
            r#"{"type":"result","result":"first","session_id":"s1"}"#,
            r#"{"type":"result","result":"second","session_id":"s2"}"#,
        ]
        .join("\n");
        let full_input = format!("{input}\n");

        let mut child = tokio::process::Command::new("printf")
            .arg(&full_input)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("printf should spawn");

        let stdout = child.stdout.take().expect("stdout piped");
        let lines = tokio::io::BufReader::new(stdout).lines();
        let mut parser = StreamParser::new(lines);

        parser.next_event().await.unwrap();
        assert_eq!(parser.session_id(), Some("s1"));

        parser.next_event().await.unwrap();
        assert_eq!(parser.session_id(), Some("s2"));
    }

    #[test]
    fn parse_tool_result_no_content() {
        let line = r#"{"type":"tool_result"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::ToolResult {
                content: String::new()
            }
        );
    }

    #[test]
    fn parse_error_no_message() {
        let line = r#"{"type":"error"}"#;
        let event = parse_line(line);
        assert_eq!(
            event,
            StreamEvent::Error {
                message: String::new()
            }
        );
    }

    #[test]
    fn stream_event_clone() {
        let event = StreamEvent::TextDelta {
            text: "hello".to_string(),
        };
        let cloned = event.clone();
        assert_eq!(event, cloned);
    }

    #[test]
    fn stream_event_debug() {
        let event = StreamEvent::TextDelta {
            text: "hello".to_string(),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("TextDelta"));
        assert!(debug.contains("hello"));
    }
}
