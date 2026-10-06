use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::jwk::Jwk;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::KeySet;

/// The largest JWK set body accepted.
pub const BODY_CAP: u64 = 64 * 1024;
const STARTUP_RETRY: Duration = Duration::from_secs(30);
const REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const REFRESH_RETRY: Duration = Duration::from_secs(5 * 60);
const REFETCH_GATE: Duration = Duration::from_secs(60);

/// Why fetching the provider's JWK set failed; carries only the kind, never the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The provider answered with an HTTP error status.
    #[error("status {0}")]
    Status(u16),
    /// The provider did not answer within the deadline.
    #[error("timed out")]
    Timeout,
    /// The body is larger than [`BODY_CAP`].
    #[error("body too large")]
    TooLarge,
    /// The connection, TLS or HTTP exchange failed.
    #[error("transport error")]
    Transport,
    /// The body is not a JWK set.
    #[error("not a jwk set")]
    Unparsable,
    /// The body is a JWK set without one usable RS256 signing key.
    #[error("no usable keys")]
    NoKeys,
}

/// Where the provider's JWK set comes from.
pub trait JwksSource: Send + Sync {
    /// Fetches the JWK set body; may block, so [`Jwks`] runs it on `spawn_blocking`.
    fn fetch(&self) -> Result<Vec<u8>, FetchError>;
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    fetched_at: u64,
    jwks: Value,
}

#[derive(Deserialize)]
struct RawSet {
    keys: Vec<Value>,
}

#[derive(Default)]
struct Loaded {
    keys: Arc<KeySet>,
    fetched_at: Option<u64>,
}

/// The provider's signing keys, refreshed in the background and persisted beside the config.
pub struct Jwks {
    source: Arc<dyn JwksSource>,
    cache_path: PathBuf,
    loaded: RwLock<Loaded>,
    last_refetch: Mutex<Option<Instant>>,
}

impl Jwks {
    /// Starts from the keys persisted at `cache_path`; a missing or unparsable file leaves the set empty.
    pub fn new(source: Arc<dyn JwksSource>, cache_path: PathBuf) -> Self {
        let loaded = load(&cache_path);
        if !loaded.keys.is_empty() {
            tracing::info!(keys = loaded.keys.len(), from = "file", "jwks loaded");
        }
        Self {
            source,
            cache_path,
            loaded: RwLock::new(loaded),
            last_refetch: Mutex::new(None),
        }
    }

