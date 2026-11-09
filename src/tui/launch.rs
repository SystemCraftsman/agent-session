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
    report_missing_session: bool,
}

impl NewSessionLaunch {
    pub(super) fn new(agent: Agent, profile: String, cwd: String, home: PathBuf) -> Self {
        let mut launch = Self {
            agent,
            profile,
            cwd,
            home,
            existing_ids: HashSet::new(),
            report_missing_session: true,
        };
        launch.existing_ids = launch.locations().into_iter().map(|(id, _)| id).collect();
        launch
    }

    pub(super) fn from_codex_command(command: &str, home: PathBuf) -> Option<Self> {
        let (directory, arguments) = command.rsplit_once(" && codex ")?;
        let cwd = directory
            .strip_prefix("cd '")?
            .strip_suffix('\'')?
            .replace("'\\''", "'");
        let mut arguments = arguments.split_whitespace();
        let profile = match arguments.next()? {
            "--profile" | "-p" => arguments.next()?,
            _ => return None,
        };
        if !crate::router::codex_config_profiles(&home)
            .iter()
            .any(|known| known == profile)
        {
            return None;
        }
        let arguments: Vec<_> = arguments.collect();
        if !arguments
            .iter()
            .any(|argument| matches!(*argument, "resume" | "fork"))
        {
            return None;
        }

        let mut launch = Self::new(Agent::Codex, profile.into(), cwd, home);
        // A shell wrapper can turn a cross-provider resume into a native fork.
        // Ordinary resumes create no new session and must not report an error.
        launch.report_missing_session = arguments.contains(&"fork");
        Some(launch)
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
                Ok(false) if status.success() && launch.report_missing_session => {
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

    #[test]
    fn provider_switch_fork_keeps_the_selected_codex_profile() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("work.config.toml"), "").unwrap();
        fs::write(home.path().join("work-enmaas.config.toml"), "").unwrap();
        write_session(home.path(), Agent::Codex, "original", "/tmp/project");
        let command = "cd '/tmp/project' && codex --profile work-enmaas -c 'tui.terminal_title=[]' resume original";
        let launch = NewSessionLaunch::from_codex_command(command, home.path().into()).unwrap();
        assert!(!launch.report_missing_session);
        assert!(!launch.try_save_profile().unwrap());

        write_session(home.path(), Agent::Codex, "enmaas-fork", "/tmp/project");
        assert!(launch.try_save_profile().unwrap());
        let profiles = crate::router::load_session_profiles(home.path());
        assert_eq!(
            profiles.get("enmaas-fork").map(String::as_str),
            Some("work-enmaas")
        );
        assert!(!profiles.contains_key("original"));
        let session = crate::codex::discover_codex_sessions(home.path())
            .into_iter()
            .find(|session| session.id == "enmaas-fork")
            .unwrap();
        let resume = session.resume_command_with_profiles(
            "_claude_run_work",
            profiles.get(&session.id).map(String::as_str),
        );
        assert!(resume.contains("codex --profile work-enmaas"));
        assert!(resume.ends_with("resume enmaas-fork"));
    }

    #[test]
    fn normal_codex_resume_does_not_require_a_new_session() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("work-enmaas.config.toml"), "").unwrap();
        let launch = NewSessionLaunch::from_codex_command(
            "cd '/tmp/project' && codex --profile work-enmaas resume existing",
            home.path().into(),
        )
        .unwrap();
        assert!(!launch.report_missing_session);
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        assert!(wait_for_agent(&mut child, Some(&launch)).unwrap().success());
        assert!(crate::router::load_session_profiles(home.path()).is_empty());
    }

    #[test]
    fn codex_command_tracking_decodes_cwd_and_distinguishes_explicit_forks() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("work-enmaas.config.toml"), "").unwrap();
        let launch = NewSessionLaunch::from_codex_command(
            "cd '/tmp/owner'\\''s project' && codex --profile work-enmaas fork original",
            home.path().into(),
        )
        .unwrap();
        assert_eq!(launch.cwd, "/tmp/owner's project");
        assert!(launch.report_missing_session);
        for command in [
            "cd '/tmp/project' && claude -r original",
            "cd '/tmp/project' && codex resume original",
            "cd '/tmp/project' && codex --profile unknown resume original",
        ] {
            assert!(NewSessionLaunch::from_codex_command(command, home.path().into()).is_none());
        }
    }

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
