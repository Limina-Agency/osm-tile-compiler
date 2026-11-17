//! Iteration order of Java 21's default `HashMap`.
//!
//! Housenumber postprocessing iterates `.values()` after grouping by its
//! temporary partition. The first selected item for each partition then
//! becomes the representative ID when equal house numbers merge. This helper
//! returns the first input index of each partition in Java map iteration
//! order. OSM tag maps and the boundary layer's region sets are filled with
//! `put`/`add` instead, which append to a bin rather than prepend.
//!
//! A bin that reaches eight entries in a table of 64 or more becomes a
//! red-black tree. Iteration still follows the bin's linked list, but tree
//! insertion links a new node after its tree parent and moves the root to the
//! front, so the order depends on the tree's shape. [`JavaHashMap`] mirrors
//! JDK 21's `HashMap` and `HashMap.TreeNode` code for insertion and resizing;
//! removal is never used here.

use std::cmp::Ordering;

const TREEIFY_THRESHOLD: usize = 8;
const UNTREEIFY_THRESHOLD: usize = 6;
const MIN_TREEIFY_CAPACITY: usize = 64;

/// `String.hashCode`, spread like `HashMap.hash`.
fn java_spread_hash(value: &str) -> i32 {
    let hash = value.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(u32::from(unit))
    });
    (hash ^ (hash >> 16)) as i32
}

