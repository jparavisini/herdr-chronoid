//! Readers that turn each agent's session history into one shape: timestamped
//! activity in a directory. What counts as a working turn, and what counts as
//! you, is decided once in the parent module, not per agent.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// You sent the agent something.
    Prompt,
    /// The agent did something: a reply, a tool call, a tool result.
    Agent,
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub time: i64,
    pub kind: Kind,
}

/// One agent session, in time order.
#[derive(Debug, Clone)]
pub struct Session {
    pub agent: &'static str,
    pub id: String,
    pub directory: String,
    /// The branch the work was on, where the history records it.
    pub branch: Option<String>,
    pub title: Option<String>,
    pub activity: Vec<Activity>,
}

/// Builds sessions from a stream of lines, starting a new segment whenever the
/// agent's directory changes. A session that moves from one worktree to another
/// bills each stretch to the repository it was actually in.
struct Segments {
    agent: &'static str,
    id: String,
    title: Option<String>,
    done: Vec<Session>,
    current: Option<Session>,
}

impl Segments {
    fn new(agent: &'static str, id: String) -> Self {
        Self { agent, id, title: None, done: Vec::new(), current: None }
    }

    fn directory(&mut self, directory: &str) {
        if directory.is_empty() || self.current.as_ref().is_some_and(|s| s.directory == directory) {
            return;
        }
        self.done.extend(self.current.take().filter(|s| !s.activity.is_empty()));
        self.current = Some(Session {
            agent: self.agent,
            id: self.id.clone(),
            directory: directory.to_string(),
            branch: None,
            title: None,
            activity: Vec::new(),
        });
    }

    fn branch(&mut self, branch: &str) {
        if let Some(session) = &mut self.current {
            if session.branch.is_none() && !branch.is_empty() {
                session.branch = Some(branch.to_string());
            }
        }
    }

    /// Activity before any directory is known has nowhere to be billed.
    fn push(&mut self, time: i64, kind: Kind) {
        if let Some(session) = &mut self.current {
            session.activity.push(Activity { time, kind });
        }
    }

    fn finish(mut self) -> Vec<Session> {
        self.done.extend(self.current.take().filter(|s| !s.activity.is_empty()));
        for session in &mut self.done {
            session.title = self.title.clone();
        }
        self.done
    }
}

/// Every prompt you typed into Claude Code, from the history file it keeps
/// outside the transcripts. It reaches back further than the transcripts,
/// which Claude Code cleans up after 30 days, so attention can be rebuilt for
/// months the transcripts no longer cover.
pub fn typed_prompts(since: i64) -> Vec<(i64, String)> {
    let path = crate::paths::home().join(".claude/history.jsonl");
    let mut prompts: Vec<(i64, String)> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| {
            let time = entry.get("timestamp")?.as_i64()? / 1000;
            let directory = entry.get("project")?.as_str()?.to_string();
            (time >= since).then_some((time, directory))
        })
        .collect();
    prompts.sort_by_key(|(time, _)| *time);
    prompts
}

pub fn read_all(since: i64) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let home = crate::paths::home();
    sessions.extend(claude(&home.join(".claude/projects"))?);
    sessions.extend(codex(&home.join(".codex/sessions"))?);
    sessions.extend(opencode(&home.join(".local/share/opencode/opencode.db"))?);
    for session in &mut sessions {
        session.activity.retain(|activity| activity.time >= since);
        session.activity.sort_by_key(|activity| activity.time);
    }
    sessions.retain(|session| !session.activity.is_empty());
    Ok(sessions)
}

fn timestamp(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Subagent transcripts live in their own folders; their work
                // already sits inside the parent session's turn.
                if path.file_name().is_some_and(|name| name == "subagents") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

fn lines(path: &Path) -> impl Iterator<Item = Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<_>>()
        .into_iter()
}

fn clip(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// Claude Code: `~/.claude/projects/<dir>/<session>.jsonl`.
fn claude(root: &Path) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    for path in jsonl_files(root) {
        let id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut segments = Segments::new("claude", id);
        let mut ai_title = None;
        let mut first_prompt = None;
        for line in lines(&path) {
            if line.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            match line.get("type").and_then(Value::as_str) {
                Some("ai-title") => {
                    ai_title = line.get("aiTitle").and_then(Value::as_str).map(str::to_string);
                }
                Some(kind @ ("user" | "assistant")) => {
                    let Some(time) = line.get("timestamp").and_then(timestamp) else {
                        continue;
                    };
                    if let Some(cwd) = line.get("cwd").and_then(Value::as_str) {
                        segments.directory(cwd);
                    }
                    if let Some(branch) = line.get("gitBranch").and_then(Value::as_str) {
                        segments.branch(branch);
                    }
                    let activity = if kind == "user" && claude_prompt(&line) {
                        if first_prompt.is_none() {
                            first_prompt = claude_text(&line).map(|text| clip(&text));
                        }
                        Kind::Prompt
                    } else {
                        // Replies, tool results, task notifications, injected
                        // context: all the agent's side of the conversation.
                        Kind::Agent
                    };
                    segments.push(time, activity);
                }
                _ => {}
            }
        }
        segments.title = ai_title.or(first_prompt);
        sessions.extend(segments.finish());
    }
    Ok(sessions)
}

