use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use super::constants::{CLIENT_ID, ISSUER, REFRESH_MARGIN_MS};
use super::jwt::{TokenResponse, extract_account_id, validate_token_response};
use super::token_store::{CodexTokenStore, StoredAuth};
use crate::auth::AuthStorage;

static CODEX_REFRESH_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

/// A read-only manager serves a stored token only while it stays valid for at
/// least this long, so a request does not start with a token about to expire.
const READ_ONLY_MIN_VALIDITY_MS: u64 = 60_000;

const READ_ONLY_EXPIRED_MESSAGE: &str = "codex auth is read-only: stored token is expired or expires within 60s; the listener that owns this store must refresh it";

/// Mode and remaining token lifetime for `/healthz`. Reads the store only; the
/// result never contains a token, account id or path.
pub fn auth_health<S: AuthStorage<StoredAuth>>(
    store: &CodexTokenStore<S>,
    read_only: bool,
    now_ms: u64,
) -> (&'static str, Option<i64>) {
    let mode = if read_only { "read_only" } else { "read_write" };
    let expires_in_s = store
        .load_auth()
        .ok()
        .flatten()
        .map(|auth| (i128::from(auth.expires) - i128::from(now_ms)).div_euclid(1000) as i64);
    (mode, expires_in_s)
}

pub struct CodexAuthManager<S: AuthStorage<StoredAuth>> {
    pub store: CodexTokenStore<S>,
    #[cfg(test)]
    test_auth: Arc<Mutex<Option<StoredAuth>>>,
    refresh_lock: Arc<AsyncMutex<()>>,
    refresh_client: reqwest::Client,
    token_endpoint: String,
    /// When set, this manager only reads the store: it never refreshes, saves
    /// or clears (see `read_only_auth`).
    read_only: bool,
}

