//! A persistent (structurally shared) radix tree keyed by `u64`.
//!
//! Cloning a [`Trie`] is O(1): it copies one `Arc`. Mutating a clone copies
//! only the nodes on the path to the changed key (64-way fanout, so at most
//! 11 nodes), and only if those nodes are still shared. This is what makes
//! branch creation constant-time and keeps lookups independent of how many
//! times a branch has been forked: there is no chain of deltas to walk.

use std::collections::HashSet;
use std::sync::Arc;

const BITS: u32 = 6;
const FANOUT: usize = 1 << BITS;
const MASK: u64 = (FANOUT as u64) - 1;

// Nodes always live behind an `Arc`, so the size difference between the
// variants costs nothing extra per reference.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum Node<V: Clone> {
    Inner([Option<Arc<Node<V>>>; FANOUT]),
    Leaf([Option<V>; FANOUT]),
}

impl<V: Clone> Node<V> {
    fn new(level: u32) -> Self {
        if level == 0 {
            Node::Leaf(std::array::from_fn(|_| None))
        } else {
            Node::Inner(std::array::from_fn(|_| None))
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Node::Inner(c) => c.iter().all(Option::is_none),
            Node::Leaf(v) => v.iter().all(Option::is_none),
        }
    }
}

/// Persistent map from `u64` to `V`.
#[derive(Clone)]
pub struct Trie<V: Clone> {
    root: Option<Arc<Node<V>>>,
    /// Number of inner levels above the leaf level.
    height: u32,
}

impl<V: Clone> Default for Trie<V> {
    fn default() -> Self {
        Self::new()
    }
}

fn fits(key: u64, height: u32) -> bool {
    let bits = BITS * (height + 1);
    bits >= 64 || key >> bits == 0
}

fn index(key: u64, level: u32) -> usize {
    ((key >> (BITS * level)) & MASK) as usize
}

