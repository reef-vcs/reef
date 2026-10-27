//! The environment `reef ws run` gives its command (README, CLI): which
//! workspace it is in, how to reach the daemon that holds it, and the agent
//! the command's own `reef` calls name (R2.05).

use reef_api::v1::Workspace;
use std::path::Path;

use super::AGENT_ENV;

/// `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`, `REEF_REPO`, and `REEF_SOCKET`
/// for a command running in `ws`, whose daemon listens on `socket`, and
/// `REEF_AGENT` when `agent` names one.
pub(crate) fn reef_env(
    ws: &Workspace,
    socket: &Path,
    agent: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("REEF_WORKSPACE", ws.id.clone()),
        ("REEF_WORKSPACE_PATH", ws.path.clone()),
        ("REEF_REPO", ws.repo_id.clone()),
        ("REEF_SOCKET", socket.display().to_string()),
    ];
    if let Some(agent) = agent {
        env.push((AGENT_ENV, agent.to_owned()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_workspace_and_the_socket() {
        let ws = Workspace {
            id: "0123456789abcdef".into(),
            repo_id: "fedcba9876543210".into(),
            path: "/state/ws/0123456789abcdef".into(),
            ..Workspace::default()
        };
        let env = reef_env(&ws, Path::new("/state/reef.sock"), None);
        let expected = [
            ("REEF_WORKSPACE", "0123456789abcdef".to_owned()),
            (
                "REEF_WORKSPACE_PATH",
                "/state/ws/0123456789abcdef".to_owned(),
            ),
            ("REEF_REPO", "fedcba9876543210".to_owned()),
            ("REEF_SOCKET", "/state/reef.sock".to_owned()),
        ];
        assert_eq!(env, expected);
        let env = reef_env(&ws, Path::new("/state/reef.sock"), Some("bot-1"));
        assert_eq!(env[..4], expected);
        assert_eq!(env[4], ("REEF_AGENT", "bot-1".to_owned()));
    }
}
