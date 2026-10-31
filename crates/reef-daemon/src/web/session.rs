//! Web UI sessions (R2.18, ADR 0047): a 256-bit bearer token per `reef ui`,
//! kept only as its blake3 digest, alive until the daemon stops or until
//! it goes unused for [`Limits::session_idle`] with no stream open.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt as _;

use super::Limits;

/// A session's key: blake3 of its token.
type Key = [u8; 32];

/// One session.
#[derive(Debug)]
struct Session {
    last_used: Instant,
    /// Requests in flight, streams included; a session with one is never
    /// idle.
    open: usize,
    /// The launch file still on disk, deleted at the first request.
    launch_file: Option<PathBuf>,
}

/// The live sessions.
#[derive(Debug)]
pub(crate) struct Sessions {
    sessions: Mutex<HashMap<Key, Session>>,
    limits: Limits,
}

/// What a request holds while it runs: dropping it (with the response
/// body) ends the request's use of its session.
#[derive(Debug)]
pub(crate) struct InUse {
    sessions: Arc<Sessions>,
    key: Key,
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock();
        if let Some(session) = sessions.get_mut(&self.key) {
            session.open = session.open.saturating_sub(1);
            session.last_used = Instant::now();
        }
    }
}

fn key(token: &str) -> Key {
    *blake3::hash(token.as_bytes()).as_bytes()
}

impl Sessions {
    pub(crate) fn new(limits: Limits) -> Sessions {
        Sessions {
            sessions: Mutex::new(HashMap::new()),
            limits,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new session's token, 32 bytes from the thread's CSPRNG, base64url
    /// without padding. At [`Limits::max_sessions`], the least recently
    /// used session with no stream open ends first: an open stream is in
    /// use, however long ago it started. Only when every session has one
    /// does the least recently used of them end. Returns the launch files
    /// of the sessions it ended, for the caller to delete.
    pub(crate) fn mint(&self) -> (String, Vec<PathBuf>) {
        let mut bytes = [0_u8; 32];
        rand::rng().fill(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let mut sessions = self.lock();
        let mut ended = Vec::new();
        let idle = self.limits.session_idle;
        sessions.retain(|_, session| {
            let keep = session.open > 0 || session.last_used.elapsed() < idle;
            if !keep {
                ended.extend(session.launch_file.take());
            }
            keep
        });
        while sessions.len() >= self.limits.max_sessions.max(1) {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| (session.open > 0, session.last_used))
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(mut session) = sessions.remove(&oldest) {
                ended.extend(session.launch_file.take());
            }
        }
        sessions.insert(
            key(&token),
            Session {
                last_used: Instant::now(),
                open: 0,
                launch_file: None,
            },
        );
        (token, ended)
    }

    /// The session `token` names, if it is live: marks it in use until the
    /// returned guard drops, and hands back its launch file if this is its
    /// first request. An idle session found here ends.
    pub(crate) fn use_token(self: &Arc<Self>, token: &str) -> Option<(InUse, Option<PathBuf>)> {
        let key = key(token);
        let mut sessions = self.lock();
        let session = sessions.get_mut(&key)?;
        if session.open == 0 && session.last_used.elapsed() >= self.limits.session_idle {
            let launch = sessions.remove(&key).and_then(|mut s| s.launch_file.take());
            drop(sessions);
            if let Some(path) = launch {
                super::launch::remove(&path);
            }
            return None;
        }
        session.open += 1;
        session.last_used = Instant::now();
        let launch = session.launch_file.take();
        Some((
            InUse {
                sessions: Arc::clone(self),
                key,
            },
            launch,
        ))
    }

    /// Records `path` as `token`'s launch file, which its first request
    /// deletes.
    pub(crate) fn attach_launch_file(&self, token: &str, path: PathBuf) {
        if let Some(session) = self.lock().get_mut(&key(token)) {
            session.launch_file = Some(path);
        }
    }

    /// Forgets `path` as a session's launch file, after its time to live
    /// has removed it.
    pub(crate) fn launch_file_gone(&self, path: &std::path::Path) {
        for session in self.lock().values_mut() {
            if session.launch_file.as_deref() == Some(path) {
                session.launch_file = None;
            }
        }
    }

    /// Live sessions, idle ones included until a mint or a use ends them.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn sessions(max: usize, idle: Duration) -> Arc<Sessions> {
        Arc::new(Sessions::new(Limits {
            max_sessions: max,
            session_idle: idle,
            ..Limits::default()
        }))
    }

    #[test]
    fn tokens_are_43_base64url_characters_and_distinct() {
        let sessions = sessions(4, Duration::from_hours(1));
        let (a, _) = sessions.mint();
        let (b, _) = sessions.mint();
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a, b);
        assert!(sessions.use_token(&a).is_some());
        assert!(sessions.use_token("x").is_none());
    }

    #[test]
    fn the_least_recently_used_session_ends_at_the_cap() {
        let sessions = sessions(2, Duration::from_hours(1));
        let (a, _) = sessions.mint();
        std::thread::sleep(Duration::from_millis(2));
        let (b, _) = sessions.mint();
        std::thread::sleep(Duration::from_millis(2));
        drop(sessions.use_token(&a)); // a is now the more recent
        let (c, _) = sessions.mint();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.use_token(&b).is_none());
        assert!(sessions.use_token(&a).is_some());
        assert!(sessions.use_token(&c).is_some());
    }

    #[test]
    fn a_session_with_a_stream_open_is_not_evicted() {
        let sessions = sessions(2, Duration::from_hours(1));
        let (streaming, _) = sessions.mint();
        // A stream opened first and still open: the oldest `last_used`.
        let held = sessions.use_token(&streaming).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let (other, _) = sessions.mint();
        std::thread::sleep(Duration::from_millis(2));
        let (newest, _) = sessions.mint();
        assert!(sessions.use_token(&other).is_none());
        assert!(sessions.use_token(&streaming).is_some());
        assert!(sessions.use_token(&newest).is_some());
        drop(held);
    }

    #[test]
    fn idle_sessions_end_unless_in_use() {
        let sessions = sessions(4, Duration::from_millis(30));
        let (idle, _) = sessions.mint();
        let (busy, _) = sessions.mint();
        let held = sessions.use_token(&busy).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert!(sessions.use_token(&idle).is_none());
        // In use for longer than the idle limit: still live, and live
        // right after it ends.
        assert!(sessions.use_token(&busy).is_some());
        drop(held);
        assert!(sessions.use_token(&busy).is_some());
    }

    #[test]
    fn the_launch_file_is_handed_back_once() {
        let sessions = sessions(4, Duration::from_hours(1));
        let (token, _) = sessions.mint();
        sessions.attach_launch_file(&token, PathBuf::from("/nonexistent/open-x.html"));
        let (_first, launch) = sessions.use_token(&token).unwrap();
        assert_eq!(launch, Some(PathBuf::from("/nonexistent/open-x.html")));
        let (_second, launch) = sessions.use_token(&token).unwrap();
        assert_eq!(launch, None);
    }
}
