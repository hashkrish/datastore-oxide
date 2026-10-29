use crate::composite::CompositeIndex;
use crate::cost::{self, Form};
use oxide_model::{Entity, Key};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};
/// Transactions live at least this long (Java's `datastore.max_txn_lifetime`).
pub(crate) const TXN_LIFETIME: Duration = Duration::from_secs(300);
/// Most entity groups one transaction may touch.
pub(crate) const MAX_GROUPS_PER_TXN: usize = 25;
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    pub entity: Entity,
    pub version: i64,
    /// Creation order within the project (Java keeps entities in insertion-ordered maps; queries
    /// break ties between keys that differ only in database by it). Updates keep it.
    pub seq: u64,
    /// How it is stored beyond its properties (`__scatter__`, export side effects).
    pub form: Form,
}
#[derive(Debug)]
pub(crate) struct Job {
    timestamp: i64,
    /// Effective puts (no-op puts are left out) with their scatter policy, and deletes; last write
    /// per key wins.
    puts: Vec<(Entity, bool)>,
    deletes: Vec<Key>,
}
#[derive(Debug, Default)]
pub(crate) struct Group {
    pub version: i64,
    jobs: VecDeque<Job>,
}
impl Group {
    /// Keys written by unapplied jobs.
    pub fn written_keys(&self) -> impl Iterator<Item = &Key> {
        self.jobs.iter().flat_map(|j| {
            j.puts
                .iter()
                .filter_map(|(e, _)| e.key.as_ref())
                .chain(&j.deletes)
        })
    }
}
#[derive(Debug)]
pub(crate) struct Txn {
    pub read_only: bool,
    /// May touch several entity groups (always for v1; App Engine's `allow_multiple_eg`).
    pub xg: bool,
    pub failed: bool,
    pub created: Instant,
    /// Tracked groups (root keys) with their version at first touch, in touch order.
    pub trackers: Vec<(Key, i64)>,
    /// Applied state of a tracked group, frozen when someone else first wrote it.
    pub snapshots: HashMap<Key, BTreeMap<Key, Stored>>,
    /// Buffered writes, last operation per key (`None` = delete), in first-write order.
    pub writes: Vec<(Key, Option<Entity>)>,
    /// Transactional tasks (opaque `TaskQueueAddRequest`s), handed back on commit.
    pub actions: Vec<Vec<u8>>,
    /// A read-only transaction's `read_time` (superset), in microseconds.
    pub read_time: Option<i64>,
}
impl Txn {
    pub fn new(read_only: bool, xg: bool) -> Self {
        Self {
            read_only,
            xg,
            failed: false,
            created: Instant::now(),
            trackers: Vec::new(),
            snapshots: HashMap::new(),
            writes: Vec::new(),
            actions: Vec::new(),
            read_time: None,
        }
    }
    pub fn buffer(&mut self, key: Key, entity: Option<Entity>) {
        match self.writes.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = entity,
            None => self.writes.push((key, entity)),
        }
    }
}
/// The kinds of a project in the order Java visits them (export order): a hash table of
/// `capacity` buckets visited in ascending bucket order, the newest kind first within a bucket.
/// It grows (doubling, keeping the relative order within buckets) when it holds more than 3/4 of
/// its capacity, and never shrinks.
#[derive(Debug, Clone)]
pub(crate) struct KindTable {
    capacity: usize,
    order: Vec<String>,
}
impl Default for KindTable {
    fn default() -> Self {
        Self {
            capacity: 16,
            order: Vec::new(),
        }
    }
}
/// `String.hashCode()` spread as a Java hash table does (`h ^ (h >>> 16)`).
fn java_hash(s: &str) -> u32 {
    let h = oxide_model::java_string_hash(s) as u32;
    h ^ (h >> 16)
}
impl KindTable {
    fn bucket(&self, kind: &str) -> usize {
        java_hash(kind) as usize & (self.capacity - 1)
    }
    pub fn contains(&self, kind: &str) -> bool {
        self.order.iter().any(|k| k == kind)
    }
    pub fn insert(&mut self, kind: &str) {
        if self.contains(kind) {
            return;
        }
        let bucket = self.bucket(kind);
        let at = self
            .order
            .iter()
            .position(|k| self.bucket(k) >= bucket)
            .unwrap_or(self.order.len());
        self.order.insert(at, kind.to_string());
        self.grow();
    }
    /// Doubles the capacity while the table is more than 3/4 full.
    fn grow(&mut self) {
        let mut grown = false;
        while self.order.len() * 4 > self.capacity * 3 {
            self.capacity *= 2;
            grown = true;
        }
        if grown {
            let mut order = std::mem::take(&mut self.order);
            order.sort_by_key(|k| self.bucket(k));
            self.order = order;
        }
    }
    /// A table holding `kinds` (saved in iteration order), keeping their order within buckets.
    pub fn deserialized(kinds: &[String]) -> Self {
        let mut table = Self::default();
        for kind in kinds {
            if !table.contains(kind) {
                table.order.push(kind.clone());
            }
        }
        while table.order.len() * 4 > table.capacity * 3 {
            table.capacity *= 2;
        }
        let mut order = std::mem::take(&mut table.order);
        order.sort_by_key(|k| table.bucket(k));
        table.order = order;
        table
    }
    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }
}
/// The entity group of `key` made of its first `len` path elements. Like Java's (a bare `Path`),
/// it ignores the namespace and database: `K:a` is one group in every namespace of a project.
pub(crate) fn group_of(key: &Key, len: usize) -> Key {
    Key {
        partition: oxide_model::PartitionId {
            project_id: key.partition.project_id.clone(),
            ..Default::default()
        },
        path: key.path[..len.min(key.path.len())].to_vec(),
    }
}
/// Whether `key` belongs to `group` (a root entity's group, or without entity groups any key's
/// own): it is the group's key or a descendant, in any namespace or database of the project.
pub(crate) fn in_group(key: &Key, group: &Key) -> bool {
    key.partition.project_id == group.partition.project_id
        && key.path.len() >= group.path.len()
        && key.path[..group.path.len()] == group.path[..]
}
/// What a write job costs: index rows written and entities changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobCost {
    pub index_writes: i64,
    pub entity_writes: i64,
}
impl std::ops::AddAssign for JobCost {
    fn add_assign(&mut self, other: Self) {
        self.index_writes += other.index_writes;
        self.entity_writes += other.entity_writes;
    }
}
/// Decides whether one apply attempt succeeds (one RNG draw for probabilistic policies).
pub(crate) type Decide<'a> = &'a mut dyn FnMut() -> bool;
#[derive(Debug)]
pub(crate) struct Project {
    pub last_commit: i64,
    /// Applied entities, across databases and namespaces (they are part of the key).
    pub entities: BTreeMap<Key, Stored>,
    pub groups: HashMap<Key, Group>,
    /// Groups with unapplied jobs, in the order they got them.
    pending: Vec<Key>,
    pub txns: HashMap<u64, Txn>,
    /// The next creation sequence number.
    pub next_seq: u64,
    /// Kinds in Java's iteration order (export order).
    pub kinds: KindTable,
    /// Set when a write is applied; cleared when the store is saved (Java's `dirty`).
    pub dirty: bool,
    /// Point-in-time reads (superset): the wall-clock time of each commit version, ascending.
    pub commit_times: VecDeque<(i64, i64)>,
    /// Committed states of written keys, `(version, entity or None for deleted)` ascending,
    /// starting with the state before the first recorded write.
    pub history: HashMap<Key, Vec<(i64, Option<Entity>)>>,
    /// Wall-clock `(create, update)` times of entities written since startup, in microseconds
    /// (superset response timestamps).
    pub times: HashMap<Key, (i64, i64)>,
}