/// `String.compareTo`: lexicographic over UTF-16 code units.
fn java_string_compare(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Long.hashCode`, spread like `HashMap.hash`.
fn java_long_spread_hash(value: i64) -> i32 {
    let hash = (value ^ ((value as u64) >> 32) as i64) as u32;
    (hash ^ (hash >> 16)) as i32
}

/// Return indices of first occurrences in the order Java 21's default
/// `HashMap.values()` would visit their partitions after `computeIfAbsent`,
/// as `Collectors.groupingBy` fills it.
pub(super) fn grouping_by_partition_order(partitions: &[String]) -> Vec<usize> {
    let mut map = JavaHashMap::new(
        partitions,
        |key: &String| java_spread_hash(key),
        |a: &String, b: &String| java_string_compare(a, b),
        0,
    );
    for index in 0..partitions.len() {
        map.compute_if_absent(index);
    }
    map.iteration_order()
}

/// [`grouping_by_partition_order`] for `Long` keys.
pub(super) fn grouping_by_long_order(keys: &[i64]) -> Vec<usize> {
    let mut map = JavaHashMap::new(keys, |key| java_long_spread_hash(*key), i64::cmp, 0);
    for index in 0..keys.len() {
        map.compute_if_absent(index);
    }
    map.iteration_order()
}

/// Return indices of `keys` in the order Java 21 iterates a
/// `HashMap.newHashMap(keys.len())` after one `put` per key, as Planetiler's
/// PBF decoder builds each element's tags. Keys must be distinct.
pub(super) fn new_hash_map_put_order(keys: &[&str]) -> Vec<usize> {
    // HashMap.calculateHashMapCapacity; tableSizeFor runs on the first put.
    let capacity = (keys.len() as f64 / 0.75).ceil().max(1.0) as usize;
    let mut map = JavaHashMap::new(
        keys,
        |key: &&str| java_spread_hash(key),
        |a: &&str, b: &&str| java_string_compare(a, b),
        capacity.next_power_of_two(),
    );
    for index in 0..keys.len() {
        map.put(index);
    }
    map.iteration_order()
}

/// Return indices of first occurrences in the order Java 21 iterates a
/// default `HashSet<Long>` after one `add` per key.
pub(super) fn hash_set_long_order(keys: &[i64]) -> Vec<usize> {
    let mut map = JavaHashMap::new(keys, |key| java_long_spread_hash(*key), i64::cmp, 0);
    for index in 0..keys.len() {
        map.put(index);
    }
    map.iteration_order()
}

#[derive(Clone, Default)]
struct Node {
    hash: i32,
    /// Index of the key that created this entry.
    key: usize,
    next: Option<usize>,
    // TreeNode fields; meaningful while `tree` is set.
    tree: bool,
    prev: Option<usize>,
    parent: Option<usize>,
    left: Option<usize>,
    right: Option<usize>,
    red: bool,
}

/// The key set of a Java 21 `HashMap` with default load factor. Entries are
/// arena indices; a key's identity is its first index in `keys`.
struct JavaHashMap<'a, K, C> {
    keys: &'a [K],
    compare: C,
    hashes: Vec<i32>,
    table: Vec<Option<usize>>,
    nodes: Vec<Node>,
    size: usize,
    /// Before the first resize, the initial capacity (0 for the default).
    threshold: usize,
}

impl<'a, K, C> JavaHashMap<'a, K, C>
where
    C: Fn(&K, &K) -> Ordering,
{
    /// `initial_capacity` is a power of two, or 0 for `new HashMap<>()`.
    fn new(keys: &'a [K], hash: impl Fn(&K) -> i32, compare: C, initial_capacity: usize) -> Self {
        Self {
            keys,
            hashes: keys.iter().map(hash).collect(),
            compare,
            table: Vec::new(),
            nodes: Vec::new(),
            size: 0,
            threshold: initial_capacity,
        }
    }

    fn same_key(&self, node: usize, hash: i32, key: usize) -> bool {
        let n = &self.nodes[node];
        n.hash == hash && (self.compare)(&self.keys[n.key], &self.keys[key]) == Ordering::Equal
    }

    fn slot(&self, hash: i32) -> usize {
        hash as u32 as usize & (self.table.len() - 1)
    }

    fn new_node(&mut self, hash: i32, key: usize, next: Option<usize>) -> usize {
        self.nodes.push(Node {
            hash,
            key,
            next,
            ..Node::default()
        });
        self.nodes.len() - 1
    }

    /// `HashMap.computeIfAbsent` with a function that never returns null.
    fn compute_if_absent(&mut self, key: usize) {
        let hash = self.hashes[key];
        if self.size > self.threshold || self.table.is_empty() {
            self.resize();
        }
        let i = self.slot(hash);
        let first = self.table[i];
        let mut bin_count = 0;
        let mut tree = None;
        if let Some(first) = first {
            if self.nodes[first].tree {
                tree = Some(first);
                if self.get_tree_node(first, hash, key).is_some() {
                    return;
                }
            } else {
                let mut e = Some(first);
                while let Some(node) = e {
                    if self.same_key(node, hash, key) {
                        return;
                    }
                    bin_count += 1;
                    e = self.nodes[node].next;
                }
            }
        }
        if let Some(first) = tree {
            self.put_tree_val(first, hash, key);
        } else {
            let node = self.new_node(hash, key, first);
            self.table[i] = Some(node);
            if bin_count >= TREEIFY_THRESHOLD - 1 {
                self.treeify_bin(hash);
            }
        }
        self.size += 1;
    }

    /// `HashMap.putVal`.
    fn put(&mut self, key: usize) {
        let hash = self.hashes[key];
        if self.table.is_empty() {
            self.resize();
        }
        let i = self.slot(hash);
        match self.table[i] {
            None => {
                let node = self.new_node(hash, key, None);
                self.table[i] = Some(node);
            }
            Some(p) if self.same_key(p, hash, key) => return,
            Some(p) if self.nodes[p].tree => {
                if self.put_tree_val(p, hash, key).is_some() {
                    return;
                }
            }
            Some(mut p) => {
                let mut bin_count = 0;
                loop {
                    let Some(e) = self.nodes[p].next else {
                        let node = self.new_node(hash, key, None);
                        self.nodes[p].next = Some(node);
                        if bin_count >= TREEIFY_THRESHOLD - 1 {
                            self.treeify_bin(hash);
                        }
                        break;
                    };
                    if self.same_key(e, hash, key) {
                        return;
                    }
                    p = e;
                    bin_count += 1;
                }
            }
        }
        self.size += 1;
        if self.size > self.threshold {
            self.resize();
        }
    }

    fn resize(&mut self) {
        let old_cap = self.table.len();
        let new_cap = if old_cap > 0 {
            old_cap * 2
        } else if self.threshold > 0 {
            self.threshold
        } else {
            16
        };
        // (int) (newCap * 0.75f), or the doubled threshold, which agree.
        self.threshold = new_cap * 3 / 4;
        let old = std::mem::replace(&mut self.table, vec![None; new_cap]);
        for (j, head) in old.into_iter().enumerate() {
            let Some(e) = head else { continue };
            if self.nodes[e].next.is_none() {
                let slot = self.slot(self.nodes[e].hash);
                self.table[slot] = Some(e);
            } else if self.nodes[e].tree {
                self.split(e, j, old_cap);
            } else {
                let (mut lo, mut hi) = (vec![], vec![]);
                let mut next = Some(e);
                while let Some(node) = next {
                    next = self.nodes[node].next;
                    if self.nodes[node].hash as u32 as usize & old_cap == 0 {
                        lo.push(node);
                    } else {
                        hi.push(node);
                    }
                }
                self.table[j] = self.link(&lo);
                self.table[j + old_cap] = self.link(&hi);
            }
        }
    }

    /// Chain `nodes` through `next` and return the head.
    fn link(&mut self, nodes: &[usize]) -> Option<usize> {
        for pair in nodes.windows(2) {
            self.nodes[pair[0]].next = Some(pair[1]);
        }
        if let Some(&last) = nodes.last() {
            self.nodes[last].next = None;
        }
        nodes.first().copied()
    }

    fn treeify_bin(&mut self, hash: i32) {
        if self.table.len() < MIN_TREEIFY_CAPACITY {
            self.resize();
            return;
        }
        let index = self.slot(hash);
        let Some(head) = self.table[index] else {
            return;
        };
        // replacementTreeNode copies each entry in list order.
        let mut prev = None;
        let mut e = Some(head);
        while let Some(node) = e {
            e = self.nodes[node].next;
            let n = &mut self.nodes[node];
            n.tree = true;
            n.prev = prev;
            n.parent = None;
            n.left = None;
            n.right = None;
            n.red = false;
            prev = Some(node);
        }
        self.treeify(head);
    }

    /// Return the root reached by following parents.
    fn root(&self, mut node: usize) -> usize {
        while let Some(parent) = self.nodes[node].parent {
            node = parent;
        }
        node
    }

    /// Ordering of an insertion against a tree node: hash, then compareTo.
    /// Distinct keys of one type never compare equal, so Java's
    /// identity-hash tie break is unreachable.
    fn direction(&self, hash: i32, key: usize, node: usize) -> Ordering {
        let n = &self.nodes[node];
        hash.cmp(&n.hash)
            .then_with(|| (self.compare)(&self.keys[key], &self.keys[n.key]))
    }

    fn treeify(&mut self, head: usize) {
        let mut root: Option<usize> = None;
        let mut next = Some(head);
        while let Some(x) = next {
            next = self.nodes[x].next;
            self.nodes[x].left = None;
            self.nodes[x].right = None;
            let Some(r) = root else {
                self.nodes[x].parent = None;
                self.nodes[x].red = false;
                root = Some(x);
                continue;
            };
            let (hash, key) = (self.nodes[x].hash, self.nodes[x].key);
            let mut p = r;
            loop {
                let less = self.direction(hash, key, p) != Ordering::Greater;
                let child = if less {
                    self.nodes[p].left
                } else {
                    self.nodes[p].right
                };
                match child {
                    Some(c) => p = c,
                    None => {
                        self.nodes[x].parent = Some(p);
                        if less {
                            self.nodes[p].left = Some(x);
                        } else {
                            self.nodes[p].right = Some(x);
                        }
                        root = Some(self.balance_insertion(r, x));
                        break;
                    }
                }
            }
        }
        if let Some(root) = root {
            self.move_root_to_front(root);
        }
    }

    fn untreeify(&mut self, head: usize) {
        let mut e = Some(head);
        while let Some(node) = e {
            e = self.nodes[node].next;
            let n = &mut self.nodes[node];
            n.tree = false;
            n.prev = None;
            n.parent = None;
            n.left = None;
            n.right = None;
            n.red = false;
        }
    }

    fn split(&mut self, head: usize, index: usize, bit: usize) {
        let (mut lo, mut hi) = (vec![], vec![]);
        let mut next = Some(head);
        while let Some(e) = next {
            next = self.nodes[e].next;
            if self.nodes[e].hash as u32 as usize & bit == 0 {
                lo.push(e);
            } else {
                hi.push(e);
            }
        }
        for list in [&lo, &hi] {
            let mut prev = None;
            for &e in list.iter() {
                self.nodes[e].prev = prev;
                prev = Some(e);
            }
        }
        let lo_head = self.link(&lo);
        let hi_head = self.link(&hi);
        self.table[index] = lo_head;
        self.table[index + bit] = hi_head;
        if let Some(lo_head) = lo_head {
            if lo.len() <= UNTREEIFY_THRESHOLD {
                self.untreeify(lo_head);
            } else if hi_head.is_some() {
                self.treeify(lo_head);
            }
        }
        if let Some(hi_head) = hi_head {
            if hi.len() <= UNTREEIFY_THRESHOLD {
                self.untreeify(hi_head);
            } else if lo_head.is_some() {
                self.treeify(hi_head);
            }
        }
    }

    fn move_root_to_front(&mut self, root: usize) {
        let index = self.slot(self.nodes[root].hash);
        let first = self.table[index];
        if first == Some(root) {
            return;
        }
        self.table[index] = Some(root);
        let rp = self.nodes[root].prev;
        let rn = self.nodes[root].next;
        if let Some(rn) = rn {
            self.nodes[rn].prev = rp;
        }
        if let Some(rp) = rp {
            self.nodes[rp].next = rn;
        }
        if let Some(first) = first {
            self.nodes[first].prev = Some(root);
        }
        self.nodes[root].next = first;
        self.nodes[root].prev = None;
    }

    fn find(&self, start: usize, hash: i32, key: usize) -> Option<usize> {
        let mut p = Some(start);
        while let Some(node) = p {
            let n = &self.nodes[node];
            let (pl, pr) = (n.left, n.right);
            if n.hash > hash {
                p = pl;
            } else if n.hash < hash {
                p = pr;
            } else if self.same_key(node, hash, key) {
                return Some(node);
            } else if pl.is_none() {
                p = pr;
            } else if pr.is_none() {
                p = pl;
            } else {
                match (self.compare)(&self.keys[key], &self.keys[n.key]) {
                    Ordering::Less => p = pl,
                    Ordering::Greater => p = pr,
                    Ordering::Equal => {
                        if let Some(q) = pr.and_then(|pr| self.find(pr, hash, key)) {
                            return Some(q);
                        }
                        p = pl;
                    }
                }
            }
        }
        None
    }

    fn get_tree_node(&self, first: usize, hash: i32, key: usize) -> Option<usize> {
        let start = if self.nodes[first].parent.is_some() {
            self.root(first)
        } else {
            first
        };
        self.find(start, hash, key)
    }

    /// `TreeNode.putTreeVal`: the existing entry, or `None` after inserting.
    fn put_tree_val(&mut self, first: usize, hash: i32, key: usize) -> Option<usize> {
        let root = if self.nodes[first].parent.is_some() {
            self.root(first)
        } else {
            first
        };
        let mut p = root;
        loop {
            if self.nodes[p].hash == hash && self.same_key(p, hash, key) {
                return Some(p);
            }
            let less = self.direction(hash, key, p) != Ordering::Greater;
            let child = if less {
                self.nodes[p].left
            } else {
                self.nodes[p].right
            };
            if let Some(c) = child {
                p = c;
                continue;
            }
            let xpn = self.nodes[p].next;
            let x = self.new_node(hash, key, xpn);
            self.nodes[x].tree = true;
            if less {
                self.nodes[p].left = Some(x);
            } else {
                self.nodes[p].right = Some(x);
            }
            self.nodes[p].next = Some(x);
            self.nodes[x].parent = Some(p);
            self.nodes[x].prev = Some(p);
            if let Some(xpn) = xpn {
                self.nodes[xpn].prev = Some(x);
            }
            let root = self.balance_insertion(root, x);
            self.move_root_to_front(root);
            return None;
        }
    }

    fn rotate_left(&mut self, mut root: usize, p: usize) -> usize {
        let Some(r) = self.nodes[p].right else {
            return root;
        };
        let rl = self.nodes[r].left;
        self.nodes[p].right = rl;
        if let Some(rl) = rl {
            self.nodes[rl].parent = Some(p);
        }
        let pp = self.nodes[p].parent;
        self.nodes[r].parent = pp;
        match pp {
            None => {
                root = r;
                self.nodes[r].red = false;
            }
            Some(pp) if self.nodes[pp].left == Some(p) => self.nodes[pp].left = Some(r),
            Some(pp) => self.nodes[pp].right = Some(r),
        }
        self.nodes[r].left = Some(p);
        self.nodes[p].parent = Some(r);
        root
    }

    fn rotate_right(&mut self, mut root: usize, p: usize) -> usize {
        let Some(l) = self.nodes[p].left else {
            return root;
        };
        let lr = self.nodes[l].right;
        self.nodes[p].left = lr;
        if let Some(lr) = lr {
            self.nodes[lr].parent = Some(p);
        }
        let pp = self.nodes[p].parent;
        self.nodes[l].parent = pp;
        match pp {
            None => {
                root = l;
                self.nodes[l].red = false;
            }
            Some(pp) if self.nodes[pp].right == Some(p) => self.nodes[pp].right = Some(l),
            Some(pp) => self.nodes[pp].left = Some(l),
        }
        self.nodes[l].right = Some(p);
        self.nodes[p].parent = Some(l);
        root
    }

    fn balance_insertion(&mut self, mut root: usize, mut x: usize) -> usize {
        self.nodes[x].red = true;
        loop {
            let Some(mut xp) = self.nodes[x].parent else {
                self.nodes[x].red = false;
                return x;
            };
            if !self.nodes[xp].red {
                return root;
            }
            let Some(mut xpp) = self.nodes[xp].parent else {
                return root;
            };
            let xppl = self.nodes[xpp].left;
            if Some(xp) == xppl {
                if let Some(xppr) = self.nodes[xpp].right.filter(|&n| self.nodes[n].red) {
                    self.nodes[xppr].red = false;
                    self.nodes[xp].red = false;
                    self.nodes[xpp].red = true;
                    x = xpp;
                } else {
                    let mut xp_opt = Some(xp);
                    let mut xpp_opt = Some(xpp);
                    if Some(x) == self.nodes[xp].right {
                        x = xp;
                        root = self.rotate_left(root, x);
                        xp_opt = self.nodes[x].parent;
                        xpp_opt = xp_opt.and_then(|xp| self.nodes[xp].parent);
                    }
                    if let Some(p) = xp_opt {
                        xp = p;
                        self.nodes[xp].red = false;
                        if let Some(pp) = xpp_opt {
                            xpp = pp;
                            self.nodes[xpp].red = true;
                            root = self.rotate_right(root, xpp);
                        }
                    }
                }
            } else if let Some(xppl) = xppl.filter(|&n| self.nodes[n].red) {
                self.nodes[xppl].red = false;
                self.nodes[xp].red = false;
                self.nodes[xpp].red = true;
                x = xpp;
            } else {
                let mut xp_opt = Some(xp);
                let mut xpp_opt = Some(xpp);
                if Some(x) == self.nodes[xp].left {
                    x = xp;
                    root = self.rotate_right(root, x);
                    xp_opt = self.nodes[x].parent;
                    xpp_opt = xp_opt.and_then(|xp| self.nodes[xp].parent);
                }
                if let Some(p) = xp_opt {
                    xp = p;
                    self.nodes[xp].red = false;
                    if let Some(pp) = xpp_opt {
                        xpp = pp;
                        self.nodes[xpp].red = true;
                        root = self.rotate_left(root, xpp);
                    }
                }
            }
        }
    }

    /// First key index of each entry, in `values()` iteration order.
    fn iteration_order(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.size);
        for head in &self.table {
            let mut e = *head;
            while let Some(node) = e {
                order.push(self.nodes[node].key);
                e = self.nodes[node].next;
            }
        }
        order
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_string_hash_uses_utf16_and_spreads_high_bits() {
        assert_eq!(java_spread_hash(""), 0);
        assert_eq!(java_spread_hash("Aa"), java_spread_hash("BB"));
        assert_eq!(java_spread_hash("😀"), 1_772_920);
        assert_eq!(java_string_compare("\u{FF61}", "😀"), Ordering::Greater);
    }

    #[test]
    fn duplicate_after_threshold_can_resize_map() {
        let mut keys: Vec<String> = (0..13).map(|i| format!("house-1-{i}")).collect();
        assert_eq!(
            grouping_by_partition_order(&keys),
            vec![3, 4, 1, 2, 7, 8, 5, 6, 12, 10, 9, 11, 0]
        );
        keys.push(keys[0].clone());
        // JDK 21 computeIfAbsent resizes before probing the 14th call even
        // though that call finds an existing key. Both expected sequences
        // were captured from HousenumberHashOrderOracle.java.
        assert_eq!(
            grouping_by_partition_order(&keys),
            vec![3, 4, 1, 2, 7, 8, 5, 6, 12, 10, 11, 0, 9]
        );
    }

    #[test]
    fn collisions_insert_at_head() {
        let keys = ["Aa", "BB", "Aa", "x"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let order = grouping_by_partition_order(&keys);
        assert!(order.contains(&0));
        assert!(order.contains(&1));
        assert!(!order.contains(&2));
        assert!(order.iter().position(|i| *i == 1) < order.iter().position(|i| *i == 0));
    }

    #[test]
    fn put_order_matches_java_21_oracle() {
        // Captured from JDK 21: HashMap.newHashMap(keys.length), put each
        // key in order, then iterate keySet().
        let keys = [
            "name", "name:cs", "name:fr", "name:it", "name:pl", "name:sk", "place",
        ];
        assert_eq!(new_hash_map_put_order(&keys), vec![5, 0, 3, 2, 4, 6, 1]);
        let keys = [
            "amenity", "name", "name:hu", "name:sk", "name:uk", "website",
        ];
        assert_eq!(new_hash_map_put_order(&keys), vec![1, 5, 3, 0, 2, 4]);
        // new HashSet<Long>(), add 49715, 14296, 16, 32.
        assert_eq!(
            hash_set_long_order(&[49_715, 14_296, 16, 32]),
            vec![2, 3, 0, 1]
        );
        // groupingBy over [49715, 14296, 49715, 14296].
        assert_eq!(
            grouping_by_long_order(&[49_715, 14_296, 49_715, 14_296]),
            vec![0, 1]
        );
    }

    #[test]
    fn real_house_partitions_match_java_21_grouping_by_oracle() {
        let partitions = include_str!("../../tests/fixtures/housenumber-partitions-256.txt")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let expected = include_str!("../../tests/fixtures/housenumber-partitions-256.expected.txt")
            .trim()
            .split(',')
            .map(|index| index.parse::<usize>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(partitions.len(), 256);
        assert_eq!(grouping_by_partition_order(&partitions), expected);
    }

    fn expected(text: &str) -> Vec<usize> {
        text.trim()
            .split(',')
            .map(|index| index.parse::<usize>().unwrap())
            .collect()
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    /// Whether filling a map with `keys` leaves a tree bin.
    fn has_tree_bin<K, C: Fn(&K, &K) -> Ordering>(map: &JavaHashMap<'_, K, C>) -> bool {
        map.table.iter().flatten().any(|&head| map.nodes[head].tree)
    }

    // Each fixture forces bins of eight or more entries in tables of 64 or
    // more: strings and longs with equal hashes, and others sharing low hash
    // bits that resizes split apart. Expected sequences were printed by
    // JavaHashMapOrderOracle.java on JDK 21.
    #[test]
    fn tree_bins_match_java_21_oracle() {
        let strings = lines(include_str!(
            "../../tests/fixtures/hashmap-tree-strings.txt"
        ))
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(
            grouping_by_partition_order(&strings),
            expected(include_str!(
                "../../tests/fixtures/hashmap-tree-strings.grouping.expected.txt"
            ))
        );
        let mut map = JavaHashMap::new(
            &strings,
            |key: &String| java_spread_hash(key),
            |a: &String, b: &String| java_string_compare(a, b),
            0,
        );
        (0..strings.len()).for_each(|i| map.compute_if_absent(i));
        assert!(has_tree_bin(&map));

        let distinct = lines(include_str!(
            "../../tests/fixtures/hashmap-tree-distinct-strings.txt"
        ));
        assert_eq!(
            new_hash_map_put_order(&distinct),
            expected(include_str!(
                "../../tests/fixtures/hashmap-tree-distinct-strings.put-sized.expected.txt"
            ))
        );

        let longs = lines(include_str!("../../tests/fixtures/hashmap-tree-longs.txt"))
            .into_iter()
            .map(|line| line.parse::<i64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            grouping_by_long_order(&longs),
            expected(include_str!(
                "../../tests/fixtures/hashmap-tree-longs.grouping.expected.txt"
            ))
        );
        assert_eq!(
            hash_set_long_order(&longs),
            expected(include_str!(
                "../../tests/fixtures/hashmap-tree-longs.hashset.expected.txt"
            ))
        );
        let mut map = JavaHashMap::new(&longs, |key| java_long_spread_hash(*key), i64::cmp, 0);
        (0..longs.len()).for_each(|i| map.put(i));
        assert!(has_tree_bin(&map));
    }
}