impl<V: Clone> Trie<V> {
    pub fn new() -> Self {
        Trie { root: None, height: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn get(&self, key: u64) -> Option<&V> {
        if !fits(key, self.height) {
            return None;
        }
        let mut node = self.root.as_ref()?;
        let mut level = self.height;
        loop {
            match &**node {
                Node::Inner(c) => {
                    node = c[index(key, level)].as_ref()?;
                    level -= 1;
                }
                Node::Leaf(v) => return v[index(key, 0)].as_ref(),
            }
        }
    }

    /// Mutable access to an existing value, copying shared nodes on the way.
    pub fn get_mut(&mut self, key: u64) -> Option<&mut V> {
        self.get(key)?;
        let mut node = Arc::make_mut(self.root.as_mut()?);
        let mut level = self.height;
        loop {
            match node {
                Node::Inner(c) => {
                    node = Arc::make_mut(c[index(key, level)].as_mut()?);
                    level -= 1;
                }
                Node::Leaf(v) => return v[index(key, 0)].as_mut(),
            }
        }
    }

    pub fn insert(&mut self, key: u64, value: V) -> Option<V> {
        while !fits(key, self.height) {
            if let Some(old) = self.root.take() {
                let mut inner = Node::new(self.height + 1);
                if let Node::Inner(c) = &mut inner {
                    c[0] = Some(old);
                }
                self.root = Some(Arc::new(inner));
            }
            self.height += 1;
        }
        fn rec<V: Clone>(slot: &mut Option<Arc<Node<V>>>, level: u32, key: u64, value: V) -> Option<V> {
            let node = Arc::make_mut(slot.get_or_insert_with(|| Arc::new(Node::new(level))));
            match node {
                Node::Leaf(v) => v[index(key, 0)].replace(value),
                Node::Inner(c) => rec(&mut c[index(key, level)], level - 1, key, value),
            }
        }
        rec(&mut self.root, self.height, key, value)
    }

    pub fn remove(&mut self, key: u64) -> Option<V> {
        self.get(key)?;
        fn rec<V: Clone>(slot: &mut Option<Arc<Node<V>>>, level: u32, key: u64) -> Option<V> {
            let node = Arc::make_mut(slot.as_mut()?);
            let out = match node {
                Node::Leaf(v) => v[index(key, 0)].take(),
                Node::Inner(c) => rec(&mut c[index(key, level)], level - 1, key),
            };
            if node.is_empty() {
                *slot = None;
            }
            out
        }
        rec(&mut self.root, self.height, key)
    }

    /// Remove every key `>= start`.
    pub fn remove_from(&mut self, start: u64) {
        if !fits(start, self.height) {
            return;
        }
        fn rec<V: Clone>(slot: &mut Option<Arc<Node<V>>>, level: u32, start: u64) {
            let Some(arc) = slot.as_mut() else { return };
            let node = Arc::make_mut(arc);
            let at = index(start, level);
            match node {
                Node::Leaf(v) => {
                    for e in &mut v[at..] {
                        *e = None;
                    }
                }
                Node::Inner(c) => {
                    for e in &mut c[at + 1..] {
                        *e = None;
                    }
                    // Keys below `start` inside child `at` must survive.
                    let low_bits = start & ((1u64 << (BITS * level)) - 1);
                    if low_bits == 0 {
                        c[at] = None;
                    } else {
                        rec(&mut c[at], level - 1, start);
                    }
                }
            }
            if node.is_empty() {
                *slot = None;
            }
        }
        rec(&mut self.root, self.height, start);
    }

    /// Visit every `(key, value)` in ascending key order.
    pub fn for_each(&self, f: &mut dyn FnMut(u64, &V)) {
        fn rec<V: Clone>(node: &Node<V>, level: u32, base: u64, f: &mut dyn FnMut(u64, &V)) {
            match node {
                Node::Leaf(v) => {
                    for (i, e) in v.iter().enumerate() {
                        if let Some(e) = e {
                            f(base | i as u64, e);
                        }
                    }
                }
                Node::Inner(c) => {
                    for (i, e) in c.iter().enumerate() {
                        if let Some(e) = e {
                            rec(e, level - 1, base | ((i as u64) << (BITS * level)), f);
                        }
                    }
                }
            }
        }
        if let Some(root) = &self.root {
            rec(root, self.height, 0, f);
        }
    }

    /// Visit every value reachable through nodes that have not been seen
    /// before. `seen` holds node addresses, so subtrees shared between many
    /// tries (branches) are walked once. This keeps garbage collection
    /// proportional to the data actually stored, not to branches x size.
    pub fn walk_unique(&self, seen: &mut HashSet<usize>, f: &mut dyn FnMut(&V)) {
        fn rec<V: Clone>(arc: &Arc<Node<V>>, seen: &mut HashSet<usize>, f: &mut dyn FnMut(&V)) {
            if !seen.insert(Arc::as_ptr(arc) as usize) {
                return;
            }
            match &**arc {
                Node::Leaf(v) => v.iter().flatten().for_each(&mut *f),
                Node::Inner(c) => c.iter().flatten().for_each(|e| rec(e, seen, f)),
            }
        }
        if let Some(root) = &self.root {
            rec(root, seen, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn collect(t: &Trie<u64>) -> BTreeMap<u64, u64> {
        let mut m = BTreeMap::new();
        t.for_each(&mut |k, v| {
            m.insert(k, *v);
        });
        m
    }

    #[test]
    fn insert_get_remove() {
        let mut t = Trie::new();
        assert!(t.get(0).is_none());
        for k in [0u64, 1, 63, 64, 4095, 4096, 1 << 30, u64::MAX] {
            assert_eq!(t.insert(k, k.wrapping_add(1)), None);
        }
        for k in [0u64, 1, 63, 64, 4095, 4096, 1 << 30, u64::MAX] {
            assert_eq!(t.get(k), Some(&(k.wrapping_add(1))));
        }
        assert_eq!(t.insert(64, 7), Some(65));
        assert_eq!(t.remove(64), Some(7));
        assert_eq!(t.remove(64), None);
        assert!(t.get(65).is_none());
    }

    #[test]
    fn clone_is_isolated() {
        let mut a = Trie::new();
        for k in 0..1000u64 {
            a.insert(k * 7, k);
        }
        let mut b = a.clone();
        b.insert(7, 999);
        b.remove(14);
        *b.get_mut(21).unwrap() = 5;
        assert_eq!(a.get(7), Some(&1));
        assert_eq!(a.get(14), Some(&2));
        assert_eq!(a.get(21), Some(&3));
        assert_eq!(b.get(7), Some(&999));
        assert_eq!(b.get(14), None);
        assert_eq!(b.get(21), Some(&5));
    }

    #[test]
    fn remove_from_matches_model() {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let mut t = Trie::new();
            let mut m = BTreeMap::new();
            for _ in 0..300 {
                let k = match next() % 3 {
                    0 => next() % 200,
                    1 => next() % 70_000,
                    _ => next() % (1 << 40),
                };
                t.insert(k, k);
                m.insert(k, k);
            }
            let cut = match next() % 4 {
                0 => 0,
                1 => next() % 200,
                2 => (next() % 1100) * 64,
                _ => next() % (1 << 40),
            };
            let snapshot = t.clone();
            t.remove_from(cut);
            m.retain(|k, _| *k < cut);
            assert_eq!(collect(&t), m);
            // The snapshot taken before truncation is untouched.
            assert!(collect(&snapshot).len() >= m.len());
            if m.is_empty() {
                assert!(t.is_empty());
            }
        }
    }

    #[test]
    fn walk_unique_skips_shared_nodes() {
        let mut a = Trie::new();
        for k in 0..10_000u64 {
            a.insert(k, k);
        }
        let mut b = a.clone();
        b.insert(5, 500);
        let mut seen = HashSet::new();
        let (mut na, mut nb) = (0, 0);
        a.walk_unique(&mut seen, &mut |_| na += 1);
        b.walk_unique(&mut seen, &mut |_| nb += 1);
        assert_eq!(na, 10_000);
        // Only the one copied leaf (64 values) is new in `b`.
        assert_eq!(nb, 64);
    }
}
