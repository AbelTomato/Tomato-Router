use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    pub fn event_type(&self) -> Option<String> {
        if let Some(event) = &self.event {
            return Some(event.clone());
        }
        serde_json::from_str::<Value>(&self.data)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    }

    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    id: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                self.process_line(&mut events);
            } else {
                self.line.push(byte);
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.process_line(&mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn process_line(&mut self, events: &mut Vec<SseEvent>) {
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line[0] == b':' {
            return;
        }

        let line = String::from_utf8_lossy(&line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            self.id = None;
            return;
        }
        events.push(SseEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
            id: self.id.take(),
        });
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_events_and_comments() {
        let mut parser = SseParser::new();
        assert!(parser.push(b": keepalive\n\nevent: response.").is_empty());
        let events = parser.push(b"created\ndata: {\"type\":\"response.created\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("response.created"));
        assert_eq!(events[0].data, "{\"type\":\"response.created\"}");
    }

    #[test]
    fn joins_multiline_data_and_handles_crlf() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: line1\r\ndata: line2\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn derives_event_type_from_json() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: {\"type\":\"response.completed\"}\n\n");
        assert_eq!(
            events[0].event_type().as_deref(),
            Some("response.completed")
        );
    }

    #[test]
    fn flushes_trailing_event_without_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: [DONE]\n").is_empty());
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert!(events[0].is_done());
    }
}
