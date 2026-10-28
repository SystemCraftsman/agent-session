use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::Duration;

use crate::session::Agent;

pub(super) struct NewSessionLaunch {
    agent: Agent,
    profile: String,
    cwd: String,
    home: PathBuf,
    existing_ids: HashSet<String>,
}

impl NewSessionLaunch {
    pub(super) fn new(agent: Agent, profile: String, cwd: String, home: PathBuf) -> Self {
        let mut launch = Self {
            agent,
            profile,
            cwd,
            home,
            existing_ids: HashSet::new(),
        };
        launch.existing_ids = launch.locations().into_iter().map(|(id, _)| id).collect();
        launch
    }

    fn locations(&self) -> Vec<(String, String)> {
        match self.agent {
            Agent::Codex => crate::codex::discover_codex_session_locations(&self.home),
            Agent::Claude => {
                let project = self.cwd.replace(['/', '.'], "-");
                let Ok(entries) = fs::read_dir(self.home.join("projects").join(project)) else {
                    return Vec::new();
                };
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let path = entry.path();
                        if !path.is_file() || path.extension()?.to_str()? != "jsonl" {
                            return None;
                        }
                        Some((path.file_stem()?.to_str()?.to_string(), self.cwd.clone()))
                    })
                    .collect()
            }
            Agent::Cursor => Vec::new(),
        }
    }

    fn try_save_profile(&self) -> Result<bool, String> {
        let candidates: HashSet<String> = self
            .locations()
            .into_iter()
            .filter(|(id, cwd)| cwd == &self.cwd && !self.existing_ids.contains(id))
            .map(|(id, _)| id)
            .collect();
        if candidates.is_empty() {
            return Ok(false);
        }
        if candidates.len() != 1 {
            return Err(
                "multiple new sessions appeared in this project; profile was not assigned"
                    .to_string(),
            );
        }
        let id = candidates.iter().next().unwrap();
        // Another browser may already have explicitly tagged this session.
        if crate::router::load_session_profiles(&self.home).contains_key(id) {
            return Ok(true);
        }
        match self.agent {
            Agent::Claude => crate::router::save_session_profile(&self.home, id, &self.profile),
            Agent::Codex => {
                let known = crate::router::codex_config_profiles(&self.home);
                crate::router::save_codex_session_profile(&self.home, id, &self.profile, &known)
            }
            Agent::Cursor => return Ok(true),
        }
        .map_err(|error| error.to_string())?;
        Ok(true)
    }
}

