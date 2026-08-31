use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::time::Instant;

struct SessionCache<K, V> {
   capacity: usize,
   entries: Vec<SessionCacheEntry<K, V>>,
}

struct SessionCacheEntry<K, V> {
   key: K,
   value: V,
   expires_at: Option<Instant>,
}

impl<K: PartialEq, V: Clone> SessionCache<K, V> {
   fn new(capacity: usize) -> Self {
      Self {
         capacity: capacity.max(1),
         entries: Vec::new(),
      }
   }

   fn remove_expired(&mut self, now: Instant) {
      self
         .entries
         .retain(|entry| entry.expires_at.is_none_or(|deadline| deadline > now));
   }

   fn get(&mut self, key: &K, now: Instant) -> Option<V> {
      self.remove_expired(now);
      let index = self.entries.iter().position(|entry| &entry.key == key)?;
      let entry = self.entries.remove(index);
      let value = entry.value.clone();
      self.entries.push(entry);
      Some(value)
   }

   fn insert(&mut self, key: K, value: V, expires_at: Option<Instant>) {
      self.entries.retain(|entry| entry.key != key);
      if self.entries.len() >= self.capacity {
         self.entries.remove(0);
      }
      self.entries.push(SessionCacheEntry {
         key,
         value,
         expires_at,
      });
   }
}

#[derive(Default)]
struct SessionCacheReaper {
   started: AtomicBool,
}

impl SessionCacheReaper {
   fn start<K, V>(&self, cache: Arc<Mutex<SessionCache<K, V>>>, interval: Duration)
   where
      K: PartialEq + Send + 'static,
      V: Clone + Send + 'static,
   {
      if self
         .started
         .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
         .is_err()
      {
         return;
      }
      let cache = Arc::downgrade(&cache);
      tauri::async_runtime::spawn(async move {
         loop {
            tokio::time::sleep(interval).await;
            let Some(cache) = cache.upgrade() else {
               break;
            };
            let Ok(mut cache) = cache.lock() else {
               break;
            };
            cache.remove_expired(Instant::now());
         }
      });
   }
}

pub(crate) struct SessionPool<K, V> {
   cache: Arc<Mutex<SessionCache<K, Arc<V>>>>,
   expiration_reaper: SessionCacheReaper,
   reaper_interval: Duration,
   build_locks: Mutex<HashMap<K, BuildEntry<V>>>,
}

struct BuildSlot<V> {
   gate: tauri::async_runtime::Mutex<()>,
   result: Mutex<Option<Arc<V>>>,
}

impl<V> BuildSlot<V> {
   fn new() -> Self {
      Self {
         gate: tauri::async_runtime::Mutex::new(()),
         result: Mutex::new(None),
      }
   }

   fn result(&self) -> crate::Result<Option<Arc<V>>> {
      self
         .result
         .lock()
         .map_err(|_| crate::Error::Custom("session build state is unavailable".to_string()))
         .map(|result| result.clone())
   }

   fn complete(&self, value: Arc<V>) -> crate::Result<()> {
      let mut stored = self
         .result
         .lock()
         .map_err(|_| crate::Error::Custom("session build state is unavailable".to_string()))?;
      if stored.is_some() {
         return Err(crate::Error::Custom(
            "session build state completed more than once".to_string(),
         ));
      }
      *stored = Some(value);
      Ok(())
   }
}

struct BuildEntry<V> {
   slot: Weak<BuildSlot<V>>,
   leases: usize,
}

struct BuildLockLease<'a, K: Eq + Hash, V> {
   table: &'a Mutex<HashMap<K, BuildEntry<V>>>,
   key: K,
   slot: Arc<BuildSlot<V>>,
   cleaned: bool,
}

impl<K: Eq + Hash, V> BuildLockLease<'_, K, V> {
   fn cleanup(&mut self) -> crate::Result<()> {
      let mut locks = self
         .table
         .lock()
         .map_err(|_| crate::Error::Custom("session lock table is unavailable".to_string()))?;
      release_build_lease(&mut locks, &self.key, &self.slot);
      self.cleaned = true;
      Ok(())
   }
}

impl<K: Eq + Hash, V> Drop for BuildLockLease<'_, K, V> {
   fn drop(&mut self) {
      if self.cleaned {
         return;
      }
      if let Ok(mut locks) = self.table.lock() {
         release_build_lease(&mut locks, &self.key, &self.slot);
      }
   }
}

fn release_build_lease<K: Eq + Hash, V>(
   locks: &mut HashMap<K, BuildEntry<V>>,
   key: &K,
   slot: &Arc<BuildSlot<V>>,
) {
   let remove = locks.get_mut(key).is_some_and(|entry| {
      if !entry.slot.ptr_eq(&Arc::downgrade(slot)) {
         return false;
      }
      if entry.leases <= 1 {
         true
      } else {
         entry.leases -= 1;
         false
      }
   });
   if remove {
      locks.remove(key);
   }
}

