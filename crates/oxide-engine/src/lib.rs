pub mod composite;
pub mod cost;
/// Kind of the pseudo-group queries in transactions track (see [`Engine::run_query`]).
const NONE_KIND: &str = "__none__";
pub use cost::Form;
pub mod cursor;
pub mod index;
pub mod index_files;
pub mod pseudo;
pub mod query;
pub mod random;
mod store;
pub mod transform;
use oxide_model::{Entity, IdOrName, Key, PathElement, Value};
use random::JavaRandom;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
pub use store::JobCost;
use store::{MAX_GROUPS_PER_TXN, Project, Txn};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    /// Transaction contention.
    Aborted,
    /// A missing composite index.
    FailedPrecondition,
    /// A failure the Java emulator reports as UNKNOWN with no message.
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: Code,
    pub message: String,
}
impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidArgument, message)
    }
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
/// How ids are chosen for incomplete keys in commits. `allocateIds` always scatters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoIdPolicy {
    #[default]
    Sequential,
    Scattered,
}
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub auto_id_policy: AutoIdPolicy,
    pub job_policy: JobPolicy,
}
/// How write jobs are applied (the emulator's simulated eventual consistency).
#[derive(Debug, Clone, PartialEq)]
pub enum JobPolicy {
    /// Each apply attempt fails with probability `unapplied_percentage`%, using
    /// `java.util.Random(random_seed)`.
    Probability {
        random_seed: i64,
        unapplied_percentage: f32,
    },
    /// Every job applies immediately.
    ForceStrongConsistency,
}
impl Default for JobPolicy {
    fn default() -> Self {
        JobPolicy::Probability {
            random_seed: 0,
            unapplied_percentage: 0.0,
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Insert(Entity),
    Update(Entity),
    Upsert(Entity),
    Delete(Key),
}
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation {
    pub op: Op,
    pub base_version: Option<i64>,
    pub on_conflict: OnConflict,
    /// Property mask and transforms (superset only).
    pub partial: transform::PartialWrite,
}

/// `Mutation.conflict_resolution_strategy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// `SERVER_VALUE` (or unset): the conflict is reported in the mutation result.
    #[default]
    ServerValue,
    /// `FAIL`: the commit fails.
    Fail,
    /// An unrecognized strategy: reported like `SERVER_VALUE`, but Java's non-transactional
    /// commit still uses up a version.
    Unrecognized,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MutationResult {
    /// Set only when the mutation's key was completed by allocating an id.
    pub key: Option<Key>,
    /// 0 when there is no version to report (a conflict on a missing entity).
    pub version: i64,
    pub conflict_detected: bool,
    /// One value per property transform of the mutation.
    pub transform_results: Vec<Value>,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommitResult {
    pub mutation_results: Vec<MutationResult>,
    pub index_updates: i64,
    /// Entities changed (written or deleted).
    pub entity_writes: i64,
    /// Highest version written by a transactional commit (0 otherwise).
    pub commit_version: i64,
    /// Transactional commits: every written key, then every deleted key, with its version.
    pub key_versions: Vec<(Key, i64)>,
    /// Transactional commits: the transaction's tasks (see [`Engine::add_actions`]).
    pub actions: Vec<Vec<u8>>,
}
/// The result of a v3 write request outside a transaction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WriteResult {
    /// The written keys (completed) in request order.
    pub keys: Vec<Key>,
    /// Each key's version, in request order.
    pub versions: Vec<i64>,
    pub cost: JobCost,
}
#[derive(Debug, Clone, PartialEq)]
pub enum LookupResult {
    Found {
        entity: Entity,
        version: i64,
    },
    /// `version` is 0 for eventually consistent reads.
    Missing {
        key: Key,
        version: i64,
    },
}
/// How a read is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// Rolls the entity group's pending writes forward first.
    Strong,
    /// Reads applied state only.
    Eventual,
    /// Reads inside a transaction (tracks the group, may read a snapshot).
    Transaction(u64),
    /// Reads the committed state as of a time in microseconds (superset point-in-time reads).
    At(i64),
}
/// Kinds that are served by pseudo-kind implementations; looking them up by key fails in Java.
const PSEUDO_KINDS: &[&str] = &["__kind__", "__property__", "__namespace__"];
/// Ids at or above this value are scattered ids.
const SCATTERED_ID_BASE: i64 = 1 << 52;
/// Number of empty high-order bits above the scattered id space.
const SCATTER_SHIFT: u32 = 64 - 52 + 1;
/// Job policy draws are `nextInt(RANDOM_CEILING)` compared against `percentage * 100`.
const RANDOM_CEILING: i32 = 10_000;
const CONTENTION: &str = "too much contention on these datastore entities. please try again.";
const TXN_INVALID: &str = "transaction has expired or is invalid";
const TXN_CLOSED: &str = "transaction closed";
/// Maps a counter value into the scattered id space `[2^52, 2^52 + 2^51)`.
pub fn to_scattered_id(counter: i64) -> i64 {
    SCATTERED_ID_BASE + ((counter as u64) << SCATTER_SHIFT).reverse_bits() as i64
}
#[derive(Debug)]
struct State {
    /// One counter for every project, kind and namespace: sequential auto ids and `allocateIds`.
    id_counter: i64,
    /// Auto ids under the SCATTERED policy come from their own counter.
    scattered_counter: i64,
    projects: HashMap<String, Project>,
    /// Unsaved writes beyond the projects' own flags (a failed save).
    dirty: bool,
}
/// The saved form of the store (see [`Engine::take_snapshot`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id_counter: i64,
    pub scattered_counter: i64,
    pub projects: Vec<ProjectSnapshot>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSnapshot {
    pub project_id: String,
    /// Applied entities (with their keys), versions and stored forms, in creation order.
    pub entities: Vec<(Entity, i64, Form)>,
    /// Every kind ever stored, in Java's iteration order (it decides export order).
    pub kinds: Vec<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            id_counter: 1,
            scattered_counter: 1,
            projects: HashMap::new(),
            dirty: false,
        }
    }
}
impl State {
    fn next_counter(&mut self) -> i64 {
        let value = self.id_counter;
        self.id_counter += 1;
        value
    }
}
pub struct Engine {
    /// Runtime-adjustable settings (the EmulatorConfig service); they survive `reset`.
    config: Mutex<Config>,
    /// The job policy's `java.util.Random`, re-seeded whenever the job policy is replaced.
    rng: Mutex<JavaRandom>,
    state: Mutex<State>,
    /// Transaction handles: one counter for all projects, not reset by `reset` (as in Java).
    next_handle: AtomicU64,
    /// Startup time in microseconds; `__entity_group__` versions are offset by it so they keep
    /// increasing across restarts (as in App Engine's `EntityGroupPseudoKind`).
    base_version: i64,
    /// Composite-index bookkeeping (index files, `--require_indexes`), if configured.
    indexes: Option<index_files::IndexManager>,
    /// Firestore-in-Datastore mode (`--firestore_in_datastore_mode`).
    firestore: bool,
    /// Keep committed history for point-in-time reads (superset).
    point_in_time: bool,
    /// `--enable_entity_groups` (default true): reads and deletes use the root entity's group;
    /// otherwise the full key path (puts always use the root, as in Java).
    entity_groups: bool,
    /// Bumped whenever the data is replaced (reset, restore), so layers holding state tied to
    /// the data (v3 query handles) can drop it.
    generation: AtomicU64,
}
fn seed_of(policy: &JobPolicy) -> i64 {
    match policy {
        JobPolicy::Probability { random_seed, .. } => *random_seed,
        JobPolicy::ForceStrongConsistency => 0,
    }
}
impl Engine {
    pub fn new(config: Config) -> Self {
        Self {
            rng: Mutex::new(JavaRandom::new(seed_of(&config.job_policy))),
            config: Mutex::new(config),
            state: Mutex::new(State::default()),
            next_handle: AtomicU64::new(1),
            base_version: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64 * 1000),
            indexes: None,
            firestore: false,
            point_in_time: false,
            entity_groups: true,
            generation: AtomicU64::new(0),
        }
    }
    /// `--enable_entity_groups=false`.
    pub fn without_entity_groups(mut self) -> Self {
        self.entity_groups = false;
        self
    }
    /// The group a read of `key` tracks and rolls forward: the root entity, or without entity
    /// groups the full key path.
    fn read_group(&self, key: &Key) -> Key {
        if self.entity_groups {
            store::group_of(key, 1)
        } else {
            store::group_of(key, key.path.len())
        }
    }
    /// The group a write of `key` goes to: like a read for deletes, but always the root entity
    /// for puts (as in Java, whatever `--enable_entity_groups` says).
    fn write_group(&self, key: &Key, delete: bool) -> Key {
        if delete {
            self.read_group(key)
        } else {
            store::group_of(key, 1)
        }
    }
    /// Firestore-in-Datastore mode (strongly consistent until the job policy is changed).
    pub fn with_firestore_mode(mut self) -> Self {
        self.set_job_policy(JobPolicy::ForceStrongConsistency);
        self.firestore = true;
        self
    }
    /// Keeps an hour of committed history so reads can ask for an earlier `read_time`.
    pub fn with_point_in_time(mut self) -> Self {
        self.point_in_time = true;
        self
    }
    /// Begins a read-only transaction that reads as of `read_time` (microseconds).
    pub fn begin_read_only_at(&self, project_id: &str, read_time: i64) -> Result<u64> {
        check_read_time(read_time)?;
        let handle = self.begin_transaction(project_id, true, true, None)?;
        let mut state = self.lock();
        if let Some(txn) = state
            .projects
            .get_mut(project_id)
            .and_then(|p| p.txns.get_mut(&handle))
        {
            txn.read_time = Some(read_time);
        }
        Ok(handle)
    }
    /// A read-only transaction with a `read_time` reads as of that time.
    fn effective_mode(project: &store::Project, mode: ReadMode) -> Result<ReadMode> {
        Ok(match mode {
            ReadMode::Transaction(h) => match project.txns.get(&h).and_then(|t| t.read_time) {
                Some(t) => ReadMode::At(t),
                None => mode,
            },
            ReadMode::At(t) => {
                check_read_time(t)?;
                mode
            }
            _ => mode,
        })
    }
    pub fn firestore_mode(&self) -> bool {
        self.firestore
    }
    /// Records queries in `indexes` (index-file generation and missing-index checks).
    pub fn with_indexes(mut self, indexes: index_files::IndexManager) -> Self {
        self.indexes = Some(indexes);
        self
    }
    pub fn config(&self) -> Config {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn set_auto_id_policy(&self, policy: AutoIdPolicy) {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .auto_id_policy = policy;
    }
    /// Replaces the job policy (and restarts its random sequence from the policy's seed).
    pub fn set_job_policy(&self, policy: JobPolicy) {
        *self.rng.lock().unwrap_or_else(|p| p.into_inner()) = JavaRandom::new(seed_of(&policy));
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .job_policy = policy;
    }
    /// The applied entities of every database of `project_id` matching the namespace and kind
    /// filters, grouped by kind in the kind table's order and in creation order within a kind.
    /// Like Java, this strips the hidden index data (`__scatter__`, embedded-entity values) of the
    /// exported entities in place.
    pub fn export_entities(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        kind: Option<&str>,
    ) -> Vec<Entity> {
        let mut state = self.lock();
        let Some(project) = state.projects.get_mut(project_id) else {
            return Vec::new();
        };
        let mut matching: Vec<&mut store::Stored> = project
            .entities
            .iter_mut()
            .filter(|(k, _)| namespace.is_none_or(|ns| k.partition.namespace_id == ns))
            .filter(|(k, _)| kind.is_none_or(|kind| k.kind() == kind))
            .map(|(_, s)| s)
            .collect();
        matching.sort_by_key(|s| s.seq);
        let mut out = Vec::with_capacity(matching.len());
        for kind in project.kinds.iter() {
            for stored in matching.iter_mut().filter(|s| {
                s.entity
                    .key
                    .as_ref()
                    .is_some_and(|k| k.kind() == kind.as_str())
            }) {
                stored.form.stripped = true;
                out.push(stored.entity.clone());
            }
        }
        out
    }
    /// Writes imported entities like App Engine v3 non-transactional puts: incomplete keys get
    /// ids, then one job per entity group.
    pub fn import_entities(&self, project_id: &str, entities: Vec<Entity>) {
        let writes = entities
            .into_iter()
            .map(|mut e| {
                let mut key = e.key.clone().expect("imported entities have keys");
                self.complete_key(&mut key);
                e.key = Some(key.clone());
                (key, Some(e))
            })
            .collect();
        self.write_groups(project_id, writes);
    }
    /// Applies every unapplied write job (Java does this before saving at shutdown).
    pub fn roll_forward_all(&self) {
        let mut state = self.lock();
        for project in state.projects.values_mut() {
            project.roll_forward_all();
        }
    }
    /// What Java saves to its backing store, if anything was written since the last save: the id
    /// counters and every applied entity with its version, in creation order. Commit timestamps,
    /// entity groups, unapplied jobs and transactions are not saved. Marks the store clean; call
    /// [`Engine::mark_dirty`] if saving fails.
    pub fn take_snapshot(&self) -> Option<Snapshot> {
        let mut state = self.lock();
        let dirty = state.dirty || state.projects.values().any(|p| p.dirty);
        if !dirty {
            return None;
        }
        state.dirty = false;
        let mut projects: Vec<ProjectSnapshot> = state
            .projects
            .iter_mut()
            .map(|(id, project)| {
                project.dirty = false;
                let mut stored: Vec<&store::Stored> = project.entities.values().collect();
                stored.sort_by_key(|s| s.seq);
                ProjectSnapshot {
                    project_id: id.clone(),
                    entities: stored
                        .into_iter()
                        .map(|s| (s.entity.clone(), s.version, s.form))
                        .collect(),
                    kinds: project.kinds.iter().cloned().collect(),
                }
            })
            .collect();
        projects.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        Some(Snapshot {
            id_counter: state.id_counter,
            scattered_counter: state.scattered_counter,
            projects,
        })
    }
    /// Marks the store as having unsaved writes (after a failed save).
    pub fn mark_dirty(&self) {
        self.lock().dirty = true;
    }
    /// Replaces all data with a loaded snapshot.
    pub fn restore(&self, snapshot: Snapshot) {
        let mut state = State {
            id_counter: snapshot.id_counter,
            scattered_counter: snapshot.scattered_counter,
            ..State::default()
        };
        for p in snapshot.projects {
            let project = state.projects.entry(p.project_id).or_default();
            let mut kinds = p.kinds;
            for (entity, _, _) in &p.entities {
                let kind = entity
                    .key
                    .as_ref()
                    .expect("stored entities have keys")
                    .kind();
                if !kinds.iter().any(|k| k == kind) {
                    kinds.push(kind.to_string());
                }
            }
            project.kinds = store::KindTable::deserialized(&kinds);
            for (entity, version, form) in p.entities {
                let key = entity.key.clone().expect("stored entities have keys");
                project.next_seq += 1;
                project.entities.insert(
                    key,
                    store::Stored {
                        entity,
                        version,
                        seq: project.next_seq,
                        form,
                    },
                );
            }
        }
        *self.lock() = state;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    /// Every composite index, from the index files and the query history of all projects.
    pub fn composite_indexes(&self) -> Vec<composite::CompositeIndex> {
        self.indexes
            .as_ref()
            .map(index_files::IndexManager::indexes)
            .unwrap_or_default()
    }
    /// Adds a composite index created through the Admin API (superset); false if it exists.
    pub fn add_admin_index(&self, index: composite::CompositeIndex) -> bool {
        self.indexes
            .as_ref()
            .is_some_and(|m| m.add_admin_index(index))
    }
    /// The defined composite indexes: from the index files and the Admin API.
    pub fn defined_indexes(&self) -> Vec<composite::CompositeIndex> {
        self.indexes
            .as_ref()
            .map(index_files::IndexManager::defined_indexes)
            .unwrap_or_default()
    }
    /// Removes an index created through the Admin API; false if there is none.
    pub fn remove_admin_index(&self, index: &composite::CompositeIndex) -> bool {
        self.indexes
            .as_ref()
            .is_some_and(|m| m.remove_admin_index(index))
    }
    /// Changes whenever the data is replaced by [`Engine::reset`] or [`Engine::restore`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
    /// The wall-clock `(create, update)` times in microseconds of entities written since startup
    /// (superset response timestamps); `None` for others.
    pub fn entity_times(&self, project_id: &str, keys: &[Key]) -> Vec<Option<(i64, i64)>> {
        let state = self.lock();
        let project = state.projects.get(project_id);
        keys.iter()
            .map(|k| project.and_then(|p| p.times.get(k).copied()))
            .collect()
    }
    /// The wall-clock time of commit `version` in microseconds, if recorded (superset).
    pub fn version_time(&self, project_id: &str, version: i64) -> Option<i64> {
        self.lock()
            .projects
            .get(project_id)
            .and_then(|p| p.version_time(version))
    }
    /// Every project with state, by name (the data viewer, superset).
    pub fn project_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().projects.keys().cloned().collect();
        ids.sort();
        ids
    }
    /// `(database, namespace, kind, entity count)` of a project's applied entities, sorted; kinds
    /// with reserved names (`__x__`) are left out (the data viewer, superset).
    pub fn kind_summary(&self, project_id: &str) -> Vec<(String, String, String, usize)> {
        let state = self.lock();
        let mut counts: BTreeMap<(String, String, String), usize> = BTreeMap::new();
        if let Some(project) = state.projects.get(project_id) {
            for key in project.entities.keys() {
                let kind = key.kind();
                if kind.starts_with("__") && kind.ends_with("__") {
                    continue;
                }
                *counts
                    .entry((
                        key.partition.database_id.to_string(),
                        key.partition.namespace_id.to_string(),
                        kind.to_string(),
                    ))
                    .or_default() += 1;
            }
        }
        counts
            .into_iter()
            .map(|((d, n, k), c)| (d, n, k, c))
            .collect()
    }
    /// Whether `project_id` has state (Java's profile): it was read or written since the last
    /// reset.
    pub fn has_project(&self, project_id: &str) -> bool {
        self.lock().projects.contains_key(project_id)
    }
    /// Drops all data and resets every counter except transaction handles.
    pub fn reset(&self) {
        *self.lock() = State::default();
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(indexes) = &self.indexes {
            indexes.clear_history();
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Runs `f` with the state and the job policy's apply decision.
    fn with_state<T>(&self, f: impl FnOnce(&mut State, &mut dyn FnMut() -> bool) -> T) -> T {
        let mut state = self.lock();
        let policy = self.config().job_policy;
        let mut rng = self.rng.lock().unwrap_or_else(|p| p.into_inner());
        let mut decide = move || match &policy {
            JobPolicy::ForceStrongConsistency => true,
            JobPolicy::Probability {
                unapplied_percentage,
                ..
            } => {
                let cutoff = (unapplied_percentage * 100.0) as i32;
                rng.next_int_bounded(RANDOM_CEILING) >= cutoff
            }
        };
        f(&mut state, &mut decide)
    }
    fn allocate(&self, state: &mut State, key: &mut Key) -> Option<Key> {
        if key.is_complete() {
            return None;
        }
        let id = match self.config().auto_id_policy {
            AutoIdPolicy::Sequential => state.next_counter(),
            AutoIdPolicy::Scattered => {
                state.scattered_counter += 1;
                to_scattered_id(state.scattered_counter - 1)
            }
        };
        key.path.last_mut().expect("validated non-empty path").id = Some(IdOrName::Id(id));
        Some(key.clone())
    }
    fn entity_group_entity(&self, project: &Project, key: &Key) -> Entity {
        let mut entity = Entity::new(key.clone());
        entity.properties.insert(
            "__version__".into(),
            oxide_model::Value::new(oxide_model::ValueKind::Integer(
                project.group_version(&self.read_group(key)) + self.base_version,
            )),
        );
        entity
    }
    /// Begins a transaction. `xg` allows several entity groups (always set by v1).
    pub fn begin_transaction(
        &self,
        project_id: &str,
        read_only: bool,
        xg: bool,
        previous: Option<u64>,
    ) -> Result<u64> {
        let mut state = self.lock();
        let project = state.projects.entry(project_id.to_string()).or_default();
        project.expire_txns();
        if let Some(previous) = previous {
            if read_only {
                return Err(Error::invalid_argument(
                    "specifying a previous transaction handle on a read-only transaction is not allowed",
                ));
            }
            if let Some(prev) = project.txns.get(&previous) {
                if prev.read_only {
                    return Err(Error::invalid_argument(
                        "Cannot retry a read-only transaction.",
                    ));
                }
                if prev.xg != xg {
                    return Err(Error::invalid_argument(
                        "Transaction options should be the same as specified previous transaction.",
                    ));
                }
                project.txns.remove(&previous);
            }
        }
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        project.txns.insert(handle, Txn::new(read_only, xg));
        Ok(handle)
    }
    pub fn rollback(&self, project_id: &str, handle: u64) -> Result<()> {
        let mut state = self.lock();
        let project = state
            .projects
            .get_mut(project_id)
            .ok_or_else(|| Error::new(Code::Unknown, ""))?;
        project.expire_txns();
        project
            .txns
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| Error::invalid_argument(TXN_INVALID))
    }
    /// The pseudo-group `__none__:1` every query run in a transaction tracks.
    fn none_group(q: &query::Query) -> Key {
        Key {
            partition: oxide_model::PartitionId {
                project_id: (&q.project_id).into(),
                ..Default::default()
            },
            path: vec![PathElement::new(NONE_KIND, Some(IdOrName::Id(1)))],
        }
    }
    /// Makes transaction `handle` track `group`. A group it already tracks is accepted as is. A new
    /// group is checked against the group limits, then every tracked group must still be at the
    /// version it had when first tracked (contention fails the read but leaves the transaction
    /// usable).
    fn track(&self, project: &mut Project, handle: u64, group: &Key) -> Result<()> {
        let current: Vec<(Key, i64)> = {
            let txn = project
                .txns
                .get(&handle)
                .ok_or_else(|| Error::invalid_argument(TXN_INVALID))?;
            if txn.failed {
                return Err(Error::invalid_argument(TXN_CLOSED));
            }
            if txn.trackers.iter().any(|(g, _)| g == group) {
                return Ok(());
            }
            let pseudo = |g: &Key| g.path.iter().any(|e| e.kind == NONE_KIND);
            if !pseudo(group) {
                let visible: Vec<&Key> = txn
                    .trackers
                    .iter()
                    .map(|(g, _)| g)
                    .filter(|g| !pseudo(g))
                    .collect();
                if !txn.xg {
                    if let Some(first) = visible.first() {
                        return Err(Error::invalid_argument(format!(
                            "cross-group transaction need to be explicitly specified, see \
                             TransactionOptions.Builder.withXGfound both {} and {}",
                            debug_path(&first.path),
                            debug_path(&group.path)
                        )));
                    }
                } else if !self.firestore && visible.len() >= MAX_GROUPS_PER_TXN {
                    return Err(Error::invalid_argument(
                        "operating on too many entity groups in a single transaction.",
                    ));
                }
            }
            txn.trackers.clone()
        };
        if current.iter().any(|(g, v)| project.group_version(g) != *v) {
            return Err(Error::new(Code::Aborted, CONTENTION));
        }
        let version = project.group_version(group);
        project
            .txns
            .get_mut(&handle)
            .expect("checked")
            .trackers
            .push((group.clone(), version));
        Ok(())
    }
    /// Reads `key` in transaction `handle`: from the snapshot of its group if someone wrote the
    /// group since the transaction tracked it, else the applied state.
    fn txn_read(&self, project: &Project, handle: u64, key: &Key) -> Option<(Entity, i64)> {
        let group = self.read_group(key);
        let stored = match project
            .txns
            .get(&handle)
            .and_then(|t| t.snapshots.get(&group))
        {
            Some(snapshot) => snapshot.get(key),
            None => project.entities.get(key),
        };
        stored.map(|s| (s.entity.clone(), s.version))
    }
    pub fn lookup(
        &self,
        project_id: &str,
        keys: &[Key],
        mode: ReadMode,
    ) -> Result<Vec<LookupResult>> {
        if keys.iter().any(|k| PSEUDO_KINDS.contains(&k.kind())) {
            return Err(Error::new(Code::Unknown, ""));
        }
        self.with_state(|state, decide| {
            let project = state.projects.entry(project_id.to_string()).or_default();
            if let ReadMode::At(t) = Self::effective_mode(project, mode)? {
                let version = project.version_at(t);
                return Ok(keys
                    .iter()
                    .map(
                        |key| match project.entity_at(key, &self.read_group(key), version) {
                            Some((entity, version)) => LookupResult::Found { entity, version },
                            None => LookupResult::Missing {
                                key: key.clone(),
                                version,
                            },
                        },
                    )
                    .collect());
            }
            let mut results = Vec::with_capacity(keys.len());
            for key in keys {
                let group = self.read_group(key);
                if let ReadMode::Transaction(h) = mode {
                    self.track(project, h, &group)?;
                }
                if !self.firestore
                    && key.kind() == pseudo::ENTITY_GROUP
                    && !is_entity_group_key(key)
                {
                    return Err(Error::new(Code::Unknown, ""));
                }
                let found = if !self.firestore && is_entity_group_key(key) {
                    Some((self.entity_group_entity(project, key), 0))
                } else {
                    match mode {
                        ReadMode::Eventual => project
                            .entities
                            .get(key)
                            .map(|s| (s.entity.clone(), s.version)),
                        ReadMode::Strong => {
                            project.roll_forward(&group);
                            project
                                .entities
                                .get(key)
                                .map(|s| (s.entity.clone(), s.version))
                        }
                        ReadMode::Transaction(h) => {
                            project.roll_forward(&group);
                            self.txn_read(project, h, key)
                        }
                        ReadMode::At(_) => unreachable!("handled above"),
                    }
                };
                results.push(match found {
                    Some((entity, version)) => LookupResult::Found { entity, version },
                    None => LookupResult::Missing {
                        key: key.clone(),
                        version: if mode == ReadMode::Eventual {
                            0
                        } else {
                            project.last_commit
                        },
                    },
                });
                project.groom(decide);
            }
            Ok(results)
        })
    }
    /// Runs a query. Non-ancestor queries see applied writes only; strong and transactional
    /// ancestor queries roll the ancestor's group forward first.
    pub fn run_query(&self, q: &query::Query, mode: ReadMode) -> Result<query::QueryBatch> {
        self.with_state(|state, decide| {
            let project = state.projects.entry(q.project_id.clone()).or_default();
            if let ReadMode::At(t) = Self::effective_mode(project, mode)? {
                return self.query_at(project, q, t);
            }
            let group = q.ancestor.as_ref().map(|a| self.read_group(a));
            let mut snapshot = None;
            if let Some(group) = &group {
                match mode {
                    ReadMode::At(_) => unreachable!("handled above"),
                    ReadMode::Eventual => {}
                    ReadMode::Strong => project.roll_forward(group),
                    ReadMode::Transaction(h) => {
                        self.track(project, h, group)?;
                        project.roll_forward(group);
                        snapshot = project
                            .txns
                            .get(&h)
                            .and_then(|t| t.snapshots.get(group))
                            .cloned();
                    }
                }
            }
            let stored = || {
                project
                    .entities
                    .iter()
                    .map(|(k, s)| (k, &s.entity, s.version))
            };
            let batch = match pseudo::entities(q, stored(), self.firestore)? {
                Some(synthetic) => query::run(
                    q,
                    synthetic.iter().map(|e| {
                        (
                            e.key.as_ref().expect("pseudo entities have keys"),
                            e,
                            0,
                            Form::default(),
                        )
                    }),
                    project.last_commit,
                )
                .map(|batch| query::QueryBatch {
                    snapshot_version: batch.snapshot_version.filter(|_| {
                        q.kind
                            .as_deref()
                            .is_some_and(|k| [pseudo::COLLECTION, pseudo::DOCUMENT].contains(&k))
                    }),
                    ..batch
                }),
                None => {
                    let of_kind = |(k, _): &(&Key, &store::Stored)| {
                        q.kind.as_ref().is_none_or(|kind| k.kind() == kind)
                    };
                    let mut candidates: Vec<(&Key, &store::Stored)> = match (&snapshot, &group) {
                        (Some(snapshot), Some(group)) => project
                            .entities
                            .iter()
                            .filter(|(k, _)| !store::in_group(k, group))
                            .chain(snapshot.iter())
                            .filter(of_kind)
                            .collect(),
                        _ => project.entities.iter().filter(of_kind).collect(),
                    };
                    candidates.sort_by_key(|(_, s)| s.seq);
                    query::run(
                        q,
                        candidates
                            .into_iter()
                            .map(|(k, s)| (k, &s.entity, s.version, s.form)),
                        project.last_commit,
                    )
                }
            }?;
            let mut batch = batch;
            if let ReadMode::Transaction(h) = mode {
                if group.is_none() {
                    for key in &batch.matched {
                        self.track(project, h, &self.read_group(key))?;
                    }
                }
                self.track(project, h, &Self::none_group(q))?;
                batch.snapshot_version = Some(project.last_commit);
            }
            if snapshot.is_none() {
                project.groom(decide);
            }
            if let Some(indexes) = &self.indexes {
                indexes.process(q)?;
            }
            Ok(batch)
        })
    }
    /// Runs `q` over the committed state as of `t` (microseconds).
    fn query_at(
        &self,
        project: &store::Project,
        q: &query::Query,
        t: i64,
    ) -> Result<query::QueryBatch> {
        let version = project.version_at(t);
        let mut keys: Vec<&Key> = project
            .entities
            .keys()
            .chain(project.history.keys())
            .collect();
        keys.extend(project.groups.values().flat_map(|g| g.written_keys()));
        keys.retain(|k| q.kind.as_ref().is_none_or(|kind| k.kind() == kind));
        keys.sort_by_key(|k| project.entities.get(*k).map_or(u64::MAX, |s| s.seq));
        keys.dedup();
        let mut seen = std::collections::HashSet::new();
        let rows: Vec<(Key, Entity, i64, cost::Form)> = keys
            .into_iter()
            .filter(|k| seen.insert(*k))
            .filter_map(|k| {
                let (entity, v) = project.entity_at(k, &self.read_group(k), version)?;
                let form = project.entities.get(k).map(|s| s.form).unwrap_or_default();
                Some((k.clone(), entity, v, form))
            })
            .collect();
        let mut batch = query::run(q, rows.iter().map(|(k, e, v, f)| (k, e, *v, *f)), version)?;
        batch.snapshot_version = Some(version);
        Ok(batch)
    }
    /// Applies mutations one at a time, each as its own job. On error, mutations before the
    /// failing one stay applied.
    pub fn commit_non_transactional(
        &self,
        project_id: &str,
        mutations: Vec<Mutation>,
    ) -> Result<CommitResult> {
        let indexes = self.composite_indexes();
        let request_time = now_micros();
        self.with_state(|state, decide| {
            let mut result = CommitResult::default();
            for mutation in mutations {
                let (key, write, allocated) = self.resolve(state, mutation.op);
                let project = state.projects.entry(project_id.to_string()).or_default();
                let group = self.write_group(&key, matches!(write, Write::Delete));
                if let Some(conflict) = check_mutation(
                    project.logical(&key, &group).map(|(_, v, _)| v),
                    &write,
                    mutation.base_version,
                    mutation.on_conflict == OnConflict::Fail,
                )? {
                    if mutation.on_conflict == OnConflict::Unrecognized {
                        project.next_timestamp();
                    }
                    result.mutation_results.push(MutationResult {
                        key: allocated,
                        ..conflict
                    });
                    continue;
                }
                let (entity, transform_results) = apply_partial(
                    &mutation.partial,
                    project.logical(&key, &group).map(|(e, _, _)| e),
                    write.into_entity(),
                    request_time,
                );
                project.bump_group(&group);
                let timestamp = project.next_timestamp();
                let scatter = self.config().auto_id_policy == AutoIdPolicy::Sequential;
                let (cost, versions) = project.add_job(
                    &group,
                    timestamp,
                    &[(key, entity)],
                    store::JobOptions {
                        scatter,
                        indexes: &indexes,
                        history: self.point_in_time,
                    },
                    decide,
                );
                result.index_updates += cost.index_writes;
                result.entity_writes += cost.entity_writes;
                result.mutation_results.push(MutationResult {
                    key: allocated,
                    version: versions[0],
                    conflict_detected: false,
                    transform_results,
                });
            }
            Ok(result)
        })
    }
    /// Commits transaction `handle` with `mutations` (Java's put/delete in the transaction followed
    /// by its commit): one commit timestamp, one job per tracked entity group.
    pub fn commit_transaction(
        &self,
        project_id: &str,
        handle: u64,
        mutations: Vec<Mutation>,
    ) -> Result<CommitResult> {
        let indexes = self.composite_indexes();
        self.with_state(|state, decide| {
            if !state.projects.contains_key(project_id) {
                return Err(Error::new(Code::Unknown, ""));
            }
            let request_time = now_micros();
            let mut results: Vec<(Key, MutationResult)> = Vec::new();
            for mutation in mutations {
                let (key, write, allocated) = self.resolve(state, mutation.op);
                let project = state.projects.get_mut(project_id).expect("checked");
                project.expire_txns();
                let txn = project
                    .txns
                    .get(&handle)
                    .ok_or_else(|| Error::invalid_argument(TXN_INVALID))?;
                if txn.failed {
                    return Err(Error::invalid_argument(TXN_CLOSED));
                }
                if txn.read_only {
                    return Err(Error::invalid_argument(
                        "Cannot modify entities in a read-only transaction.",
                    ));
                }
                self.track(
                    project,
                    handle,
                    &self.write_group(&key, matches!(write, Write::Delete)),
                )?;
                let txn = project.txns.get(&handle).expect("tracked");
                let seen = match txn.writes.iter().find(|(k, _)| *k == key) {
                    Some((_, w)) => w.as_ref().map(|_| 0),
                    None => self.txn_read(project, handle, &key).map(|(_, v)| v),
                };
                if let Some(conflict) = check_mutation(
                    seen,
                    &write,
                    mutation.base_version,
                    mutation.on_conflict == OnConflict::Fail,
                )? {
                    results.push((
                        key,
                        MutationResult {
                            key: allocated,
                            ..conflict
                        },
                    ));
                    continue;
                }
                let (entity, transform_results) = if mutation.partial.is_empty() {
                    (write.into_entity(), Vec::new())
                } else {
                    let existing = match txn.writes.iter().find(|(k, _)| *k == key) {
                        Some((_, w)) => w.clone(),
                        None => self.txn_read(project, handle, &key).map(|(e, _)| e),
                    };
                    apply_partial(
                        &mutation.partial,
                        existing.as_ref(),
                        write.into_entity(),
                        request_time,
                    )
                };
                project
                    .txns
                    .get_mut(&handle)
                    .expect("tracked")
                    .buffer(key.clone(), entity);
                results.push((
                    key,
                    MutationResult {
                        key: allocated,
                        version: 0,
                        conflict_detected: false,
                        transform_results,
                    },
                ));
            }
            let project = state.projects.get_mut(project_id).expect("checked");
            let txn = project
                .txns
                .remove(&handle)
                .ok_or_else(|| Error::invalid_argument(TXN_INVALID))?;
            if txn.failed {
                project.txns.insert(handle, txn);
                return Err(Error::invalid_argument(TXN_CLOSED));
            }
            if txn.writes.is_empty() {
                return Ok(CommitResult {
                    mutation_results: results.into_iter().map(|(_, r)| r).collect(),
                    actions: txn.actions,
                    ..Default::default()
                });
            }
            if txn
                .trackers
                .iter()
                .any(|(g, v)| project.group_version(g) != *v)
            {
                let mut failed = Txn::new(txn.read_only, txn.xg);
                failed.failed = true;
                project.txns.insert(handle, failed);
                return Err(Error::new(Code::Aborted, CONTENTION));
            }
            let timestamp = project.next_timestamp();
            let mut version_of: HashMap<Key, i64> = HashMap::new();
            let mut total = JobCost::default();
            for (group, _) in &txn.trackers {
                project.bump_group(group);
                let writes: Vec<(Key, Option<Entity>)> = txn
                    .writes
                    .iter()
                    .filter(|(k, w)| self.write_group(k, w.is_none()) == *group)
                    .cloned()
                    .collect();
                let scatter = self.config().auto_id_policy == AutoIdPolicy::Sequential;
                let (cost, versions) = project.add_job(
                    group,
                    timestamp,
                    &writes,
                    store::JobOptions {
                        scatter,
                        indexes: &indexes,
                        history: self.point_in_time,
                    },
                    decide,
                );
                total += cost;
                for ((key, _), v) in writes.into_iter().zip(versions) {
                    version_of.insert(key, v);
                }
            }
            let mutation_results: Vec<MutationResult> = results
                .into_iter()
                .map(|(key, r)| {
                    if r.conflict_detected {
                        r
                    } else {
                        MutationResult {
                            version: version_of[&key],
                            ..r
                        }
                    }
                })
                .collect();
            let commit_version = mutation_results
                .iter()
                .map(|r| r.version)
                .max()
                .unwrap_or(0);
            let key_versions = (txn.writes.iter().filter(|(_, w)| w.is_some()))
                .chain(txn.writes.iter().filter(|(_, w)| w.is_none()))
                .map(|(k, _)| (k.clone(), version_of[k]))
                .collect();
            Ok(CommitResult {
                mutation_results,
                index_updates: total.index_writes,
                entity_writes: total.entity_writes,
                commit_version,
                key_versions,
                actions: txn.actions,
            })
        })
    }
    /// Completes the key (allocating an id if needed) and splits a mutation into key and write.
    fn resolve(&self, state: &mut State, op: Op) -> (Key, Write, Option<Key>) {
        let (mut key, write) = match op {
            Op::Delete(key) => (key, Write::Delete),
            Op::Insert(e) => (entity_key(&e), Write::Insert(e)),
            Op::Update(e) => (entity_key(&e), Write::Update(e)),
            Op::Upsert(e) => (entity_key(&e), Write::Upsert(e)),
        };
        let allocated = self.allocate(state, &mut key);
        let write = match write {
            Write::Insert(mut e) => {
                e.key = Some(key.clone());
                Write::Insert(e)
            }
            Write::Update(mut e) => {
                e.key = Some(key.clone());
                Write::Update(e)
            }
            Write::Upsert(mut e) => {
                e.key = Some(key.clone());
                Write::Upsert(e)
            }
            Write::Delete => Write::Delete,
        };
        (key, write, allocated)
    }
    /// Gives an incomplete key an id under the configured auto-id policy.
    pub fn complete_key(&self, key: &mut Key) {
        let mut state = self.lock();
        self.allocate(&mut state, key);
    }
    /// Non-transactional App Engine v3 writes (`None` = delete) of complete keys: one job per
    /// entity group, in the order the groups first appear, each with its own commit timestamp.
    pub fn write_groups(
        &self,
        project_id: &str,
        writes: Vec<(Key, Option<Entity>)>,
    ) -> WriteResult {
        let indexes = self.composite_indexes();
        let scatter = self.config().auto_id_policy == AutoIdPolicy::Sequential;
        self.with_state(|state, decide| {
            let project = state.projects.entry(project_id.to_string()).or_default();
            let mut groups: Vec<(Key, Vec<usize>)> = Vec::new();
            for (i, (key, write)) in writes.iter().enumerate() {
                let group = self.write_group(key, write.is_none());
                match groups.iter_mut().find(|(g, _)| *g == group) {
                    Some((_, members)) => members.push(i),
                    None => groups.push((group, vec![i])),
                }
            }
            let mut versions = vec![0; writes.len()];
            let mut cost = JobCost::default();
            for (group, members) in groups {
                let job: Vec<(Key, Option<Entity>)> =
                    members.iter().map(|&i| writes[i].clone()).collect();
                project.bump_group(&group);
                let timestamp = project.next_timestamp();
                let (job_cost, job_versions) = project.add_job(
                    &group,
                    timestamp,
                    &job,
                    store::JobOptions {
                        scatter,
                        indexes: &indexes,
                        history: self.point_in_time,
                    },
                    decide,
                );
                cost += job_cost;
                for (i, v) in members.into_iter().zip(job_versions) {
                    versions[i] = v;
                }
            }
            WriteResult {
                keys: writes.into_iter().map(|(k, _)| k).collect(),
                versions,
                cost,
            }
        })
    }
    /// Buffers App Engine v3 writes (`None` = delete) of complete keys in transaction `handle`.
    pub fn buffer_writes(
        &self,
        project_id: &str,
        handle: u64,
        writes: Vec<(Key, Option<Entity>)>,
    ) -> Result<Vec<Key>> {
        let mut state = self.lock();
        let project = state.projects.entry(project_id.to_string()).or_default();
        project.expire_txns();
        let mut keys = Vec::with_capacity(writes.len());
        for (key, write) in writes {
            let txn = project
                .txns
                .get(&handle)
                .ok_or_else(|| Error::invalid_argument(TXN_INVALID))?;
            if txn.read_only {
                return Err(Error::invalid_argument(
                    "Cannot modify entities in a read-only transaction.",
                ));
            }
            self.track(project, handle, &self.write_group(&key, write.is_none()))?;
            project
                .txns
                .get_mut(&handle)
                .expect("tracked")
                .buffer(key.clone(), write);
            keys.push(key);
        }
        Ok(keys)
    }
    /// Most tasks one transaction may hold.
    const MAX_ACTIONS: usize = 5;
    pub fn add_actions(&self, project_id: &str, handle: u64, actions: Vec<Vec<u8>>) -> Result<()> {
        let mut state = self.lock();
        let project = state
            .projects
            .get_mut(project_id)
            .ok_or_else(|| Error::new(Code::Unknown, ""))?;
        project.expire_txns();
        let txn = project
            .txns
            .get_mut(&handle)
            .ok_or_else(|| Error::invalid_argument(TXN_INVALID))?;
        if txn.failed {
            return Err(Error::invalid_argument(TXN_CLOSED));
        }
        if txn.actions.len() + actions.len() > Self::MAX_ACTIONS {
            return Err(Error::invalid_argument(format!(
                "Too many messages, maximum allowed: {}",
                Self::MAX_ACTIONS
            )));
        }
        txn.actions.extend(actions);
        Ok(())
    }
    /// App Engine v3 `AllocateIds` on the sequential counter: `size` ids from the counter (a
    /// negative size moves it back), or else every id up to `max`. Returns the inclusive range
    /// `(start, end)`, empty when `end < start`.
    pub fn allocate_range(&self, size: Option<i64>, max: i64) -> (i64, i64) {
        let mut state = self.lock();
        let start = state.id_counter;
        match size {
            Some(size) => {
                state.id_counter = start.wrapping_add(size);
                (start, start.wrapping_add(size).wrapping_sub(1))
            }
            None => {
                if start <= max {
                    state.id_counter = max + 1;
                }
                (start, max.max(start - 1))
            }
        }
    }
    /// Assigns scattered ids to incomplete keys (always scattered, whatever the auto-id policy).
    pub fn allocate_ids(&self, mut keys: Vec<Key>) -> Vec<Key> {
        let mut state = self.lock();
        for key in &mut keys {
            let id = to_scattered_id(state.next_counter());
            if let Some(last) = key.path.last_mut() {
                last.id = Some(IdOrName::Id(id));
            }
        }
        keys
    }
    /// Reserving ids has no effect in the Java emulator: the shared counter is not advanced.
    pub fn reserve_ids(&self, _keys: &[Key]) {}
}
/// Applies a mutation's mask and transforms to the entity it writes.
fn apply_partial(
    partial: &transform::PartialWrite,
    existing: Option<&Entity>,
    written: Option<Entity>,
    request_time: i64,
) -> (Option<Entity>, Vec<Value>) {
    match written {
        Some(e) if !partial.is_empty() => {
            let (e, results) = partial.apply(existing, e, request_time);
            (Some(e), results)
        }
        other => (other, Vec::new()),
    }
}

use store::now_micros;

/// Point-in-time reads reach back as far as the history is kept.
fn check_read_time(t: i64) -> Result<()> {
    if t < now_micros() - store::HISTORY_RETENTION_MICROS {
        return Err(Error::invalid_argument(
            "The read_time is older than the one-hour version retention period.",
        ));
    }
    Ok(())
}

enum Write {
    Insert(Entity),
    Update(Entity),
    Upsert(Entity),
    Delete,
}
impl Write {
    fn into_entity(self) -> Option<Entity> {
        match self {
            Write::Insert(e) | Write::Update(e) | Write::Upsert(e) => Some(e),
            Write::Delete => None,
        }
    }
}
/// Base-version and existence checks for one mutation against the version it would overwrite.
/// Returns `Some(result)` for a conflict (the mutation is skipped).
fn check_mutation(
    existing: Option<i64>,
    write: &Write,
    base_version: Option<i64>,
    fail_on_conflict: bool,
) -> Result<Option<MutationResult>> {
    if let Some(base) = base_version {
        let conflict = |version: i64| {
            if fail_on_conflict {
                Err(Error::new(
                    Code::FailedPrecondition,
                    format!(
                        "the stored version ({version}) does not match the required base version ({base})"
                    ),
                ))
            } else {
                Ok(Some(MutationResult {
                    key: None,
                    version,
                    conflict_detected: true,
                    transform_results: Vec::new(),
                }))
            }
        };
        match existing {
            Some(v) if base > v => {
                return Err(Error::invalid_argument(
                    "Invalid base version, it is greater than the stored version",
                ));
            }
            Some(v) if base < v => return conflict(v),
            None if base > 0 => return conflict(0),
            _ => {}
        }
    }
    match (write, existing) {
        (Write::Insert(_), Some(_)) => {
            Err(Error::new(Code::AlreadyExists, "entity already exists"))
        }
        (Write::Update(_), None) => Err(Error::new(Code::NotFound, "no entity to update")),
        _ => Ok(None),
    }
}
/// A path element in protobuf text format, as Java prints a `Path.Element` message.
fn debug_element(e: &PathElement) -> String {
    let id = match &e.id {
        Some(IdOrName::Id(id)) => format!("  id: {id}\n"),
        Some(IdOrName::Name(name)) => format!("  name: {}\n", debug_quote(name)),
        None => String::new(),
    };
    format!("Element {{\n  type: {}\n{id}}}\n", debug_quote(&e.kind))
}
/// A path in protobuf text format: its elements, one after the other.
fn debug_path(path: &[PathElement]) -> String {
    path.iter().map(debug_element).collect()
}
/// A string as protobuf text format quotes it: C escapes, non-printable and non-ASCII bytes in
/// octal.
pub fn debug_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for &b in s.as_bytes() {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'"' => out.push_str("\\\""),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
}
/// `<root>/__entity_group__:1` reads the entity group's version (Datastore mode only).
fn is_entity_group_key(key: &Key) -> bool {
    key.path.len() == 2
        && key.path[1].kind == pseudo::ENTITY_GROUP
        && key.path[1].id == Some(IdOrName::Id(1))
}
fn entity_key(entity: &Entity) -> Key {
    entity
        .key
        .clone()
        .expect("validated: mutation entities have keys")
}
#[cfg(test)]
mod tests {
    use super::*;
    use oxide_model::{PartitionId, PathElement, Value, ValueKind};
    fn key(kind: &str, id: Option<IdOrName>) -> Key {
        Key {
            partition: PartitionId {
                project_id: "p".into(),
                ..Default::default()
            },
            path: vec![PathElement::new(kind, id)],
        }
    }
    fn named(name: &str) -> Key {
        key("K", Some(IdOrName::Name(name.into())))
    }
    fn upsert(k: Key, props: &[(&str, i64)]) -> Mutation {
        let mut e = Entity::new(k);
        for (n, v) in props {
            e.properties
                .insert((*n).into(), Value::new(ValueKind::Integer(*v)));
        }
        Mutation {
            op: Op::Upsert(e),
            base_version: None,
            on_conflict: OnConflict::ServerValue,
            partial: Default::default(),
        }
    }
    #[test]
    fn scattered_ids_match_java() {
        assert_eq!(to_scattered_id(1), 5629499534213120);
        assert_eq!(to_scattered_id(2), 5066549580791808);
        assert_eq!(to_scattered_id(5), 5910974510923776);
    }
    #[test]
    fn versions_follow_commit_timestamps() {
        let engine = Engine::new(Config::default());
        let r = engine
            .commit_non_transactional("p", vec![upsert(named("a"), &[("v", 1)])])
            .unwrap();
        assert_eq!(r.mutation_results[0].version, 2);
        let r = engine
            .commit_non_transactional("p", vec![upsert(named("a"), &[("v", 1)])])
            .unwrap();
        assert_eq!((r.mutation_results[0].version, r.index_updates), (2, 0));
        let r = engine
            .commit_non_transactional("p", vec![upsert(named("a"), &[("v", 2)])])
            .unwrap();
        assert_eq!((r.mutation_results[0].version, r.index_updates), (4, 4));
        let r = engine
            .commit_non_transactional("q", vec![upsert(named("a"), &[])])
            .unwrap();
        assert_eq!(r.mutation_results[0].version, 2);
        let found = engine
            .lookup("p", &[named("a"), named("missing")], ReadMode::Strong)
            .unwrap();
        assert!(matches!(&found[0], LookupResult::Found { version: 4, .. }));
        assert!(matches!(
            &found[1],
            LookupResult::Missing { version: 4, .. }
        ));
    }
    #[test]
    fn sequential_ids_share_one_counter_with_allocate_ids() {
        let engine = Engine::new(Config::default());
        let r = engine
            .commit_non_transactional("p", vec![upsert(key("K", None), &[])])
            .unwrap();
        assert_eq!(
            r.mutation_results[0].key.as_ref().unwrap().path[0].id,
            Some(IdOrName::Id(1))
        );
        let ids = engine.allocate_ids(vec![key("K", None)]);
        assert_eq!(ids[0].path[0].id, Some(IdOrName::Id(to_scattered_id(2))));
        let r = engine
            .commit_non_transactional("q", vec![upsert(key("K", None), &[])])
            .unwrap();
        assert_eq!(
            r.mutation_results[0].key.as_ref().unwrap().path[0].id,
            Some(IdOrName::Id(3))
        );
    }
    #[test]
    fn failed_mutation_keeps_earlier_writes() {
        let engine = Engine::new(Config::default());
        let insert = |k| Mutation {
            op: Op::Insert(Entity::new(k)),
            base_version: None,
            on_conflict: OnConflict::ServerValue,
            partial: Default::default(),
        };
        engine
            .commit_non_transactional("p", vec![insert(named("x"))])
            .unwrap();
        let err = engine
            .commit_non_transactional("p", vec![upsert(named("y"), &[]), insert(named("x"))])
            .unwrap_err();
        assert_eq!(err.code, Code::AlreadyExists);
        assert!(matches!(
            engine.lookup("p", &[named("y")], ReadMode::Strong).unwrap()[0],
            LookupResult::Found { version: 3, .. }
        ));
    }
    #[test]
    fn base_version_conflicts() {
        let engine = Engine::new(Config::default());
        engine
            .commit_non_transactional("p", vec![upsert(named("e"), &[])])
            .unwrap();
        let with_base = |base| Mutation {
            base_version: Some(base),
            ..upsert(named("e"), &[("x", 1)])
        };
        let r = engine
            .commit_non_transactional("p", vec![with_base(1)])
            .unwrap();
        assert_eq!(
            r.mutation_results[0],
            MutationResult {
                key: None,
                version: 2,
                conflict_detected: true,
                transform_results: vec![],
            }
        );
        assert_eq!(
            engine
                .commit_non_transactional("p", vec![with_base(3)])
                .unwrap_err()
                .code,
            Code::InvalidArgument
        );
        let r = engine
            .commit_non_transactional("p", vec![with_base(2)])
            .unwrap();
        assert_eq!(r.mutation_results[0].version, 3);
    }
}