    /// The keys in use.
    pub fn keys(&self) -> Arc<KeySet> {
        let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&loaded.keys)
    }

    /// The number of usable keys in use.
    pub fn key_count(&self) -> usize {
        self.keys().len()
    }

    /// Seconds since the keys in use were fetched, `None` when none are loaded.
    pub fn age(&self) -> Option<u64> {
        let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
        let fetched_at = loaded.fetched_at?;
        Some(unix_now().saturating_sub(fetched_at))
    }

    /// Fetches until the first success, retrying every 30 s, then refreshes every 12 h, retrying a failed refresh in 5 min.
    pub async fn run_refresh(self: Arc<Self>) {
        let mut retry = STARTUP_RETRY;
        loop {
            let wait = match self.refresh().await {
                Ok(()) => {
                    retry = REFRESH_RETRY;
                    REFRESH_INTERVAL
                }
                Err(err) => {
                    tracing::warn!(error = %err, retry_in_s = retry.as_secs(), "cannot fetch the jwks");
                    retry
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Fetches once for a token naming an unknown `kid`, unless the last such fetch was under 60 s ago; concurrent callers share one fetch.
    pub async fn refetch_unknown_key(&self) {
        let mut last = self.last_refetch.lock().await;
        if let Some(at) = *last
            && at.elapsed() < REFETCH_GATE
        {
            return;
        }
        *last = Some(Instant::now());
        if let Err(err) = self.fetch_and_store().await {
            tracing::warn!(error = %err, "cannot fetch the jwks for an unknown key");
        }
    }

    async fn refresh(&self) -> Result<(), FetchError> {
        let _fetching = self.last_refetch.lock().await;
        self.fetch_and_store().await
    }

    async fn fetch_and_store(&self) -> Result<(), FetchError> {
        let source = Arc::clone(&self.source);
        let Ok(body) = tokio::task::spawn_blocking(move || source.fetch()).await else {
            return Err(FetchError::Transport);
        };
        let body = body?;
        let size = u64::try_from(body.len()).unwrap_or(u64::MAX);
        if size > BODY_CAP {
            return Err(FetchError::TooLarge);
        }
        let Ok(jwks) = serde_json::from_slice::<Value>(&body) else {
            return Err(FetchError::Unparsable);
        };
        let keys = key_set(&jwks)?;
        let count = keys.len();
        let persisted = Persisted {
            fetched_at: unix_now(),
            jwks,
        };
        if let Err(err) = persist(&self.cache_path, &persisted) {
            tracing::warn!(error = %err.kind(), "cannot persist the jwks");
        }
        let mut loaded = self.loaded.write().unwrap_or_else(PoisonError::into_inner);
        *loaded = Loaded {
            keys: Arc::new(keys),
            fetched_at: Some(persisted.fetched_at),
        };
        tracing::info!(keys = count, from = "provider", "jwks loaded");
        Ok(())
    }
}

fn key_set(jwks: &Value) -> Result<KeySet, FetchError> {
    let Ok(RawSet { keys }) = RawSet::deserialize(jwks) else {
        return Err(FetchError::Unparsable);
    };
    let mut parsed = Vec::new();
    for key in keys {
        let Ok(jwk) = serde_json::from_value::<Jwk>(key) else {
            continue;
        };
        parsed.push(jwk);
    }
    let keys = KeySet::from_jwks(&parsed);
    if keys.is_empty() {
        return Err(FetchError::NoKeys);
    }
    Ok(keys)
}

fn load(path: &Path) -> Loaded {
    let Ok(text) = fs::read(path) else {
        tracing::info!("no persisted jwks");
        return Loaded::default();
    };
    let Ok(Persisted { fetched_at, jwks }) = serde_json::from_slice(&text) else {
        tracing::warn!("persisted jwks unparsable, ignored");
        return Loaded::default();
    };
    let Ok(keys) = key_set(&jwks) else {
        tracing::warn!("persisted jwks has no usable keys, ignored");
        return Loaded::default();
    };
    Loaded {
        keys: Arc::new(keys),
        fetched_at: Some(fetched_at),
    }
}

fn persist(path: &Path, persisted: &Persisted) -> io::Result<()> {
    let text = serde_json::to_vec(persisted)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)
}

fn unix_now() -> u64 {
    let Ok(since) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    since.as_secs()
}

#[cfg(test)]
pub(crate) mod test_source {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use serde_json::{Value, json};

    use super::{FetchError, Jwks, JwksSource, unix_now};
    use crate::auth::test_keys::KEY;

    /// A source that answers each fetch with the next scripted result, then with `Transport`.
    #[derive(Default)]
    pub(crate) struct ScriptedSource {
        script: Mutex<VecDeque<Result<Vec<u8>, FetchError>>>,
        calls: AtomicUsize,
    }

    impl ScriptedSource {
        /// A source answering with `script` in order.
        pub(crate) fn new(script: Vec<Result<Vec<u8>, FetchError>>) -> Self {
            Self {
                script: Mutex::new(VecDeque::from(script)),
                calls: AtomicUsize::new(0),
            }
        }

        /// The number of fetches so far.
        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl JwksSource for ScriptedSource {
        fn fetch(&self) -> Result<Vec<u8>, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
            script.pop_front().unwrap_or(Err(FetchError::Transport))
        }
    }

    /// A JWK set body holding the trusted test key under each of `kids`.
    pub(crate) fn body(kids: &[&str]) -> Vec<u8> {
        let mut keys = Vec::new();
        for kid in kids {
            keys.push(KEY.jwk_json(kid));
        }
        body_of(keys)
    }

    /// A JWK set body holding `keys` as given.
    pub(crate) fn body_of(keys: Vec<Value>) -> Vec<u8> {
        json!({ "keys": keys }).to_string().into_bytes()
    }

    /// A cache in `dir` loaded at construction with the trusted key under each of `kids`, whose source never answers; no file is written when `kids` is empty.
    pub(crate) fn preloaded(dir: &Path, kids: &[&str]) -> Arc<Jwks> {
        let path = dir.join("jwks-cache.json");
        if !kids.is_empty() {
            let jwks: Value = serde_json::from_slice(&body(kids)).unwrap();
            let text = json!({ "fetched_at": unix_now(), "jwks": jwks }).to_string();
            fs::write(&path, text).unwrap();
        }
        Arc::new(Jwks::new(Arc::new(ScriptedSource::default()), path))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::TempDir;

    use super::test_source::{ScriptedSource, body, body_of};
    use super::*;
    use crate::auth::test_keys::KEY;

    const KID: &str = "key-1";
    const SETTLE_ROUNDS: usize = 20;
    const WAIT_ROUNDS: usize = 10_000;

    struct Harness {
        dir: TempDir,
        source: Arc<ScriptedSource>,
        jwks: Arc<Jwks>,
    }

    impl Harness {
        fn new(script: Vec<Result<Vec<u8>, FetchError>>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self::in_dir(dir, script)
        }

        fn in_dir(dir: TempDir, script: Vec<Result<Vec<u8>, FetchError>>) -> Self {
            let source = Arc::new(ScriptedSource::new(script));
            let jwks = Arc::new(Jwks::new(
                Arc::clone(&source) as Arc<dyn JwksSource>,
                cache_path(&dir),
            ));
            Self { dir, source, jwks }
        }

        fn spawn_refresh(&self) -> tokio::task::JoinHandle<()> {
            tokio::spawn(Arc::clone(&self.jwks).run_refresh())
        }

        async fn wait_for(&self, calls: usize, keys: usize) {
            for _ in 0..WAIT_ROUNDS {
                if self.source.calls() == calls && self.jwks.key_count() == keys {
                    settle().await;
                    assert_eq!(self.source.calls(), calls);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            panic!(
                "expected {calls} calls and {keys} keys, have {} calls and {} keys",
                self.source.calls(),
                self.jwks.key_count()
            );
        }
    }

    fn cache_path(dir: &TempDir) -> PathBuf {
        dir.path().join("jwks-cache.json")
    }

    async fn settle() {
        for _ in 0..SETTLE_ROUNDS {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn write_cache(dir: &TempDir, fetched_at: u64, jwks: &[u8]) {
        let jwks: Value = serde_json::from_slice(jwks).unwrap();
        let text = json!({ "fetched_at": fetched_at, "jwks": jwks }).to_string();
        fs::write(cache_path(dir), text).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_startup_fetch_retries_after_30_s() {
        let harness = Harness::new(vec![Err(FetchError::Status(500)), Ok(body(&[KID]))]);
        let task = harness.spawn_refresh();

        harness.wait_for(1, 0).await;
        tokio::time::advance(Duration::from_secs(28)).await;
        settle().await;
        assert_eq!(harness.source.calls(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        harness.wait_for(2, 1).await;

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_fires_every_12_h() {
        let harness = Harness::new(vec![Ok(body(&[KID])), Ok(body(&[KID, "key-2"]))]);
        let task = harness.spawn_refresh();

        harness.wait_for(1, 1).await;
        tokio::time::advance(Duration::from_secs(12 * 60 * 60 - 60)).await;
        settle().await;
        assert_eq!(harness.source.calls(), 1);
        tokio::time::advance(Duration::from_secs(60)).await;
        harness.wait_for(2, 2).await;

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_keeps_the_old_keys_and_retries_in_5_min() {
        let harness = Harness::new(vec![
            Ok(body(&[KID])),
            Err(FetchError::Timeout),
            Ok(body(&[KID, "key-2"])),
        ]);
        let task = harness.spawn_refresh();

        harness.wait_for(1, 1).await;
        tokio::time::advance(Duration::from_secs(12 * 60 * 60)).await;
        harness.wait_for(2, 1).await;
        tokio::time::advance(Duration::from_secs(290)).await;
        settle().await;
        assert_eq!(harness.source.calls(), 2);
        tokio::time::advance(Duration::from_secs(10)).await;
        harness.wait_for(3, 2).await;

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn body_without_rsa_keys_keeps_the_old_keys() {
        let harness = Harness::new(vec![
            Ok(body(&[KID])),
            Ok(body_of(vec![
                json!({"kty": "oct", "kid": "hmac", "k": "c2VjcmV0"}),
            ])),
        ]);
        harness.jwks.refresh().await.unwrap();

        let result = harness.jwks.refresh().await;

        assert_eq!(result, Err(FetchError::NoKeys));
        assert_eq!(harness.jwks.key_count(), 1);
        assert!(harness.jwks.keys().get(KID).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn empty_key_list_is_a_failed_fetch() {
        let harness = Harness::new(vec![Ok(body(&[]))]);

        assert_eq!(harness.jwks.refresh().await, Err(FetchError::NoKeys));
        assert_eq!(harness.jwks.key_count(), 0);
        assert_eq!(harness.jwks.age(), None);
        assert!(!cache_path(&harness.dir).exists());
    }

    #[tokio::test(start_paused = true)]
    async fn body_over_the_cap_is_a_failure() {
        let mut oversized: Value = serde_json::from_slice(&body(&[KID])).unwrap();
        oversized["padding"] = json!("x".repeat(70 * 1024));
        let harness = Harness::new(vec![Ok(oversized.to_string().into_bytes())]);

        assert_eq!(harness.jwks.refresh().await, Err(FetchError::TooLarge));
        assert_eq!(harness.jwks.key_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn unparsable_body_is_a_failure() {
        let harness = Harness::new(vec![
            Ok(b"<html>".to_vec()),
            Ok(br#"{"keys": "none"}"#.to_vec()),
        ]);

        assert_eq!(harness.jwks.refresh().await, Err(FetchError::Unparsable));
        assert_eq!(harness.jwks.refresh().await, Err(FetchError::Unparsable));
        assert_eq!(harness.jwks.key_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn source_error_is_passed_through() {
        let harness = Harness::new(vec![Err(FetchError::Status(404))]);

        assert_eq!(harness.jwks.refresh().await, Err(FetchError::Status(404)));
    }

    #[tokio::test(start_paused = true)]
    async fn encryption_and_es256_keys_are_dropped() {
        let mut enc = KEY.jwk_json("enc");
        enc["use"] = json!("enc");
        let mut es256 = KEY.jwk_json("es256");
        es256["alg"] = json!("ES256");
        let broken = json!({"kty": "RSA", "kid": "broken"});
        let harness = Harness::new(vec![Ok(body_of(vec![
            KEY.jwk_json(KID),
            enc,
            es256,
            broken,
        ]))]);

        harness.jwks.refresh().await.unwrap();

        let keys = harness.jwks.keys();
        assert_eq!(keys.len(), 1);
        assert!(keys.get(KID).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn good_fetch_is_persisted_and_loaded_by_a_fresh_jwks() {
        let harness = Harness::new(vec![Ok(body(&[KID, "key-2"]))]);

        harness.jwks.refresh().await.unwrap();

        let text = fs::read(cache_path(&harness.dir)).unwrap();
        let Persisted { fetched_at, jwks } = serde_json::from_slice(&text).unwrap();
        assert!(unix_now() - fetched_at <= 1);
        assert_eq!(jwks["keys"].as_array().unwrap().len(), 2);
        assert!(!harness.dir.path().join("jwks-cache.json.tmp").exists());
        let Harness { dir, .. } = harness;
        let fresh = Harness::in_dir(dir, vec![]);
        assert_eq!(fresh.jwks.key_count(), 2);
        assert!(fresh.jwks.age().unwrap() <= 1);
        assert_eq!(fresh.source.calls(), 0);
    }

    #[test]
    fn age_is_computed_from_fetched_at() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(&dir, unix_now() - 3600, &body(&[KID]));

        let harness = Harness::in_dir(dir, vec![]);

        assert_eq!(harness.jwks.key_count(), 1);
        let age = harness.jwks.age().unwrap();
        assert!((3600..=3601).contains(&age), "{age}");
    }

    #[test]
    fn missing_file_is_absent() {
        let harness = Harness::new(vec![]);

        assert_eq!(harness.jwks.key_count(), 0);
        assert_eq!(harness.jwks.age(), None);
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(cache_path(&dir), "{not json").unwrap();

        let harness = Harness::in_dir(dir, vec![]);

        assert_eq!(harness.jwks.key_count(), 0);
        assert_eq!(harness.jwks.age(), None);
    }

    #[test]
    fn file_without_usable_keys_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(&dir, unix_now(), &body(&[]));

        let harness = Harness::in_dir(dir, vec![]);

        assert_eq!(harness.jwks.key_count(), 0);
        assert_eq!(harness.jwks.age(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_keys_serve_while_the_startup_fetch_keeps_failing() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(&dir, unix_now() - 600, &body(&[KID]));
        let harness = Harness::in_dir(
            dir,
            vec![
                Err(FetchError::Transport),
                Err(FetchError::Status(502)),
                Ok(body(&[KID, "key-2"])),
            ],
        );
        let task = harness.spawn_refresh();

        harness.wait_for(1, 1).await;
        assert!(harness.jwks.keys().get(KID).is_some());
        assert!((600..=601).contains(&harness.jwks.age().unwrap()));
        tokio::time::advance(Duration::from_secs(30)).await;
        harness.wait_for(2, 1).await;
        tokio::time::advance(Duration::from_secs(29)).await;
        settle().await;
        assert_eq!(harness.source.calls(), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        harness.wait_for(3, 2).await;
        assert!(harness.jwks.age().unwrap() <= 1);

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_persist_still_loads_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let source = Arc::new(ScriptedSource::new(vec![Ok(body(&[KID]))]));
        let jwks = Jwks::new(source, dir.path().join("missing").join("jwks-cache.json"));

        jwks.refresh().await.unwrap();

        assert_eq!(jwks.key_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn refetch_is_gated_for_60_s() {
        let harness = Harness::new(vec![Ok(body(&[KID])), Ok(body(&[KID, "key-2"]))]);

        harness.jwks.refetch_unknown_key().await;
        assert_eq!(harness.source.calls(), 1);
        tokio::time::advance(Duration::from_secs(59)).await;
        harness.jwks.refetch_unknown_key().await;
        assert_eq!(harness.source.calls(), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        harness.jwks.refetch_unknown_key().await;

        assert_eq!(harness.source.calls(), 2);
        assert_eq!(harness.jwks.key_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refetch_still_closes_the_gate() {
        let harness = Harness::new(vec![Err(FetchError::Transport), Ok(body(&[KID]))]);

        harness.jwks.refetch_unknown_key().await;
        harness.jwks.refetch_unknown_key().await;

        assert_eq!(harness.source.calls(), 1);
        assert_eq!(harness.jwks.key_count(), 0);
    }

    #[test]
    fn fetch_error_text_names_only_the_kind() {
        assert_eq!(FetchError::Status(503).to_string(), "status 503");
        assert_eq!(FetchError::Timeout.to_string(), "timed out");
        assert_eq!(FetchError::TooLarge.to_string(), "body too large");
        assert_eq!(FetchError::Transport.to_string(), "transport error");
        assert_eq!(FetchError::Unparsable.to_string(), "not a jwk set");
        assert_eq!(FetchError::NoKeys.to_string(), "no usable keys");
    }
}