impl<S: AuthStorage<StoredAuth>> CodexAuthManager<S> {
    pub fn new(store: CodexTokenStore<S>) -> Self {
        Self::new_with_token_endpoint(store, format!("{ISSUER}/oauth/token"))
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn new_with_token_endpoint(store: CodexTokenStore<S>, token_endpoint: String) -> Self {
        Self {
            read_only: false,
            store,
            #[cfg(test)]
            test_auth: Arc::new(Mutex::new(None)),
            refresh_lock: CODEX_REFRESH_LOCK.clone(),
            refresh_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to create Codex OAuth refresh client"),
            token_endpoint,
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub async fn get_auth(&self) -> Result<StoredAuth, anyhow::Error> {
        let stored = self.load_auth()?.ok_or_else(|| {
            anyhow::anyhow!("Not authenticated. Run: claude-code-proxy codex auth login")
        })?;

        if self.read_only {
            return Self::accept_read_only(stored, None);
        }

        if stored.expires > Self::now_ms() + REFRESH_MARGIN_MS {
            return Ok(stored);
        }

        self.refresh(false, None).await
    }

    pub async fn force_refresh(&self, rejected_access: &str) -> Result<StoredAuth, anyhow::Error> {
        if self.read_only {
            let stored = self
                .load_auth()?
                .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
            return Self::accept_read_only(stored, Some(rejected_access));
        }
        self.refresh(true, Some(rejected_access)).await
    }

    /// Read-only policy: serve what the owning listener stored, never mint.
    /// After an upstream rejection the stored token is only useful if the
    /// owner has since replaced it.
    fn accept_read_only(
        stored: StoredAuth,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        let min_validity = if rejected_access.is_some() {
            0
        } else {
            READ_ONLY_MIN_VALIDITY_MS
        };
        let usable = stored.expires > Self::now_ms() + min_validity
            && rejected_access.is_none_or(|rejected| stored.access != rejected);
        if usable {
            Ok(stored)
        } else {
            anyhow::bail!(READ_ONLY_EXPIRED_MESSAGE)
        }
    }

    fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        #[cfg(test)]
        if let Some(auth) = self
            .test_auth
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone()
        {
            return Ok(Some(auth));
        }

        self.store.load_auth()
    }

    async fn refresh(
        &self,
        force: bool,
        rejected_access: Option<&str>,
    ) -> Result<StoredAuth, anyhow::Error> {
        // Backstop: every refresh path (and so every store write) passes here.
        if self.read_only {
            anyhow::bail!(READ_ONLY_EXPIRED_MESSAGE);
        }
        let _refresh_guard = self.refresh_lock.lock().await;

        // Reload from durable storage after acquiring the single-flight lock.
        // Another request may have rotated and persisted the token while this
        // caller was waiting.
        let current = self
            .load_auth()?
            .ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;

        if (!force && current.expires > Self::now_ms() + REFRESH_MARGIN_MS)
            || rejected_access.is_some_and(|access| current.access != access)
        {
            return Ok(current);
        }

        self.refresh_now(&current).await
    }

    async fn refresh_now(&self, current: &StoredAuth) -> Result<StoredAuth, anyhow::Error> {
        if current.refresh.is_empty() {
            anyhow::bail!("No refresh token stored; re-authenticate");
        }

        let form = [
            ("client_id", CLIENT_ID.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh.clone()),
        ];

        let resp = self
            .refresh_client
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("refresh network error: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            if let Some(latest) = self.store.load_auth()?
                && latest != *current
            {
                return Ok(latest);
            }
            self.store.clear_auth()?;
            let err_msg = resp
                .text()
                .await
                .unwrap_or_else(|_| "Token refresh unauthorized".to_string());
            anyhow::bail!("{err_msg}");
        }

        if !resp.status().is_success() {
            anyhow::bail!("Token refresh failed: {status}");
        }

        let tokens: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
        validate_token_response(&tokens)?;
        let account_id = extract_account_id(&tokens).or_else(|| current.account_id.clone());
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let next = StoredAuth {
            access: tokens.access_token,
            refresh: tokens.refresh_token,
            expires,
            account_id,
        };
        self.store.save_auth(next.clone())?;
        Ok(next)
    }

    pub fn persist_initial_tokens(
        &self,
        tokens: &TokenResponse,
    ) -> Result<StoredAuth, anyhow::Error> {
        if self.read_only {
            anyhow::bail!("codex auth is read-only: refusing to write the store");
        }
        validate_token_response(tokens)?;
        let account_id = extract_account_id(tokens);
        let expires = Self::now_ms() + (tokens.expires_in.unwrap_or(3600) * 1000);
        let auth = StoredAuth {
            access: tokens.access_token.clone(),
            refresh: tokens.refresh_token.clone(),
            expires,
            account_id,
        };
        self.store.save_auth(auth.clone())?;
        Ok(auth)
    }

    #[cfg(test)]
    pub fn set_test_auth(&self, auth: StoredAuth) {
        if let Ok(mut guard) = self.test_auth.lock() {
            *guard = Some(auth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn test_store() -> CodexTokenStore<InMemoryAuthStore<StoredAuth>> {
        CodexTokenStore::new(InMemoryAuthStore::new())
    }

    #[tokio::test]
    async fn get_auth_returns_stored() {
        let store = test_store();
        let auth = StoredAuth {
            access: "test_access".into(),
            refresh: "test_refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let manager = CodexAuthManager::new(store);
        let result = manager.get_auth().await.unwrap();
        assert_eq!(result.access, "test_access");
        assert_eq!(result.account_id.as_deref(), Some("acct_1"));
    }

    #[tokio::test]
    async fn get_auth_fails_when_no_auth() {
        let store = test_store();
        let manager = CodexAuthManager::new(store);
        assert!(manager.get_auth().await.is_err());
        assert!(
            manager
                .get_auth()
                .await
                .unwrap_err()
                .to_string()
                .contains("Not authenticated")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_expired_auth_refreshes_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let server_refreshes = refreshes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).contains("refresh_token=stale"));
            server_refreshes.fetch_add(1, Ordering::SeqCst);

            let body = br#"{"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "expired".into(),
                refresh: "stale".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = Arc::new(CodexAuthManager::new_with_token_endpoint(
            store,
            format!("http://{addr}/oauth/token"),
        ));
        let (first, second) = tokio::join!(manager.get_auth(), manager.get_auth());
        let results = [first.unwrap(), second.unwrap()];
        server.join().unwrap();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|auth| auth.access == "rotated"));
        assert!(results.iter().all(|auth| auth.refresh == "rotated-refresh"));
    }

    #[tokio::test]
    async fn stale_401_reuses_already_rotated_auth() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new_with_token_endpoint(
            store,
            "http://127.0.0.1:1/should-not-be-called".into(),
        );

        let auth = manager.force_refresh("rejected").await.unwrap();
        assert_eq!(auth.access, "rotated");
        assert_eq!(auth.refresh, "rotated-refresh");
    }

    #[tokio::test]
    async fn unauthorized_refresh_preserves_changed_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let backing = InMemoryAuthStore::new();
        let server_backing = backing.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            server_backing
                .save(StoredAuth {
                    access: "same-access".into(),
                    refresh: "replacement-refresh".into(),
                    expires: u64::MAX,
                    account_id: Some("acct_1".into()),
                })
                .unwrap();
            let body = b"rejected refresh token";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let store = CodexTokenStore::new(backing);
        store
            .save_auth(StoredAuth {
                access: "same-access".into(),
                refresh: "rejected-refresh".into(),
                expires: 0,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager =
            CodexAuthManager::new_with_token_endpoint(store, format!("http://{addr}/oauth/token"));

        let auth = manager.get_auth().await.unwrap();
        server.join().unwrap();
        assert_eq!(auth.access, "same-access");
        assert_eq!(auth.refresh, "replacement-refresh");
        assert_eq!(manager.store.load_auth().unwrap(), Some(auth));
    }

    /// Store spy: counts every write or clear that reaches the backing store.
    #[derive(Clone)]
    struct SpyStore {
        inner: InMemoryAuthStore<StoredAuth>,
        writes: Arc<AtomicUsize>,
    }

    impl AuthStorage<StoredAuth> for SpyStore {
        fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
            self.inner.load()
        }
        fn save(&self, value: StoredAuth) -> anyhow::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.inner.save(value)
        }
        fn clear(&self) -> anyhow::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.inner.clear()
        }
        fn path(&self) -> String {
            self.inner.path()
        }
    }

