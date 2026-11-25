//! The project map that decides where a pane's time is filed.
//!
//! `~/.config/chronoid/projects.json` names every Chronoid project time can go
//! to. You keep it, by hand or with your own sync from an issue tracker. This
//! plugin only reads it and never invents a project of its own.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub name: String,
    /// Where this project's attention goes. Agent runtime is not in Chronoid:
    /// it overlaps, and Chronoid refuses overlapping entries.
    pub chronoid_id: i64,
}

/// A repository's default, for the period it applied. A repository that served
/// one project throughout carries a single entry with no dates.
#[derive(Debug, Clone, Deserialize)]
struct RepoDefault {
    /// A project key, or `category:<key>`.
    target: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RepoDefaults {
    One(String),
    Dated(Vec<RepoDefault>),
}

#[derive(Debug, Deserialize)]
pub struct Map {
    projects: BTreeMap<String, Project>,
    /// Chronoid's category roots, for repositories whose default is a category
    /// rather than one project.
    categories: BTreeMap<String, i64>,
    /// A ticket like `ABC-12` to the project key that owns it. The prefixes
    /// these keys use are the ones looked for in branches and paths.
    issues: BTreeMap<String, String>,
    /// Repository directory name to a project key, for repositories that
    /// only ever serve one project.
    repos: BTreeMap<String, RepoDefaults>,
    personal_paths: Vec<String>,
    /// Resolving a directory runs git twice. The backfill asks about the same
    /// few hundred directories tens of thousands of times.
    #[serde(skip)]
    resolved: RefCell<HashMap<String, Attribution>>,
}

/// Where a directory's time belongs.
#[derive(Clone, Debug)]
pub enum Attribution {
    /// Personal: never recorded, by either clock.
    Personal,
    Project { linear_id: String, project: Project },
    /// Business time with no project. Attention is dropped, since Chronoid
    /// keeps the tracked activity and the weekly review triages it. Agent
    /// runtime still counts: those hours came out of a paid subscription.
    Unmapped,
}

pub fn path() -> PathBuf {
    crate::paths::home().join(".config/chronoid/projects.json")
}

impl Map {
    pub fn load() -> Result<Self> {
        let text = std::fs::read_to_string(path()).with_context(|| {
            format!("read {}: create the project map first (see the README)", path().display())
        })?;
        serde_json::from_str(&text).context("parse the project map")
    }

    /// The project a block's target names: a project key, or a category
    /// root for work that has a category but no project of its own.
    pub fn project(&self, target: &str) -> Option<Project> {
        if let Some(key) = target.strip_prefix("category:") {
            return self.category_project(key);
        }
        self.projects.get(target).cloned()
    }

    fn category_project(&self, key: &str) -> Option<Project> {
        Some(Project { name: key.to_string(), chronoid_id: *self.categories.get(key)? })
    }

    /// The project a directory's time belongs to: the ticket named by its
    /// branch or its path, else the repository's default project.
    pub fn attribute(&self, directory: &str, at: i64) -> Attribution {
        self.attribute_on(directory, None, at)
    }

    /// `branch` is the branch the work was on, for history that records it.
    /// Without one the directory's current branch is used, which is right for
    /// live panes and wrong for a worktree that has since moved on or gone.
    pub fn attribute_on(&self, directory: &str, branch: Option<&str>, at: i64) -> Attribution {
        let key = format!("{directory}\n{}\n{}", branch.unwrap_or_default(), crate::clock::local_date(at));
        if let Some(attribution) = self.resolved.borrow().get(&key) {
            return attribution.clone();
        }
        let attribution = self.resolve(directory, branch, at);
        self.resolved.borrow_mut().insert(key, attribution.clone());
        attribution
    }