/// A user line you actually sent. Newer transcripts say so in `origin`; older
/// ones have no origin, so fall back to plain text that is not a tool result,
/// a meta line, or a harness tag like `<command-name>`. Headless SDK runs are
/// never you.
fn claude_prompt(line: &Value) -> bool {
    if line.get("promptSource").and_then(Value::as_str) == Some("sdk") {
        return false;
    }
    if let Some(origin) = line.get("origin") {
        return origin.get("kind").and_then(Value::as_str) == Some("human");
    }
    if line.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    claude_text(line).is_some_and(|text| !text.trim_start().starts_with('<'))
}

fn claude_text(line: &Value) -> Option<String> {
    let content = line.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    if blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return None;
    }
    blocks
        .iter()
        .find_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
fn codex(root: &Path) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    for path in jsonl_files(root) {
        let id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut segments = Segments::new("codex", id);
        for line in lines(&path) {
            let Some(time) = line.get("timestamp").and_then(timestamp) else {
                continue;
            };
            let payload = line.get("payload").cloned().unwrap_or(Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str);
            match (line.get("type").and_then(Value::as_str), payload_type) {
                (Some("session_meta" | "turn_context"), _) => {
                    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                        segments.directory(cwd);
                    }
                }
                (Some("event_msg"), Some("user_message")) => {
                    if segments.title.is_none() {
                        segments.title = payload.get("message").and_then(Value::as_str).map(clip);
                    }
                    segments.push(time, Kind::Prompt);
                }
                // Instructions and echoed user input, not the agent working.
                (Some("response_item"), Some("message"))
                    if payload.get("role").and_then(Value::as_str) != Some("assistant") => {}
                (Some("response_item"), _)
                | (Some("event_msg"), Some("agent_message" | "agent_reasoning" | "task_complete")) => {
                    segments.push(time, Kind::Agent);
                }
                _ => {}
            }
        }
        sessions.extend(segments.finish());
    }
    Ok(sessions)
}

/// OpenCode keeps sessions in SQLite. Read it through the system `sqlite3`
/// rather than linking a second SQLite into the plugin.
fn opencode(database: &Path) -> Result<Vec<Session>> {
    if !database.exists() {
        return Ok(Vec::new());
    }
    let output = std::process::Command::new("sqlite3")
        .arg("-json")
        .arg("-readonly")
        .arg(database)
        .arg(
            "SELECT s.id AS session, s.directory, s.title, m.data \
             FROM message m JOIN session s ON s.id = m.session_id \
             WHERE s.parent_id IS NULL ORDER BY m.time_created",
        )
        .output()
        .context("run sqlite3 on the OpenCode database")?;
    if !output.status.success() {
        anyhow::bail!(
            "sqlite3 could not read the OpenCode database: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let rows: Vec<Value> = if output.stdout.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(&output.stdout).context("parse OpenCode rows")?
    };

    let mut sessions: Vec<Session> = Vec::new();
    for row in rows {
        let Some(id) = row.get("session").and_then(Value::as_str) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(row.get("data").and_then(Value::as_str).unwrap_or("")) else {
            continue;
        };
        if sessions.last().map(|session| session.id.as_str()) != Some(id) {
            sessions.push(Session {
                branch: None,
                agent: "opencode",
                id: id.to_string(),
                directory: row.get("directory").and_then(Value::as_str).unwrap_or_default().to_string(),
                title: row.get("title").and_then(Value::as_str).map(str::to_string),
                activity: Vec::new(),
            });
        }
        let session = sessions.last_mut().expect("just pushed");
        let times = message.get("time");
        let millis = |key: &str| times.and_then(|t| t.get(key)).and_then(Value::as_i64).map(|ms| ms / 1000);
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(time) = millis("created") {
                    session.activity.push(Activity { time, kind: Kind::Prompt });
                }
            }
            Some("assistant") => {
                for time in [millis("created"), millis("completed")].into_iter().flatten() {
                    session.activity.push(Activity { time, kind: Kind::Agent });
                }
            }
            _ => {}
        }
    }
    Ok(sessions)
}
