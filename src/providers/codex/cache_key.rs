//! Chooses the `prompt_cache_key` sent on Codex Responses requests.
//!
//! Applied once to the translated request, so HTTP, WebSocket and compaction
//! requests (all cloned from it) share one key.

use sha2::{Digest, Sha256};

use crate::config::PromptCacheKeyMode;
use crate::request_identity::ConversationIdentity;

const THREAD_KEY_DOMAIN: &str = "ccp-codex-thread-v1";

/// Log label for the request's identity; never the ids themselves.
pub(crate) fn identity_kind(identity: Option<&ConversationIdentity>) -> &'static str {
    match identity {
        Some(ConversationIdentity::Main(_)) => "main",
        Some(ConversationIdentity::Agent(..)) => "agent",
        None => "none",
    }
}

/// `session` keeps the session id. `thread` keeps it for the main thread and
/// for requests without valid identity headers, and gives a subagent a SHA-256
/// derived key (fixed length, no raw ids). `omit` sends no key.
pub(crate) fn resolve(
    mode: PromptCacheKeyMode,
    session_id: Option<&str>,
    identity: Option<&ConversationIdentity>,
) -> Option<String> {
    match (mode, identity) {
        (PromptCacheKeyMode::Omit, _) => None,
        (PromptCacheKeyMode::Thread, Some(ConversationIdentity::Agent(session, agent))) => {
            Some(thread_key(session, agent))
        }
        _ => session_id.map(str::to_owned),
    }
}

fn thread_key(session: &str, agent: &str) -> String {
    // Header values are ASCII-graphic, so NUL cannot occur inside an id.
    let mut hasher = Sha256::new();
    for part in [THREAD_KEY_DOMAIN, session, agent] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("ccp-thread-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_identity::{CLAUDE_AGENT_HEADER, CLAUDE_SESSION_HEADER};
    use http::{HeaderMap, HeaderValue};

    fn identity(session: &str, agent: Option<&str>) -> Option<ConversationIdentity> {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_SESSION_HEADER,
            HeaderValue::from_str(session).unwrap(),
        );
        if let Some(agent) = agent {
            headers.insert(CLAUDE_AGENT_HEADER, HeaderValue::from_str(agent).unwrap());
        }
        ConversationIdentity::from_headers(&headers)
    }

    fn key(mode: PromptCacheKeyMode, session: &str, agent: Option<&str>) -> Option<String> {
        resolve(mode, Some(session), identity(session, agent).as_ref())
    }

    #[test]
    fn mode_parsing_accepts_known_values_and_rejects_others() {
        use crate::config::parse_prompt_cache_key_mode as parse;
        assert_eq!(parse("session").unwrap(), PromptCacheKeyMode::Session);
        assert_eq!(parse("thread").unwrap(), PromptCacheKeyMode::Thread);
        assert_eq!(parse("omit").unwrap(), PromptCacheKeyMode::Omit);
        for bad in ["Thread", "agent", "true", " "] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn session_and_omit_modes_keep_or_drop_the_session_key() {
        for agent in [None, Some("agent-a")] {
            assert_eq!(
                key(PromptCacheKeyMode::Session, "session-1", agent).as_deref(),
                Some("session-1")
            );
            assert_eq!(key(PromptCacheKeyMode::Omit, "session-1", agent), None);
        }
    }

    #[test]
    fn thread_mode_separates_agents_and_keeps_main_and_fallbacks_on_session() {
        let thread = PromptCacheKeyMode::Thread;
        let agent_a = key(thread, "session-1", Some("agent-a")).unwrap();
        let agent_b = key(thread, "session-1", Some("agent-b")).unwrap();
        assert_ne!(agent_a, agent_b);
        assert_ne!(agent_a, "session-1");
        assert!(!agent_a.contains("session-1") && !agent_a.contains("agent-a"));
        // Successive turns of one thread are stable; sessions are isolated.
        assert_eq!(
            Some(&agent_a),
            key(thread, "session-1", Some("agent-a")).as_ref()
        );
        assert_ne!(
            Some(&agent_a),
            key(thread, "session-2", Some("agent-a")).as_ref()
        );
        // Main thread keeps the session id.
        assert_eq!(key(thread, "session-1", None).as_deref(), Some("session-1"));
        // Absent or malformed identity falls back to the session behavior.
        assert_eq!(
            resolve(thread, Some("session-1"), None).as_deref(),
            Some("session-1")
        );
        let mut malformed = HeaderMap::new();
        malformed.append(CLAUDE_SESSION_HEADER, HeaderValue::from_static("session-1"));
        malformed.append(CLAUDE_SESSION_HEADER, HeaderValue::from_static("session-2"));
        malformed.insert(CLAUDE_AGENT_HEADER, HeaderValue::from_static("agent-a"));
        let parsed = ConversationIdentity::from_headers(&malformed);
        assert_eq!(identity_kind(parsed.as_ref()), "none");
        assert_eq!(
            resolve(thread, Some("session-1"), parsed.as_ref()).as_deref(),
            Some("session-1")
        );
    }
}