    fn resolve(&self, directory: &str, branch: Option<&str>, at: i64) -> Attribution {
        let directory = Path::new(directory);
        if self.is_personal(directory) {
            return Attribution::Personal;
        }
        let branch = match branch {
            Some(branch) => branch.to_string(),
            None => git(directory, &["branch", "--show-current"]),
        };
        let prefixes: BTreeSet<&str> =
            self.issues.keys().filter_map(|key| Some(key.split_once('-')?.0)).collect();
        let target = ticket(&prefixes, &branch)
            .or_else(|| ticket(&prefixes, &directory.to_string_lossy()))
            .and_then(|ticket| self.issues.get(&ticket).cloned())
            .or_else(|| repository(directory).and_then(|repo| self.repo_default(&repo, at)));
        let Some(target) = target else {
            return Attribution::Unmapped;
        };
        if target.starts_with("category:") {
            return match self.project(&target) {
                Some(project) => Attribution::Project { linear_id: target, project },
                None => Attribution::Unmapped,
            };
        }
        match self.projects.get_key_value(&target) {
            Some((linear_id, project)) => Attribution::Project {
                linear_id: linear_id.clone(),
                project: project.clone(),
            },
            None => Attribution::Unmapped,
        }
    }

    /// The default that was in force on the given day.
    fn repo_default(&self, repo: &str, at: i64) -> Option<String> {
        match self.repos.get(repo)? {
            RepoDefaults::One(target) => Some(target.clone()),
            RepoDefaults::Dated(entries) => {
                let day = crate::clock::local_date(at);
                entries
                    .iter()
                    .find(|entry| {
                        entry.from.as_ref().is_none_or(|from| day >= *from)
                            && entry.until.as_ref().is_none_or(|until| day < *until)
                    })
                    .map(|entry| entry.target.clone())
            }
        }
    }

    fn is_personal(&self, directory: &Path) -> bool {
        self.personal_paths.iter().any(|personal| {
            let personal = match personal.strip_prefix("~/") {
                Some(rest) => crate::paths::home().join(rest),
                None => PathBuf::from(personal),
            };
            directory.starts_with(personal)
        })
    }
}

/// `ABC-12` from a branch like `feature/abc-12-login` or from a worktree path
/// that names the ticket, for any of the given prefixes.
fn ticket(prefixes: &BTreeSet<&str>, text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    prefixes.iter().find_map(|prefix| {
        let needle = prefix.to_lowercase();
        let mut rest = lower.as_str();
        while let Some(at) = rest.find(&needle) {
            let after = &rest[at + needle.len()..];
            let digits: String = after
                .trim_start_matches(['-', '_', ' '])
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                return Some(format!("{prefix}-{digits}"));
            }
            rest = after;
        }
        None
    })
}

/// The repository a directory belongs to, with worktrees collapsed onto the
/// repository they came from. History often names a worktree that has since
/// been removed, so a directory that is gone is looked up through the nearest
/// one that is still there.
fn repository(directory: &Path) -> Option<String> {
    let existing = directory.ancestors().find(|dir| dir.is_dir())?;
    let common = git(existing, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    if common.is_empty() {
        return None;
    }
    let path = Path::new(&common);
    let root = if path.file_name().is_some_and(|name| name == ".git") {
        path.parent()?
    } else {
        path
    };
    root.file_name().map(|name| name.to_string_lossy().into_owned())
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(directory).args(args).output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::ticket;
    use std::collections::BTreeSet;

    #[test]
    fn finds_the_ticket_in_a_branch_or_a_path() {
        let prefixes = BTreeSet::from(["ABC", "OPS"]);
        assert_eq!(ticket(&prefixes, "feature/abc-12-login").as_deref(), Some("ABC-12"));
        assert_eq!(ticket(&prefixes, "/home/me/.herdr/worktrees/widgets/OPS-345").as_deref(), Some("OPS-345"));
        assert_eq!(ticket(&prefixes, "main").as_deref(), None);
        assert_eq!(ticket(&prefixes, "abc-widgets").as_deref(), None);
        assert_eq!(ticket(&prefixes, "xyz-9").as_deref(), None);
    }
}
