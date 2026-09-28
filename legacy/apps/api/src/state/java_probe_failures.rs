use axial_minecraft::JavaRuntimeProbeSnapshot;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const JAVA_PROBE_FAILURE_TTL: Duration = Duration::from_secs(3);
const JAVA_PROBE_FAILURE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum JavaProbeFailureKind {
    Missing,
    SpawnFailed,
    TimedOut,
    OutputParseFailed,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct JavaProbeFailureKey {
    snapshot: JavaRuntimeProbeSnapshot,
    required_major: Option<u32>,
    required_min_update: Option<u32>,
}

impl JavaProbeFailureKey {
    pub(crate) fn new(
        snapshot: JavaRuntimeProbeSnapshot,
        required_major: Option<u32>,
        required_min_update: Option<u32>,
    ) -> Self {
        Self {
            snapshot,
            required_major,
            required_min_update,
        }
    }
}

enum FailureEntryState {
    InFlight(tokio::sync::watch::Sender<bool>),
    Ready {
        kind: JavaProbeFailureKind,
        expires_at: Instant,
    },
}

struct FailureEntry {
    key: JavaProbeFailureKey,
    state: FailureEntryState,
}

#[derive(Default)]
pub(crate) struct JavaProbeFailureCache {
    entries: Mutex<VecDeque<FailureEntry>>,
}

pub(crate) enum JavaProbeFailureClaim {
    Hit(JavaProbeFailureKind),
    Owner(Box<JavaProbeFailureOwner>),
    Uncached,
}

pub(crate) struct JavaProbeFailureOwner {
    cache: Arc<JavaProbeFailureCache>,
    key: Option<JavaProbeFailureKey>,
}

impl JavaProbeFailureCache {
    pub(crate) async fn claim(self: &Arc<Self>, key: JavaProbeFailureKey) -> JavaProbeFailureClaim {
        loop {
            let mut waiter = {
                let now = Instant::now();
                let mut entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                entries.retain(|entry| match entry.state {
                    FailureEntryState::InFlight(_) => true,
                    FailureEntryState::Ready { expires_at, .. } => expires_at > now,
                });
                if let Some(entry) = entries.iter().find(|entry| entry.key == key) {
                    match &entry.state {
                        FailureEntryState::Ready { kind, .. } => {
                            return JavaProbeFailureClaim::Hit(*kind);
                        }
                        FailureEntryState::InFlight(completed) => completed.subscribe(),
                    }
                } else {
                    if entries.len() >= JAVA_PROBE_FAILURE_CAPACITY {
                        if let Some(index) = entries.iter().position(|entry| {
                            matches!(entry.state, FailureEntryState::Ready { .. })
                        }) {
                            entries.remove(index);
                        } else {
                            return JavaProbeFailureClaim::Uncached;
                        }
                    }
                    let (completed, _) = tokio::sync::watch::channel(false);
                    entries.push_back(FailureEntry {
                        key: key.clone(),
                        state: FailureEntryState::InFlight(completed),
                    });
                    return JavaProbeFailureClaim::Owner(Box::new(JavaProbeFailureOwner {
                        cache: self.clone(),
                        key: Some(key),
                    }));
                }
            };
            let _ = waiter.changed().await;
        }
    }

    fn finish(&self, key: &JavaProbeFailureKey, kind: Option<JavaProbeFailureKind>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(index) = entries.iter().position(|entry| &entry.key == key) else {
            return;
        };
        let completed = match &entries[index].state {
            FailureEntryState::InFlight(completed) => completed.clone(),
            FailureEntryState::Ready { .. } => return,
        };
        if let Some(kind) = kind {
            entries[index].state = FailureEntryState::Ready {
                kind,
                expires_at: Instant::now() + JAVA_PROBE_FAILURE_TTL,
            };
        } else {
            entries.remove(index);
        }
        let _ = completed.send(true);
    }

    #[cfg(test)]
    fn expire_ready_for_test(&self) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in entries.iter_mut() {
            if let FailureEntryState::Ready { expires_at, .. } = &mut entry.state {
                *expires_at = Instant::now();
            }
        }
    }
}

impl JavaProbeFailureOwner {
    pub(crate) fn finish(mut self, kind: JavaProbeFailureKind) {
        if let Some(key) = self.key.take() {
            self.cache.finish(&key, Some(kind));
        }
    }

    pub(crate) fn dismiss(mut self) {
        if let Some(key) = self.key.take() {
            self.cache.finish(&key, None);
        }
    }
}

