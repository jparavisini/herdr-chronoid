use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const MCP_URL: &str = "http://127.0.0.1:17686/mcp";

/// Chronoid serves MCP over loopback while the app is running, guarded by a
/// per-user bearer token it writes to Application Support with mode 0600.
fn auth_token() -> Result<String> {
    for bundle in ["com.vu.Chronoid", "com.vu.Chronoid.dev"] {
        let path = crate::paths::home()
            .join("Library/Application Support")
            .join(bundle)
            .join("mcp-auth-token");
        if let Ok(token) = std::fs::read_to_string(&path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }
    Err(anyhow!("no Chronoid auth token: open Chronoid once to create it"))
}

pub struct Chronoid {
    agent: ureq::Agent,
    token: String,
    next_id: std::cell::Cell<u64>,
}

impl Chronoid {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .build(),
            token: auth_token()?,
            next_id: std::cell::Cell::new(1),
        })
    }

    /// Chronoid's answer to one tool call. `Err` means there was no answer:
    /// Chronoid is closed, or the request never got through. A refusal is an
    /// answer, reported inside a successful JSON-RPC reply, as a JSON-RPC
    /// error, or as an HTTP client error, and asking again will not change it.
    /// An auth failure or a server error is not an answer about the entry.
    fn call(&self, tool: &str, arguments: Value) -> Result<Reply> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let response = self
            .agent
            .post(MCP_URL)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .send_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }));
        let response = match response {
            Err(ureq::Error::Status(code, response)) if (400..500).contains(&code) && code != 401 && code != 403 => {
                let body = response.into_string().unwrap_or_default();
                return Ok(Reply::Refused(format!("Chronoid refused {tool} with HTTP {code}: {body}")));
            }
            other => other.with_context(|| format!("call Chronoid {tool} (is Chronoid running?)"))?,
        };
        let reply: Value = response.into_json().context("parse the Chronoid reply")?;

        if let Some(error) = reply.get("error") {
            return Ok(Reply::Refused(format!("Chronoid rejected {tool}: {error}")));
        }
        let result = reply
            .get("result")
            .ok_or_else(|| anyhow!("Chronoid returned no result for {tool}"))?;
        let text = result
            .get("content")
            .and_then(|content| content.get(0))
            .and_then(|first| first.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(Reply::Refused(format!("Chronoid {tool} failed: {text}")));
        }
        Ok(Reply::Text(text))
    }

    /// Whether Chronoid recorded any activity of its own on a day. Its day
    /// report wants `device_ids` even though the published schema calls it
    /// optional, so this asks for this Mac explicitly.
    pub fn tracked_anything(&self, date: &str) -> Result<bool> {
        let text = match self.call(
            "get_full_day_report",
            json!({"start_date": date, "end_date": date, "device_ids": [-1], "min_span_seconds": 86400}),
        )? {
            Reply::Text(text) => text,
            Reply::Refused(reason) => return Err(anyhow!(reason)),
        };
        let total = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("Total tracked:"))
            .ok_or_else(|| anyhow!("Chronoid's day report has no total for {date}"))?;
        Ok(total.trim() != "0m")
    }

    pub fn create_time_entry(&self, entry: &TimeEntry) -> Result<Entry> {
        let reply = self.call(
            "create_manual_time_entry",
            json!({
                "title": entry.title,
                "start_time": entry.start,
                "end_time": entry.end,
                "category_id": entry.project_id,
                "notes": entry.notes,
                "entry_kind": entry.kind,
                "assignment_policy": entry.assignment_policy,
            }),
        )?;
        Ok(match reply {
            Reply::Text(_) => Entry::Created,
            Reply::Refused(reason) if reason.contains("cannot overlap") => Entry::Overlaps,
            Reply::Refused(reason) => Entry::Refused(reason),
        })
    }
}

enum Reply {
    Text(String),
    Refused(String),
}

/// What Chronoid did with a time entry it received.
pub enum Entry {
    Created,
    /// Another manual entry already holds some of those minutes. Chronoid
    /// refuses any overlap, of any kind, in any project.
    Overlaps,
    Refused(String),
}

pub struct TimeEntry<'a> {
    pub title: &'a str,
    pub notes: &'a str,
    pub start: String,
    pub end: String,
    pub project_id: i64,
    /// `project_overlay` claims the tracked activity underneath it;
    /// `annotation_only` records the span without touching tracked time.
    pub kind: &'a str,
    pub assignment_policy: &'a str,
}