/// How a job is stored.
#[derive(Clone, Copy)]
pub(crate) struct JobOptions<'a> {
    /// Sequential ids: puts get the hidden `__scatter__` property.
    pub scatter: bool,
    pub indexes: &'a [CompositeIndex],
    /// Record point-in-time history.
    pub history: bool,
}

/// A committed write for the history: key, state before (with its version), state after.
type HistoryWrite = (Key, Option<(Entity, i64)>, Option<Entity>);

/// How long point-in-time reads reach back (production's default retention).
pub(crate) const HISTORY_RETENTION_MICROS: i64 = 3_600_000_000;

pub(crate) fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}
impl Default for Project {
    fn default() -> Self {
        Self {
            next_seq: 0,
            kinds: KindTable::default(),
            dirty: false,
            last_commit: 1,
            entities: BTreeMap::new(),
            groups: HashMap::new(),
            pending: Vec::new(),
            txns: HashMap::new(),
            commit_times: VecDeque::new(),
            history: HashMap::new(),
            times: HashMap::new(),
        }
    }
}
impl Project {
    pub fn group_version(&self, group: &Key) -> i64 {
        self.groups.get(group).map_or(0, |g| g.version)
    }
    /// `key` as `group`'s jobs leave it: the last unapplied job of the group that writes it, else
    /// the applied entity. Returns the entity, its version and its stored form.
    pub fn logical(&self, key: &Key, group: &Key) -> Option<(&Entity, i64, Form)> {
        if let Some(g) = self.groups.get(group) {
            for job in g.jobs.iter().rev() {
                if job.deletes.contains(key) {
                    return None;
                }
                if let Some((entity, scatter)) =
                    job.puts.iter().find(|(e, _)| e.key.as_ref() == Some(key))
                {
                    let form = Form {
                        scatter: *scatter,
                        stripped: false,
                    };
                    return Some((entity, job.timestamp, form));
                }
            }
        }
        self.entities
            .get(key)
            .map(|s| (&s.entity, s.version, s.form))
    }
    /// Applied entities of one group: the group's key and its descendants.
    fn group_entities(&self, group: &Key) -> BTreeMap<Key, Stored> {
        self.entities
            .iter()
            .filter(|(k, _)| in_group(k, group))
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect()
    }
    /// Marks `group` as modified: bumps its version and freezes it for transactions that tracked
    /// it at the old version.
    pub fn bump_group(&mut self, group: &Key) {
        let old = self.group_version(group);
        let needs_snapshot: Vec<u64> = self
            .txns
            .iter()
            .filter(|(_, t)| {
                t.trackers.iter().any(|(g, v)| g == group && *v == old)
                    && !t.snapshots.contains_key(group)
            })
            .map(|(h, _)| *h)
            .collect();
        if !needs_snapshot.is_empty() {
            let snapshot = self.group_entities(group);
            for h in needs_snapshot {
                self.txns
                    .get_mut(&h)
                    .expect("listed")
                    .snapshots
                    .insert(group.clone(), snapshot.clone());
            }
        }
        self.groups.entry(group.clone()).or_default().version = old + 1;
    }
    pub fn next_timestamp(&mut self) -> i64 {
        self.last_commit += 1;
        self.last_commit
    }