impl Drop for JavaProbeFailureOwner {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.cache.finish(&key, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::snapshot_java_runtime;

    #[test]
    fn snapshot_key_changes_for_creation_replacement_alias_and_requirements() {
        let root =
            std::env::temp_dir().join(format!("axial-java-failure-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first_alias = root.join("java-a");
        let second_alias = root.join("java-b");
        let missing = JavaProbeFailureKey::new(
            snapshot_java_runtime(&first_alias).expect("missing snapshot"),
            Some(21),
            None,
        );
        std::fs::create_dir_all(&root).expect("key root");
        std::fs::write(&first_alias, b"first").expect("first executable");
        let created = JavaProbeFailureKey::new(
            snapshot_java_runtime(&first_alias).expect("created snapshot"),
            Some(21),
            None,
        );
        assert!(missing != created);

        std::fs::write(&first_alias, b"replacement").expect("replace executable");
        let replaced = JavaProbeFailureKey::new(
            snapshot_java_runtime(&first_alias).expect("replacement snapshot"),
            Some(21),
            None,
        );
        assert!(created != replaced);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&first_alias)
                .expect("replacement metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&first_alias, permissions).expect("execute permission");
            let permission_changed = JavaProbeFailureKey::new(
                snapshot_java_runtime(&first_alias).expect("permission snapshot"),
                Some(21),
                None,
            );
            assert!(replaced != permission_changed);
        }

        std::fs::hard_link(&first_alias, &second_alias).expect("second alias");
        let aliased = JavaProbeFailureKey::new(
            snapshot_java_runtime(&second_alias).expect("alias snapshot"),
            Some(21),
            None,
        );
        assert!(replaced != aliased);
        assert!(
            replaced
                != JavaProbeFailureKey::new(
                    snapshot_java_runtime(&first_alias).expect("requirement snapshot"),
                    Some(17),
                    None,
                )
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ttl_requirement_and_capacity_are_bounded() {
        let cache = Arc::new(JavaProbeFailureCache::default());
        let first = key("first", Some(21), None);
        finish_owner(
            cache.claim(first.clone()).await,
            JavaProbeFailureKind::SpawnFailed,
        );
        assert!(matches!(
            cache.claim(first.clone()).await,
            JavaProbeFailureClaim::Hit(JavaProbeFailureKind::SpawnFailed)
        ));

        let changed_requirement = key("first", Some(17), None);
        let claim = cache.claim(changed_requirement).await;
        assert!(matches!(&claim, JavaProbeFailureClaim::Owner(_)));
        dismiss_owner(claim);

        cache.expire_ready_for_test();
        let claim = cache.claim(first.clone()).await;
        assert!(matches!(&claim, JavaProbeFailureClaim::Owner(_)));
        dismiss_owner(claim);

        for index in 0..JAVA_PROBE_FAILURE_CAPACITY {
            finish_owner(
                cache
                    .claim(key(&format!("entry-{index}"), Some(21), None))
                    .await,
                JavaProbeFailureKind::TimedOut,
            );
        }
        finish_owner(
            cache.claim(key("entry-overflow", Some(21), None)).await,
            JavaProbeFailureKind::TimedOut,
        );
        let claim = cache.claim(key("entry-0", Some(21), None)).await;
        assert!(matches!(&claim, JavaProbeFailureClaim::Owner(_)));
        dismiss_owner(claim);
    }

    #[tokio::test]
    async fn same_key_singleflights_and_cancelled_owner_wakes_followers() {
        let cache = Arc::new(JavaProbeFailureCache::default());
        let singleflight_key = key("singleflight", Some(21), None);
        let owner = match cache.claim(singleflight_key.clone()).await {
            JavaProbeFailureClaim::Owner(owner) => owner,
            _ => panic!("first claim must own probe"),
        };
        let follower_cache = cache.clone();
        let follower_key = singleflight_key;
        let follower = tokio::spawn(async move { follower_cache.claim(follower_key).await });
        tokio::task::yield_now().await;
        assert!(!follower.is_finished());
        owner.finish(JavaProbeFailureKind::OutputParseFailed);
        assert!(matches!(
            follower.await.expect("follower"),
            JavaProbeFailureClaim::Hit(JavaProbeFailureKind::OutputParseFailed)
        ));

        let cancelled_key = key("cancelled", Some(21), None);
        let owner = match cache.claim(cancelled_key.clone()).await {
            JavaProbeFailureClaim::Owner(owner) => owner,
            _ => panic!("first cancellation claim must own probe"),
        };
        let follower_cache = cache.clone();
        let follower = tokio::spawn(async move { follower_cache.claim(cancelled_key).await });
        tokio::task::yield_now().await;
        drop(owner);
        let claim = follower.await.expect("cancelled follower");
        assert!(matches!(&claim, JavaProbeFailureClaim::Owner(_)));
        dismiss_owner(claim);
    }

    fn key(
        alias: &str,
        required_major: Option<u32>,
        required_min_update: Option<u32>,
    ) -> JavaProbeFailureKey {
        let path = std::env::temp_dir().join(format!(
            "axial-java-failure-cache-key-{}-{alias}",
            std::process::id()
        ));
        JavaProbeFailureKey::new(
            snapshot_java_runtime(&path).expect("missing snapshot"),
            required_major,
            required_min_update,
        )
    }

    fn finish_owner(claim: JavaProbeFailureClaim, kind: JavaProbeFailureKind) {
        match claim {
            JavaProbeFailureClaim::Owner(owner) => owner.finish(kind),
            _ => panic!("expected owner claim"),
        }
    }

    fn dismiss_owner(claim: JavaProbeFailureClaim) {
        match claim {
            JavaProbeFailureClaim::Owner(owner) => owner.dismiss(),
            _ => panic!("expected owner claim"),
        }
    }
}