impl<K, V> SessionPool<K, V>
where
   K: Clone + Eq + Hash + Send + 'static,
   V: Send + Sync + 'static,
{
   pub(crate) fn new(capacity: usize, reaper_interval: Duration) -> Self {
      Self {
         cache: Arc::new(Mutex::new(SessionCache::new(capacity))),
         expiration_reaper: SessionCacheReaper::default(),
         reaper_interval,
         build_locks: Mutex::new(HashMap::new()),
      }
   }

   fn cached(&self, key: &K) -> crate::Result<Option<Arc<V>>> {
      self
         .cache
         .lock()
         .map_err(|_| crate::Error::Custom("session cache is unavailable".to_string()))
         .map(|mut cache| cache.get(key, Instant::now()))
   }

   fn build_lock(&self, key: &K) -> crate::Result<BuildLockLease<'_, K, V>> {
      let mut locks = self
         .build_locks
         .lock()
         .map_err(|_| crate::Error::Custom("session lock table is unavailable".to_string()))?;
      locks.retain(|_, entry| entry.leases > 0 && entry.slot.strong_count() > 0);
      let slot = if let Some(entry) = locks.get_mut(key) {
         let slot = entry.slot.upgrade().ok_or_else(|| {
            crate::Error::Custom("session build state is unavailable".to_string())
         })?;
         entry.leases = entry.leases.checked_add(1).ok_or_else(|| {
            crate::Error::Custom("too many concurrent session builds".to_string())
         })?;
         slot
      } else {
         let slot = Arc::new(BuildSlot::new());
         locks.insert(
            key.clone(),
            BuildEntry {
               slot: Arc::downgrade(&slot),
               leases: 1,
            },
         );
         slot
      };
      Ok(BuildLockLease {
         table: &self.build_locks,
         key: key.clone(),
         slot,
         cleaned: false,
      })
   }

   fn insert_cached(&self, key: K, value: Arc<V>, ttl: Duration) -> crate::Result<()> {
      let expires_at = Instant::now().checked_add(ttl);
      self
         .cache
         .lock()
         .map_err(|_| crate::Error::Custom("session cache is unavailable".to_string()))?
         .insert(key, value, expires_at);
      Ok(())
   }

   fn complete_value(slot: &BuildSlot<V>, value: Arc<V>) -> crate::Result<Arc<V>> {
      slot.complete(Arc::clone(&value))?;
      Ok(value)
   }

   pub(crate) async fn get_or_try_build<F, Fut>(
      &self,
      key: K,
      ttl: Duration,
      build: F,
   ) -> crate::Result<Arc<V>>
   where
      F: FnOnce() -> Fut,
      Fut: Future<Output = crate::Result<V>>,
   {
      self
         .expiration_reaper
         .start(Arc::clone(&self.cache), self.reaper_interval);
      if let Some(value) = self.cached(&key)? {
         return Ok(value);
      }

      let mut build_lock = self.build_lock(&key)?;
      let build_guard = build_lock.slot.gate.lock().await;
      let result = match build_lock.slot.result() {
         Ok(Some(value)) => Ok(value),
         Ok(None) => match self.cached(&key) {
            Ok(Some(value)) => Self::complete_value(&build_lock.slot, value),
            Ok(None) => match build().await {
               Ok(value) => {
                  let value = Arc::new(value);
                  match self.insert_cached(key.clone(), Arc::clone(&value), ttl) {
                     Ok(()) => Self::complete_value(&build_lock.slot, value),
                     Err(error) => Err(error),
                  }
               }
               Err(error) => Err(error),
            },
            Err(error) => Err(error),
         },
         Err(error) => Err(error),
      };

      drop(build_guard);
      build_lock.cleanup()?;
      result
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use std::sync::Arc;
   use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
   use std::time::Duration;

   #[test]
   fn session_cache_evicts_the_least_recently_used_entry() {
      let now = Instant::now();
      let mut cache = SessionCache::new(2);
      cache.insert("first".to_string(), 1, None);
      cache.insert("second".to_string(), 2, None);

      assert_eq!(cache.get(&"first".to_string(), now), Some(1));
      cache.insert("third".to_string(), 3, None);

      assert_eq!(cache.get(&"second".to_string(), now), None);
      assert_eq!(cache.get(&"first".to_string(), now), Some(1));
      assert_eq!(cache.get(&"third".to_string(), now), Some(3));
   }

   #[test]
   fn session_cache_drops_an_entry_at_its_expiration_deadline() {
      let now = Instant::now();
      let deadline = now + Duration::from_secs(30);
      let mut cache = SessionCache::new(1);
      cache.insert("remote".to_string(), 1, Some(deadline));

      assert_eq!(cache.get(&"remote".to_string(), now), Some(1));
      assert_eq!(cache.get(&"remote".to_string(), deadline), None);
   }

   #[tokio::test]
   async fn session_pool_preserves_lru_order_and_expires_entries() {
      let pool = SessionPool::new(2, Duration::from_secs(60));
      let builds = AtomicUsize::new(0);

      for key in ["first", "second"] {
         pool
            .get_or_try_build(key, Duration::from_secs(60), || async {
               builds.fetch_add(1, Ordering::SeqCst);
               Ok(key)
            })
            .await
            .expect("initial values should build");
      }
      pool
         .get_or_try_build("first", Duration::from_secs(60), || async {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected rebuild")
         })
         .await
         .expect("the first value should be cached");
      pool
         .get_or_try_build("third", Duration::from_secs(60), || async {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok("third")
         })
         .await
         .expect("the third value should build");
      pool
         .get_or_try_build("second", Duration::ZERO, || async {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok("second rebuilt")
         })
         .await
         .expect("the evicted second value should rebuild");
      pool
         .get_or_try_build("second", Duration::from_secs(60), || async {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok("second after expiry")
         })
         .await
         .expect("the zero-TTL value should expire immediately");

      assert_eq!(builds.load(Ordering::SeqCst), 5);
   }

   #[tokio::test]
   async fn session_pool_reaper_releases_expired_values_without_cache_access() {
      let pool = SessionPool::new(1, Duration::from_millis(2));
      let value = pool
         .get_or_try_build("local", Duration::from_millis(10), || async { Ok(()) })
         .await
         .expect("value should build");
      let weak = Arc::downgrade(&value);
      drop(value);

      tokio::time::timeout(Duration::from_secs(1), async {
         while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
         }
      })
      .await
      .expect("the lazy reaper should release the expired value");
   }

   #[tokio::test]
   async fn concurrent_cold_requests_share_exactly_one_fallible_build() {
      let pool = Arc::new(SessionPool::new(2, Duration::from_secs(60)));
      let builds = Arc::new(AtomicUsize::new(0));

      let request = |pool: Arc<SessionPool<&'static str, usize>>, builds: Arc<AtomicUsize>| async move {
         pool
            .get_or_try_build("shared", Duration::from_secs(60), || async move {
               builds.fetch_add(1, Ordering::SeqCst);
               tokio::time::sleep(Duration::from_millis(20)).await;
               Ok(42)
            })
            .await
      };
      let (first, second, third) = tokio::join!(
         request(Arc::clone(&pool), Arc::clone(&builds)),
         request(Arc::clone(&pool), Arc::clone(&builds)),
         request(Arc::clone(&pool), Arc::clone(&builds)),
      );
      let first = first.expect("first request should build");
      let second = second.expect("second request should share the build");
      let third = third.expect("third request should share the build");

      assert_eq!(builds.load(Ordering::SeqCst), 1);
      assert!(Arc::ptr_eq(&first, &second));
      assert!(Arc::ptr_eq(&first, &third));
   }

   #[tokio::test]
   async fn failed_build_leaves_no_entry_or_retained_build_lock() {
      let pool = SessionPool::<&str, usize>::new(1, Duration::from_secs(60));

      let failed = pool
         .get_or_try_build("shared", Duration::from_secs(60), || async {
            Err(crate::Error::Custom("expected failure".to_string()))
         })
         .await;

      assert!(failed.is_err());
      assert!(
         pool
            .cache
            .lock()
            .expect("cache should remain available")
            .get(&"shared", Instant::now())
            .is_none()
      );
      assert!(
         pool
            .build_locks
            .lock()
            .expect("build-lock table should remain available")
            .is_empty()
      );

      let recovered = pool
         .get_or_try_build("shared", Duration::from_secs(60), || async { Ok(7) })
         .await
         .expect("a later build should recover");
      assert_eq!(*recovered, 7);
   }

   #[tokio::test]
   async fn concurrent_leader_failure_is_retried_and_success_is_shared() {
      const REQUESTS: usize = 8;
      let pool = Arc::new(SessionPool::<&str, usize>::new(1, Duration::from_secs(60)));
      let builds = Arc::new(AtomicUsize::new(0));
      let release_failure = Arc::new(AtomicBool::new(false));
      let mut requests = Vec::new();

      for _ in 0..REQUESTS {
         let request_pool = Arc::clone(&pool);
         let request_builds = Arc::clone(&builds);
         let request_release = Arc::clone(&release_failure);
         requests.push(tokio::spawn(async move {
            request_pool
               .get_or_try_build("shared", Duration::from_secs(60), || async move {
                  let attempt = request_builds.fetch_add(1, Ordering::SeqCst);
                  while !request_release.load(Ordering::SeqCst) {
                     tokio::task::yield_now().await;
                  }
                  if attempt == 0 {
                     Err(crate::Error::Custom("leader failure".to_string()))
                  } else {
                     Ok(9)
                  }
               })
               .await
         }));
      }

      tokio::time::timeout(Duration::from_secs(1), async {
         loop {
            let joined = pool
               .build_locks
               .lock()
               .expect("build-lock table should remain available")
               .values()
               .next()
               .map_or(0, |entry| entry.leases);
            if joined == REQUESTS {
               break;
            }
            tokio::task::yield_now().await;
         }
      })
      .await
      .expect("all requests should join the same in-flight build");
      release_failure.store(true, Ordering::SeqCst);

      let mut failure_count = 0;
      let mut successes = Vec::new();
      for request in requests {
         match request.await.expect("request task should complete") {
            Ok(value) => successes.push(value),
            Err(error) => {
               failure_count += 1;
               assert_eq!(error.to_string(), "leader failure");
            }
         }
      }
      assert_eq!(failure_count, 1, "only the build leader should fail");
      assert_eq!(successes.len(), REQUESTS - 1);
      assert_eq!(builds.load(Ordering::SeqCst), 2);
      assert!(successes.iter().all(|value| **value == 9));
      assert!(
         successes
            .windows(2)
            .all(|pair| Arc::ptr_eq(&pair[0], &pair[1])),
         "the successor's successful build should be shared"
      );
      assert!(
         pool
            .cache
            .lock()
            .expect("cache should remain available")
            .get(&"shared", Instant::now())
            .is_some()
      );
      assert!(
         pool
            .build_locks
            .lock()
            .expect("build-lock table should remain available")
            .is_empty()
      );
   }

   #[test]
   fn overlapping_lease_cleanup_immediately_removes_the_matching_weak_entry() {
      let pool = SessionPool::<&str, usize>::new(1, Duration::from_secs(60));
      let mut first = pool.build_lock(&"shared").expect("first lease");
      let mut second = pool.build_lock(&"shared").expect("second lease");

      first.cleanup().expect("first cleanup");
      second.cleanup().expect("second cleanup");

      assert!(
         pool
            .build_locks
            .lock()
            .expect("build-lock table should remain available")
            .is_empty(),
         "the final overlapping cleanup must remove the exact matching entry"
      );
   }

   #[test]
   fn stale_lease_cleanup_never_removes_a_newer_slot_for_the_same_key() {
      let pool = SessionPool::<&str, usize>::new(1, Duration::from_secs(60));
      let mut stale = pool.build_lock(&"shared").expect("stale lease");
      let newer_slot = Arc::new(BuildSlot::new());
      pool
         .build_locks
         .lock()
         .expect("build-lock table should remain available")
         .insert(
            "shared",
            BuildEntry {
               slot: Arc::downgrade(&newer_slot),
               leases: 1,
            },
         );
      let mut newer = BuildLockLease {
         table: &pool.build_locks,
         key: "shared",
         slot: Arc::clone(&newer_slot),
         cleaned: false,
      };

      stale.cleanup().expect("stale cleanup");

      let locks = pool
         .build_locks
         .lock()
         .expect("build-lock table should remain available");
      let stored = locks.get("shared").expect("newer slot must remain");
      assert!(stored.slot.ptr_eq(&Arc::downgrade(&newer_slot)));
      assert_eq!(stored.leases, 1);
      drop(locks);

      newer.cleanup().expect("newer cleanup");
      assert!(
         pool
            .build_locks
            .lock()
            .expect("build-lock table should remain available")
            .is_empty()
      );
   }

   #[tokio::test]
   async fn cancelled_build_does_not_leave_a_dead_weak_lock_entry() {
      let pool = Arc::new(SessionPool::<&str, usize>::new(1, Duration::from_secs(60)));
      let started = Arc::new(AtomicUsize::new(0));
      let task_pool = Arc::clone(&pool);
      let task_started = Arc::clone(&started);
      let task = tokio::spawn(async move {
         task_pool
            .get_or_try_build("shared", Duration::from_secs(60), || async move {
               task_started.store(1, Ordering::SeqCst);
               std::future::pending::<crate::Result<usize>>().await
            })
            .await
      });

      tokio::time::timeout(Duration::from_secs(1), async {
         while started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
         }
      })
      .await
      .expect("the cancellable build should start");
      task.abort();
      assert!(
         task
            .await
            .expect_err("the build should be cancelled")
            .is_cancelled()
      );

      assert!(
         pool
            .build_locks
            .lock()
            .expect("build-lock table should remain available")
            .is_empty(),
         "cancellation must not accumulate dead weak entries"
      );
   }
}
