use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// The pane fields this plugin reads. Herdr sends many more; the rest are
/// display state that says nothing about how long anything took.
#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The directory the foreground process is in — a worktree, when an agent
    /// moved into one — where `cwd` stays on the pane's starting directory.
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    /// The agent's own session id, which names its transcript.
    #[serde(default)]
    pub agent_session: Option<crate::activity::AgentSession>,
}

impl Pane {
    pub fn is_working(&self) -> bool {
        self.agent_status.as_deref() == Some("working")
    }

    /// The directory that decides which project the pane's time belongs to.
    pub fn directory(&self) -> Option<&str> {
        self.foreground_cwd
            .as_deref()
            .or(self.cwd.as_deref())
            .filter(|dir| !dir.is_empty())
    }

    pub fn title(&self) -> Option<&str> {
        self.terminal_title_stripped
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
    }
}

/// A pane event, already reduced to what the tracker acts on: either a pane's
/// current state, or the pane going away.
#[derive(Debug, Clone)]
pub enum Event {
    Updated(Pane),
    Gone(String),
}

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    let default = crate::paths::home().join(".config/herdr/herdr.sock");
    if default.exists() {
        return Ok(default);
    }
    Err(anyhow!(
        "no Herdr socket: set HERDR_SOCKET_PATH or start Herdr"
    ))
}

/// Herdr serves one request per connection, newline-delimited JSON both ways.
pub fn request(method: &str, params: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket_path()?).context("connect to the Herdr socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = &stream;
    writeln!(writer, "{}", json!({"id": format!("chronoid:{method}"), "method": method, "params": params}))
        .context("send a Herdr request")?;
    writer.flush()?;

    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .context("read the Herdr reply")?;
    let reply: Value = serde_json::from_str(&line).context("parse the Herdr reply")?;
    if let Some(error) = reply.get("error") {
        return Err(anyhow!("Herdr rejected {method}: {error}"));
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("Herdr returned no result for {method}"))
}

pub fn pane_list() -> Result<Vec<Pane>> {
    let result = request("pane.list", json!({}))?;
    let panes = result
        .get("panes")
        .cloned()
        .ok_or_else(|| anyhow!("pane.list returned no panes"))?;
    serde_json::from_value(panes).context("parse the pane list")
}

/// A live subscription to every pane's state. `pane.updated` carries the whole
/// pane, so one global subscription covers focus, agent status, title and cwd.
pub struct Events {
    reader: BufReader<UnixStream>,
}

impl Events {
    pub fn subscribe(read_timeout: Duration) -> Result<Self> {
        let stream = UnixStream::connect(socket_path()?).context("connect to the Herdr socket")?;
        let mut writer = &stream;
        writeln!(
            writer,
            "{}",
            json!({
                "id": "chronoid:events",
                "method": "events.subscribe",
                "params": {"subscriptions": [
                    {"type": "pane.updated"},
                    {"type": "pane.created"},
                    {"type": "pane.focused"},
                    {"type": "pane.closed"},
                    {"type": "pane.exited"},
                ]},
            })
        )
        .context("subscribe to Herdr pane events")?;
        writer.flush()?;
        stream.set_read_timeout(Some(read_timeout))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// The next event, or `None` when the read timed out so the caller can run
    /// its periodic work and come back.
    pub fn next(&mut self) -> Result<Option<Event>> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err(anyhow!("Herdr closed the event stream")),
            Ok(_) => Ok(parse_event(&line)),
            Err(error) if would_block(&error) => Ok(None),
            Err(error) => Err(error).context("read a Herdr event"),
        }
    }
}

fn would_block(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

/// Events this plugin does not track, and the subscription acknowledgement,
/// parse to `None`.
fn parse_event(line: &str) -> Option<Event> {
    let value: Value = serde_json::from_str(line).ok()?;
    let data = value.get("data")?;
    match data.get("type")?.as_str()? {
        "pane_updated" | "pane_created" => {
            serde_json::from_value(data.get("pane")?.clone()).ok().map(Event::Updated)
        }
        "pane_closed" | "pane_exited" => Some(Event::Gone(data.get("pane_id")?.as_str()?.to_string())),
        _ => None,
    }
}
