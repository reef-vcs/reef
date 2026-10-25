//! The environment `reef ws run` gives its command (README, CLI): which
//! workspace it is in and how to reach the daemon that holds it.

use std::path::Path;

use reef_api::proto::v1::Workspace;

/// `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`, `REEF_REPO`, and `REEF_SOCKET`
/// for a command running in `ws`, whose daemon listens on `socket`.
pub(crate) fn reef_env(ws: &Workspace, socket: &Path) -> [(&'static str, String); 4] {
    [
        ("REEF_WORKSPACE", ws.id.clone()),
        ("REEF_WORKSPACE_PATH", ws.path.clone()),
        ("REEF_REPO", ws.repo_id.clone()),
        ("REEF_SOCKET", socket.display().to_string()),
    ]
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
        let env = reef_env(&ws, Path::new("/state/reef.sock"));
        assert_eq!(
            env,
            [
                ("REEF_WORKSPACE", "0123456789abcdef".to_owned()),
                (
                    "REEF_WORKSPACE_PATH",
                    "/state/ws/0123456789abcdef".to_owned()
                ),
                ("REEF_REPO", "fedcba9876543210".to_owned()),
                ("REEF_SOCKET", "/state/reef.sock".to_owned()),
            ]
        );
    }
}