    /// Records the wall-clock time of commit `version` (once per version) and returns it.
    fn record_commit_time(&mut self, version: i64) -> i64 {
        if let Some(&(v, t)) = self.commit_times.back()
            && v >= version
        {
            return t;
        }
        let now = now_micros().max(self.commit_times.back().map_or(0, |(_, t)| t + 1));
        self.commit_times.push_back((version, now));
        // Keep one commit older than the retention window: reads inside it resolve to it.
        let cutoff = now - HISTORY_RETENTION_MICROS;
        while self.commit_times.len() > 1 && self.commit_times[1].1 <= cutoff {
            self.commit_times.pop_front();
        }
        now
    }

    /// The wall-clock time of commit `version`, if it is recorded.
    pub fn version_time(&self, version: i64) -> Option<i64> {
        self.commit_times
            .iter()
            .rev()
            .find(|(v, _)| *v == version)
            .map(|(_, t)| *t)
    }

    /// The last version committed at or before `micros` (point-in-time reads).
    pub fn version_at(&self, micros: i64) -> i64 {
        match self.commit_times.iter().rposition(|(_, t)| *t <= micros) {
            Some(i) => self.commit_times[i].0,
            // Before every recorded commit: the state just before the first one.
            None => self
                .commit_times
                .front()
                .map_or(self.last_commit, |(v, _)| v - 1),
        }
    }

