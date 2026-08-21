use std::time::Instant;

pub(crate) struct SessionCache<K, V> {
   capacity: usize,
   entries: Vec<SessionCacheEntry<K, V>>,
}

struct SessionCacheEntry<K, V> {
   key: K,
   value: V,
   expires_at: Option<Instant>,
}

impl<K: PartialEq, V: Clone> SessionCache<K, V> {
   pub(crate) fn new(capacity: usize) -> Self {
      Self {
         capacity: capacity.max(1),
         entries: Vec::new(),
      }
   }

   pub(crate) fn remove_expired(&mut self, now: Instant) {
      self
         .entries
         .retain(|entry| entry.expires_at.is_none_or(|deadline| deadline > now));
   }

   pub(crate) fn get(&mut self, key: &K, now: Instant) -> Option<V> {
      self.remove_expired(now);
      let index = self.entries.iter().position(|entry| &entry.key == key)?;
      let entry = self.entries.remove(index);
      let value = entry.value.clone();
      self.entries.push(entry);
      Some(value)
   }

   pub(crate) fn insert(&mut self, key: K, value: V, expires_at: Option<Instant>) {
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

#[cfg(test)]
mod tests {
   use super::*;
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
}
