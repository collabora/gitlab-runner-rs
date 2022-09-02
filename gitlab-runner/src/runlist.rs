use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;
use tokio::sync::watch;

#[derive(Debug)]
pub(crate) struct RunListEntry<K, V>
where
    K: Eq + std::hash::Hash,
{
    owner: Arc<Inner<K, V>>,
    key: K,
    value: V,
}

impl<K, V> std::ops::Deref for RunListEntry<K, V>
where
    K: Eq + std::hash::Hash,
{
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<K, V> Drop for RunListEntry<K, V>
where
    K: Eq + std::hash::Hash,
{
    fn drop(&mut self) {
        self.owner.remove(&self.key);
    }
}

#[derive(Debug)]
struct Inner<K, V> {
    rundata: RwLock<HashMap<K, V>>,
    changed: watch::Sender<usize>,
}

impl<K, V> Inner<K, V>
where
    K: Eq + std::hash::Hash,
{
    fn remove(&self, key: &K) {
        let mut r = self.rundata.write();
        r.remove(key);
        self.changed.send_replace(r.len());
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RunList<K, V> {
    inner: Arc<Inner<K, V>>,
    size: watch::Receiver<usize>,
}

impl<K, V> RunList<K, V>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    pub(crate) fn new() -> Self {
        let (tx, rx) = watch::channel(0);
        RunList {
            inner: Arc::new(Inner {
                rundata: RwLock::new(HashMap::new()),
                changed: tx,
            }),
            size: rx,
        }
    }

    pub(crate) async fn wait_for_space(&mut self, max: usize) {
        loop {
            if *self.size.borrow_and_update() < max {
                break;
            }
            // Returns error if sender is dropped, but sender is owned by self
            let _ = self.size.changed().await;
        }
    }

    pub(crate) fn size(&self) -> usize {
        *self.size.borrow()
    }

    pub(crate) fn insert(&mut self, id: K, data: V) -> RunListEntry<K, V> {
        let mut r = self.inner.rundata.write();
        r.insert(id.clone(), data.clone());
        self.inner.changed.send_replace(r.len());
        RunListEntry {
            owner: self.inner.clone(),
            key: id,
            value: data,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn lookup(&self, id: &K) -> Option<V> {
        let r = self.inner.rundata.read();
        r.get(id).cloned()
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use tokio::time::sleep;

    use super::*;

    #[tokio::test]
    async fn runlist() {
        let mut runlist = RunList::new();

        let v = runlist.insert(1, 2);
        // Should return straight away
        runlist.wait_for_space(2).await;
        let mut r_clone = runlist.clone();

        let join = tokio::spawn(async move {
            r_clone.wait_for_space(1).await;
            assert_eq!(r_clone.size(), 0)
        });

        assert_eq!(runlist.size(), 1);
        assert_eq!(*v, 2);
        assert_eq!(runlist.lookup(&1), Some(2));

        sleep(Duration::from_millis(100)).await;
        drop(v);

        assert_eq!(runlist.size(), 0);
        assert_eq!(runlist.lookup(&1), None);
        join.await.unwrap();
    }
}