    fn auth_with(access: &str, expires: u64) -> StoredAuth {
        StoredAuth {
            access: access.into(),
            refresh: "refresh".into(),
            expires,
            account_id: Some("acct_1".into()),
        }
    }

    #[tokio::test]
    async fn read_only_never_writes_the_store_or_calls_the_token_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let now = CodexAuthManager::<SpyStore>::now_ms();
        let backing = InMemoryAuthStore::new();
        let writes = Arc::new(AtomicUsize::new(0));
        let spy = SpyStore {
            inner: backing.clone(),
            writes: writes.clone(),
        };
        let manager = CodexAuthManager::new_with_token_endpoint(
            CodexTokenStore::new(spy),
            format!("http://{addr}/oauth/token"),
        )
        .with_read_only(true);
        let expired_message = "codex auth is read-only: stored token is expired or expires within 60s; the listener that owns this store must refresh it";

        // get_auth: fresh is served; near-expiry and expired are refused.
        for (expires, served) in [(now + 3_600_000, true), (now + 30_000, false), (0, false)] {
            backing.save(auth_with("stored", expires)).unwrap();
            let result = manager.get_auth().await;
            assert_eq!(result.is_ok(), served, "expires={expires}");
            if !served {
                assert_eq!(result.unwrap_err().to_string(), expired_message);
            }
        }

        // 401 with an unchanged store: nothing newer to use, and no refresh.
        backing.save(auth_with("stored", now + 3_600_000)).unwrap();
        let err = manager.force_refresh("stored").await.unwrap_err();
        assert_eq!(err.to_string(), expired_message);

        // 401 after another process rotated the store: its token is returned.
        backing.save(auth_with("rotated", now + 3_600_000)).unwrap();
        assert_eq!(
            manager.force_refresh("stored").await.unwrap().access,
            "rotated"
        );

        // A rotated but already-expired token is refused too.
        backing.save(auth_with("rotated-expired", 0)).unwrap();
        assert!(manager.force_refresh("stored").await.is_err());

        // persist_initial_tokens (login) is also refused.
        let tokens: TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "a", "refresh_token": "r", "expires_in": 3600
        }))
        .unwrap();
        assert!(manager.persist_initial_tokens(&tokens).is_err());

        assert_eq!(writes.load(Ordering::SeqCst), 0, "store writes or clears");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "token endpoint received a connection"
        );
    }

    #[test]
    fn auth_health_reports_mode_and_signed_seconds_to_expiry() {
        let store = test_store();
        assert_eq!(auth_health(&store, false, 1_000_000), ("read_write", None));
        store.save_auth(auth_with("a", 1_061_500)).unwrap();
        assert_eq!(
            auth_health(&store, true, 1_000_000),
            ("read_only", Some(61))
        );
        assert_eq!(
            auth_health(&store, false, 1_070_000),
            ("read_write", Some(-9))
        );
    }

    #[tokio::test]
    async fn durable_rotation_and_logout_are_observed_by_shared_manager() {
        let store = test_store();
        store
            .save_auth(StoredAuth {
                access: "first".into(),
                refresh: "first-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_1".into()),
            })
            .unwrap();
        let manager = CodexAuthManager::new(store);
        assert_eq!(manager.get_auth().await.unwrap().access, "first");

        manager
            .store
            .save_auth(StoredAuth {
                access: "rotated".into(),
                refresh: "rotated-refresh".into(),
                expires: u64::MAX,
                account_id: Some("acct_2".into()),
            })
            .unwrap();
        let rotated = manager.get_auth().await.unwrap();
        assert_eq!(rotated.access, "rotated");
        assert_eq!(rotated.account_id.as_deref(), Some("acct_2"));

        manager.store.clear_auth().unwrap();
        assert!(manager.get_auth().await.is_err());
    }
}