    /// `key` as committed at `version` (point-in-time reads).
    pub fn entity_at(&self, key: &Key, group: &Key, version: i64) -> Option<(Entity, i64)> {
        match self.history.get(key) {
            Some(states) => states
                .iter()
                .rev()
                .find(|(v, _)| *v <= version)
                .and_then(|(v, e)| e.clone().map(|e| (e, *v))),
            None => self
                .logical(key, group)
                .filter(|(_, v, _)| *v <= version)
                .map(|(e, v, _)| (e.clone(), v)),
        }
    }

    /// Records a committed write of `key` in its history.
    fn record(
        &mut self,
        key: &Key,
        before: Option<(Entity, i64)>,
        version: i64,
        after: Option<Entity>,
    ) {
        let oldest = self.commit_times.front().map_or(i64::MIN, |(v, _)| *v);
        let states = self.history.entry(key.clone()).or_insert_with(|| {
            before
                .map(|(e, v)| vec![(v, Some(e))])
                .unwrap_or_else(|| vec![(0, None)])
        });
        states.push((version, after));
        // Drop states that no retained version can read.
        while states.len() > 1 && states[1].0 <= oldest {
            states.remove(0);
        }
    }
    /// Creates a job for `group` from writes (`None` = delete) and adds it. Returns the job's
    /// cost and each key's resulting version, in the order given. Unchanged puts (including the
    /// hidden `__scatter__`) keep their version.
    pub fn add_job(
        &mut self,
        group: &Key,
        timestamp: i64,
        writes: &[(Key, Option<Entity>)],
        options: JobOptions,
        decide: Decide,
    ) -> (JobCost, Vec<i64>) {
        let JobOptions {
            scatter,
            indexes,
            history,
        } = options;
        let mut job = Job {
            timestamp,
            puts: Vec::new(),
            deletes: Vec::new(),
        };
        let mut cost = JobCost::default();
        let mut versions = Vec::with_capacity(writes.len());
        let mut last: Vec<(&Key, &Option<Entity>)> = Vec::new();
        for (k, w) in writes {
            match last.iter_mut().find(|(lk, _)| *lk == k) {
                Some(slot) => slot.1 = w,
                None => last.push((k, w)),
            }
        }
        let mut version_of: HashMap<&Key, i64> = HashMap::new();
        let mut recorded: Vec<HistoryWrite> = Vec::new();
        for (key, write) in &last {
            let before = self.logical(key, group);
            let before_owned = before.map(|(e, v, _)| (e.clone(), v));
            match write {
                None => {
                    if let Some((old, _, form)) = before {
                        cost.index_writes += cost::delete_cost(old, form, indexes);
                        cost.entity_writes += 1;
                        job.deletes.push((*key).clone());
                        version_of.insert(key, timestamp);
                        recorded.push(((*key).clone(), before_owned, None));
                    } else {
                        version_of.insert(key, self.last_commit);
                    }
                }
                Some(entity) => {
                    cost.index_writes +=
                        cost::write_cost(before.map(|(e, _, f)| (e, f)), entity, scatter, indexes);
                    let new_form = Form {
                        scatter,
                        stripped: false,
                    };
                    match before {
                        Some((old, v, form))
                            if old.properties == entity.properties
                                && form.same_index_data(new_form, old) =>
                        {
                            version_of.insert(key, v);
                        }
                        _ => {
                            job.puts.push((entity.clone(), scatter));
                            cost.entity_writes += 1;
                            version_of.insert(key, timestamp);
                            recorded.push(((*key).clone(), before_owned, Some(entity.clone())));
                        }
                    }
                }
            }
        }
        for (key, _) in writes {
            versions.push(version_of[key]);
        }
        if history {
            let now = self.record_commit_time(timestamp);
            for (key, before, after) in &recorded {
                match after {
                    Some(_) => {
                        let created = self
                            .times
                            .get(key)
                            .filter(|_| before.is_some())
                            .map_or(now, |(c, _)| *c);
                        self.times.insert(key.clone(), (created, now));
                    }
                    None => {
                        self.times.remove(key);
                    }
                }
            }
            for (key, before, after) in recorded {
                self.record(&key, before, timestamp, after);
            }
        }
        let group_state = self.groups.entry(group.clone()).or_default();
        group_state.jobs.push_back(job);
        if !self.pending.contains(group) {
            self.pending.push(group.clone());
        }
        self.maybe_roll_forward(group, decide);
        (cost, versions)
    }
    fn apply(&mut self, job: Job) {
        for key in &job.deletes {
            self.entities.remove(key);
        }
        for (entity, scatter) in job.puts {
            let key = entity.key.clone().expect("stored entities have keys");
            self.kinds.insert(key.kind());
            let seq = match self.entities.get(&key) {
                Some(s) => s.seq,
                None => {
                    self.next_seq += 1;
                    self.next_seq
                }
            };
            self.entities.insert(
                key,
                Stored {
                    entity,
                    version: job.timestamp,
                    seq,
                    form: Form {
                        scatter,
                        stripped: false,
                    },
                },
            );
        }
        self.dirty = true;
    }
    /// Tries to apply `group`'s jobs in order, stopping at the first one the policy rejects.
    pub fn maybe_roll_forward(&mut self, group: &Key, decide: Decide) {
        while let Some(g) = self.groups.get_mut(group) {
            if g.jobs.is_empty() || !decide() {
                break;
            }
            let job = g.jobs.pop_front().expect("non-empty");
            self.apply(job);
        }
        if self.groups.get(group).is_none_or(|g| g.jobs.is_empty()) {
            self.pending.retain(|p| p != group);
        }
    }
    /// Applies every job of `group` without consulting the policy (a strong read).
    pub fn roll_forward(&mut self, group: &Key) {
        while let Some(job) = self.groups.get_mut(group).and_then(|g| g.jobs.pop_front()) {
            self.apply(job);
        }
        self.pending.retain(|p| p != group);
    }
    /// Applies every unapplied job, group by group in the order they got jobs.
    pub fn roll_forward_all(&mut self) {
        for group in self.pending.clone() {
            self.roll_forward(&group);
        }
    }
    /// Gives every group with unapplied jobs one chance to catch up.
    pub fn groom(&mut self, decide: Decide) {
        for group in self.pending.clone() {
            self.maybe_roll_forward(&group, decide);
        }
    }
    /// Drops transactions past their lifetime.
    pub fn expire_txns(&mut self) {
        self.txns.retain(|_, t| t.created.elapsed() < TXN_LIFETIME);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn order(t: &KindTable) -> Vec<&str> {
        t.iter().map(String::as_str).collect()
    }
    #[test]
    fn kind_order_matches_java() {
        let mut t = KindTable::default();
        for k in ["A", "b", "Q", "r", "a"] {
            t.insert(k);
        }
        assert_eq!(order(&t), ["a", "Q", "A", "r", "b"]);
        let mut t = KindTable::default();
        for k in [
            "A", "Q", "a", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L",
        ] {
            t.insert(k);
        }
        assert_eq!(
            order(&t),
            [
                "a", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "Q"
            ]
        );
    }
}
