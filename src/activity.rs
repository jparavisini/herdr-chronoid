//! When an agent last did anything, read from its own transcript.
//!
//! Herdr can leave a pane `working` for days after the agent stopped, and it
//! sends no event for pane output. The agent's transcript is written on every
//! prompt, tool call and reply, so its modification time is the agent's last
//! activity. Claude Code writes a delegated subagent's turns next to the
//! session, in `<session>/subagents/`, so those count too. A long tool call
//! that writes nothing for fifteen minutes still reads as quiet. Agents whose
//! transcripts are not found here run uncapped.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The agent session Herdr reports on a pane.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: String,
    pub kind: String,
    pub value: String,
}

/// Transcript paths found so far, keyed by session. Sessions keep their file
/// for life, so a hit is cached for good; a miss is searched for again.
pub struct Transcripts {
    claude_projects: PathBuf,
    codex_sessions: PathBuf,
    found: HashMap<AgentSession, PathBuf>,
}

impl Transcripts {
    pub fn new(home: &Path) -> Self {
        Self {
            claude_projects: home.join(".claude/projects"),
            codex_sessions: home.join(".codex/sessions"),
            found: HashMap::new(),
        }
    }

    /// Unix seconds of the session's last transcript write, or None when this
    /// agent's transcript cannot be found.
    pub fn last_activity(&mut self, session: &AgentSession) -> Option<i64> {
        if !self.found.contains_key(session) {
            let path = self.search(session)?;
            self.found.insert(session.clone(), path);
        }
        let transcript = &self.found[session];
        let main = modified(transcript)?;
        // `<project>/<session>.jsonl` keeps its subagents in `<project>/<session>/`.
        Some(main.max(newest_under(&transcript.with_extension(""))))
    }

    fn search(&self, session: &AgentSession) -> Option<PathBuf> {
        if session.kind != "id" || session.value.is_empty() {
            return None;
        }
        match session.agent.as_str() {
            // ~/.claude/projects/<project>/<session id>.jsonl
            "claude" => std::fs::read_dir(&self.claude_projects)
                .ok()?
                .flatten()
                .map(|project| project.path().join(format!("{}.jsonl", session.value)))
                .find(|path| path.is_file()),
            // ~/.codex/sessions/YYYY/MM/DD/rollout-<stamp>-<session id>.jsonl
            "codex" => find_file(&self.codex_sessions, &format!("-{}.jsonl", session.value)),
            _ => None,
        }
    }
}

fn modified(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// The latest modification time of any file under `root`, or 0 when there is
/// none.
fn newest_under(root: &Path) -> i64 {
    let mut newest = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(time) = modified(&path) {
                newest = newest.max(time);
            }
        }
    }
    newest
}

fn find_file(root: &Path, suffix: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().ends_with(suffix) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent: &str, value: &str) -> AgentSession {
        AgentSession { agent: agent.into(), kind: "id".into(), value: value.into() }
    }

    #[test]
    fn reads_the_last_write_of_claude_and_codex_transcripts() {
        let home = std::env::temp_dir().join(format!("herdr-chronoid-activity-{}", std::process::id()));
        let claude = home.join(".claude/projects/-Users-me-repo");
        let codex = home.join(".codex/sessions/2026/11/21");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        for path in [claude.join("abc.jsonl"), codex.join("rollout-2026-11-21T10-00-00-def.jsonl")] {
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(stamp).unwrap();
        }

        let mut transcripts = Transcripts::new(&home);
        assert_eq!(transcripts.last_activity(&session("claude", "abc")), Some(1_790_000_000));
        // A subagent still writing keeps the session active.
        std::fs::create_dir_all(claude.join("abc/subagents")).unwrap();
        let subagent = std::fs::File::create(claude.join("abc/subagents/agent-1.jsonl")).unwrap();
        subagent.set_modified(stamp + std::time::Duration::from_secs(600)).unwrap();
        assert_eq!(transcripts.last_activity(&session("claude", "abc")), Some(1_790_000_600));
        assert_eq!(transcripts.last_activity(&session("codex", "def")), Some(1_790_000_000));
        assert_eq!(transcripts.last_activity(&session("claude", "missing")), None);
        assert_eq!(transcripts.last_activity(&session("opencode", "abc")), None);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