/// Persist the launch profile while the child owns the terminal. Once saved,
/// wait normally so a later profile change cannot be overwritten on exit.
pub(super) fn wait_for_agent(
    child: &mut Child,
    launch: Option<&NewSessionLaunch>,
) -> io::Result<ExitStatus> {
    let Some(launch) = launch else {
        return child.wait();
    };
    loop {
        let status = child.try_wait()?;
        let saved = launch.try_save_profile();
        if matches!(saved, Ok(true)) {
            return match status {
                Some(status) => Ok(status),
                None => child.wait(),
            };
        }
        if let Some(status) = status {
            match saved {
                Err(error) => eprintln!("Failed to remember new session profile: {error}"),
                Ok(false) if status.success() => {
                    eprintln!("Failed to remember new session profile: new session metadata was not found");
                }
                _ => {}
            }
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Instant;

    fn write_session(home: &Path, agent: Agent, id: &str, cwd: &str) {
        let (path, body) = match agent {
            Agent::Codex => (
                home.join("sessions/2026/10/28")
                    .join(format!("rollout-{id}.jsonl")),
                serde_json::json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd}}),
            ),
            Agent::Claude => (
                home.join("projects")
                    .join(cwd.replace(['/', '.'], "-"))
                    .join(format!("{id}.jsonl")),
                serde_json::json!({"type": "user", "cwd": cwd}),
            ),
            Agent::Cursor => unreachable!(),
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{body}\n")).unwrap();
    }

    #[test]
    fn profile_is_visible_in_another_browser_before_agent_exits_and_is_not_reapplied() {
        for agent in [Agent::Codex, Agent::Claude] {
            let home = tempfile::tempdir().unwrap();
            fs::write(home.path().join("personal.config.toml"), "").unwrap();
            fs::write(home.path().join("work.config.toml"), "").unwrap();
            write_session(home.path(), agent, "existing", "/tmp/project");
            let launch = NewSessionLaunch::new(
                agent,
                "personal".into(),
                "/tmp/project".into(),
                home.path().to_path_buf(),
            );
            let mut child = Command::new("sh")
                .args(["-c", "read line; exit 0"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let (finished_tx, finished_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = wait_for_agent(&mut child, Some(&launch));
                let _ = finished_tx.send(result);
            });
            write_session(home.path(), agent, "unrelated", "/tmp/other-project");
            write_session(home.path(), agent, "new-personal", "/tmp/project");
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let profiles = crate::router::load_session_profiles(home.path());
                if profiles.get("new-personal").map(String::as_str) == Some("personal") {
                    assert!(!profiles.contains_key("existing"));
                    assert!(!profiles.contains_key("unrelated"));
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "profile not visible while child is alive"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(matches!(
                finished_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            if agent == Agent::Codex {
                let sessions = crate::codex::discover_codex_sessions(home.path());
                let session = sessions.iter().find(|s| s.id == "new-personal").unwrap();
                let profiles = crate::router::load_session_profiles(home.path());
                let resume = session.resume_command_with_profiles(
                    "claude",
                    profiles.get(&session.id).map(String::as_str),
                );
                assert!(resume.contains("codex --profile personal"));
                crate::router::save_codex_session_profile(
                    home.path(),
                    "new-personal",
                    "work",
                    &["work".into(), "personal".into()],
                )
                .unwrap();
            } else {
                crate::router::save_session_profile(home.path(), "new-personal", "work").unwrap();
            }
            write_session(home.path(), agent, "later-session", "/tmp/project");
            input.write_all(b"done\n").unwrap();
            drop(input);
            assert!(finished_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .success());
            worker.join().unwrap();
            let profiles = crate::router::load_session_profiles(home.path());
            assert_eq!(
                profiles.get("new-personal").map(String::as_str),
                Some("work")
            );
            assert!(!profiles.contains_key("later-session"));
        }
    }

    #[test]
    fn ambiguous_new_sessions_are_not_assigned_a_profile() {
        let home = tempfile::tempdir().unwrap();
        let launch = NewSessionLaunch::new(
            Agent::Codex,
            "personal".into(),
            "/tmp/project".into(),
            home.path().to_path_buf(),
        );
        write_session(home.path(), Agent::Codex, "first", "/tmp/project");
        write_session(home.path(), Agent::Codex, "second", "/tmp/project");
        assert!(launch.try_save_profile().is_err());
        assert!(crate::router::load_session_profiles(home.path()).is_empty());
    }

    #[test]
    fn incomplete_header_is_retried_and_an_explicit_profile_is_preserved() {
        let home = tempfile::tempdir().unwrap();
        let launch = NewSessionLaunch::new(
            Agent::Codex,
            "personal".into(),
            "/tmp/project".into(),
            home.path().to_path_buf(),
        );
        let directory = home.path().join("sessions/2026/10/28");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("rollout-new.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":",
        )
        .unwrap();
        assert!(!launch.try_save_profile().unwrap());
        assert!(crate::router::load_session_profiles(home.path()).is_empty());
        write_session(home.path(), Agent::Codex, "new", "/tmp/project");
        crate::router::save_codex_session_profile(home.path(), "new", "work", &["work".into()])
            .unwrap();
        assert!(launch.try_save_profile().unwrap());
        assert_eq!(
            crate::router::load_session_profiles(home.path())
                .get("new")
                .map(String::as_str),
            Some("work")
        );
    }
}
