//! The App Engine v3 API server (`apphosting.tools.devappserver2.CallHandler`): v3 datastore calls
//! in a generic envelope, run against the same engine as the v1 API. See
//! `docs/porting-notes/appengine-v3.md`.

#![allow(deprecated)]

pub mod actions;
pub mod appid;
pub mod entity;
pub mod query;
pub mod wire;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use oxide_engine::composite::CompositeIndex;
use oxide_engine::{Code, Engine, LookupResult, ReadMode};
use oxide_model::{Entity, Key};
use oxide_proto::apphosting::tools::devappserver2 as das;
use oxide_proto::apphosting_datastore_v3 as v3;
use oxide_proto::storage_onestore_v3 as os;
use prost::Message;
use tonic::{Request, Response, Status};

use appid::{Resolver, Unscrubbable};
use entity::text;
use query::LiveQuery;
use wire::WireError;

/// How a call fails: an `application_error` (datastore error code), an `rpc_error`, or a bare
/// gRPC UNKNOWN (Java's uncaught exceptions).
#[derive(Debug)]
pub enum Fail {
    App(i32, String),
    Rpc(i32, String),
    Unknown,
}

const BAD_REQUEST: i32 = v3::error::ErrorCode::BadRequest as i32;
const CONCURRENT_TRANSACTION: i32 = v3::error::ErrorCode::ConcurrentTransaction as i32;
const INTERNAL_ERROR: i32 = v3::error::ErrorCode::InternalError as i32;
const NEED_INDEX: i32 = v3::error::ErrorCode::NeedIndex as i32;
const PARSE_ERROR: i32 = das::rpc_error::ErrorCode::ParseError as i32;
/// A Get with `allow_deferred` defers the keys after this many response bytes (superset).
const DEFERRAL_BYTES: usize = 1 << 20;

impl From<oxide_engine::Error> for Fail {
    fn from(e: oxide_engine::Error) -> Self {
        match e.code {
            Code::Unknown => Fail::Unknown,
            Code::Aborted => Fail::App(CONCURRENT_TRANSACTION, e.message),
            Code::FailedPrecondition => Fail::App(NEED_INDEX, e.message),
            Code::InvalidArgument | Code::NotFound | Code::AlreadyExists => {
                Fail::App(BAD_REQUEST, e.message)
            }
        }
    }
}

impl From<Unscrubbable> for Fail {
    fn from(_: Unscrubbable) -> Self {
        Fail::Unknown
    }
}

impl From<WireError> for Fail {
    fn from(e: WireError) -> Self {
        Fail::Rpc(
            PARSE_ERROR,
            match e {
                WireError::Malformed => wire::MALFORMED,
                WireError::MissingRequired => wire::MISSING_REQUIRED,
            }
            .into(),
        )
    }
}

fn bad_request(message: impl Into<String>) -> Fail {
    Fail::App(BAD_REQUEST, message.into())
}

type Outcome = Result<Vec<u8>, Fail>;

/// Live query snapshots by project and handle, valid for one generation of the engine's data.
#[derive(Default)]
struct Queries {
    generation: u64,
    live: HashMap<(String, u64), LiveQuery>,
}

pub struct Service {
    engine: Arc<Engine>,
    /// Production behavior the Java emulator lacks (docs/superset-backlog.md).
    superset: bool,
    resolver: Resolver,
    queries: Mutex<Queries>,
    /// Query handles: one counter for all projects that survives `/reset`.
    next_query: AtomicU64,
}

impl Service {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            superset: false,
            resolver: Resolver::default(),
            queries: Mutex::new(Queries::default()),
            next_query: AtomicU64::new(0),
        }
    }

    pub fn with_superset(self, superset: bool) -> Self {
        Self { superset, ..self }
    }

    /// Java's uncaught exception (bare UNKNOWN), or with the superset a BAD_REQUEST saying why.
    fn invalid(&self, message: impl Into<String>) -> Fail {
        if self.superset {
            bad_request(message)
        } else {
            Fail::Unknown
        }
    }

    pub fn into_server(self) -> das::call_handler_server::CallHandlerServer<Self> {
        das::call_handler_server::CallHandlerServer::new(self)
            .max_decoding_message_size(usize::MAX)
            .max_encoding_message_size(usize::MAX)
    }

    fn decode<M: Message + Default>(name: &str, bytes: &[u8]) -> Result<M, Fail> {
        Ok(wire::decode(
            &wire::descriptor(&format!("apphosting_datastore_v3.{name}")),
            bytes,
        )?)
    }

    fn project(&self, app: &[u8]) -> Result<String, Fail> {
        self.resolver
            .to_project_id(&text(app))
            .map_err(|_| self.invalid(format!("Invalid app id: {}", text(app))))
    }

    /// Maps the app ids of an inbound entity to project ids.
    fn inbound(&self, e: &mut os::EntityProto) -> Result<(), Fail> {
        entity::map_apps(e, &mut |app| {
            Ok(self.resolver.to_project_id(&text(app))?.into())
        })?;
        Ok(())
    }

    /// An entity as v3 reads return it, with app ids for project ids (`app` for its key).
    fn read_proto(&self, e: &Entity, app: &str) -> Result<os::EntityProto, Fail> {
        let mut proto = entity::stored_proto(e);
        entity::map_apps(&mut proto, &mut |project| {
            Ok(self.resolver.to_app_id(&text(project)).into())
        })?;
        proto.key.app = app.to_string().into();
        Ok(proto)
    }

    fn queries(&self) -> std::sync::MutexGuard<'_, Queries> {
        let mut queries = self.queries.lock().unwrap_or_else(|p| p.into_inner());
        let generation = self.engine.generation();
        if queries.generation != generation {
            queries.live.clear();
            queries.generation = generation;
        }
        queries
    }

    async fn call(&self, method: &str, request: &[u8], hostport: &str) -> Outcome {
        match method {
            "Get" => self.get(Self::decode("GetRequest", request)?),
            "Put" => self.put(Self::decode("PutRequest", request)?),
            "Delete" => self.delete(Self::decode("DeleteRequest", request)?),
            "RunQuery" => self.run_query(Self::decode("Query", request)?),
            "Next" => self.next(Self::decode("NextRequest", request)?),
            "BeginTransaction" => self.begin(Self::decode("BeginTransactionRequest", request)?),
            "Commit" => {
                self.commit(Self::decode("Transaction", request)?, hostport)
                    .await
            }
            "Rollback" => self.rollback(Self::decode("Transaction", request)?),
            "AllocateIds" => self.allocate_ids(Self::decode("AllocateIdsRequest", request)?),
            "GetIndices" => self.get_indices(Self::decode("GetIndicesRequest", request)?),
            "AddActions" => self.add_actions(request),
            "Touch" if self.superset => self.touch(Self::decode("TouchRequest", request)?),
            "DeleteCursor" if self.superset => self.delete_cursor(Self::decode("Cursor", request)?),
            "CreateIndex" | "UpdateIndex" | "DeleteIndex" if self.superset => {
                self.index_call(method, request)
            }
            _ => Err(bad_request(format!(
                "Cannot find definition for method: {method}"
            ))),
        }
    }

    fn get(&self, r: v3::GetRequest) -> Outcome {
        let mut keys = Vec::with_capacity(r.key.len());
        for k in &r.key {
            let project = self.project(&k.app)?;
            entity::check_read_key(k).map_err(bad_request)?;
            keys.push((entity::key_of(k, &project), project));
        }
        let read_time = r.read_time_epoch_micros.filter(|_| self.superset);
        let mode = match (&r.transaction, read_time) {
            (Some(t), _) => ReadMode::Transaction(t.handle),
            (None, Some(t)) => ReadMode::At(t),
            (None, None) if r.strong == Some(false) => ReadMode::Eventual,
            (None, None) => ReadMode::Strong,
        };
        let mut response = v3::GetResponse {
            read_time_epoch_micros: read_time,
            ..Default::default()
        };
        // With allow_deferred, keys past this response size are deferred (superset).
        let defer = self.superset && r.allow_deferred == Some(true);
        let mut size = 0;
        for (key, project) in keys {
            if defer && size > DEFERRAL_BYTES {
                response
                    .deferred
                    .push(entity::reference(&key, &self.resolver.to_app_id(&project)));
                response.in_order = Some(false);
                continue;
            }
            let app = self.resolver.to_app_id(&project);
            let result = self
                .engine
                .lookup(&project, std::slice::from_ref(&key), mode)?
                .pop()
                .expect("one result per key");
            response.entity.push(match result {
                LookupResult::Found { entity, version } => v3::get_response::Entity {
                    entity: Some(self.read_proto(&entity, &app)?),
                    version: Some(version),
                    ..Default::default()
                },
                LookupResult::Missing { key, version } => v3::get_response::Entity {
                    key: Some(entity::reference(&key, &app)),
                    version: (mode != ReadMode::Eventual).then_some(version),
                    ..Default::default()
                },
            });
            if defer {
                size += response.entity.last().map_or(0, Message::encoded_len);
            }
        }
        Ok(response.encode_to_vec())
    }

    /// Validates an entity of a Put and turns it into its stored form.
    fn stored(&self, mut e: os::EntityProto, project: &str) -> Result<Entity, Fail> {
        let path = &e.key.path.element;
        if path.is_empty() {
            return Err(bad_request(
                "A OnestoreEntity.Path must have at least one Element.",
            ));
        }
        if let Some(el) = path[..path.len() - 1]
            .iter()
            .find(|e| entity::incomplete(e))
        {
            return Err(self.invalid(format!(
                "Incomplete key.path.element: type: {}\n",
                oxide_engine::debug_quote(&text(&el.r#type))
            )));
        }
        if path
            .iter()
            .any(|el| el.name.as_ref().is_some_and(|n| n.len() > 1500))
        {
            return Err(bad_request(
                "The key path element name is longer than 1500 bytes.",
            ));
        }
        for p in e.property.iter().chain(&e.raw_property) {
            let name = text(&p.name);
            if name.len() >= 4 && name.starts_with("__") && name.ends_with("__") {
                return Err(bad_request(format!("illegal property.name: {name}")));
            }
        }
        for p in &e.property {
            let meaning = p.meaning.unwrap_or(0);
            if meaning == entity::ENTITY_PROTO
                && let Some(bytes) = &p.value.string_value
                && os::EntityProto::decode(bytes.as_ref()).is_err()
            {
                return Err(self.invalid(format!(
                    "Invalid embedded entity in property {}.",
                    text(&p.name)
                )));
            }
            if ![entity::BLOB, entity::TEXT, entity::ENTITY_PROTO].contains(&meaning)
                && p.value
                    .string_value
                    .as_ref()
                    .is_some_and(|s| s.len() > 1500)
            {
                return Err(bad_request(format!(
                    "string property {} is too long.  It cannot exceed 1500 bytes.",
                    text(&p.name)
                )));
            }
        }
        self.inbound(&mut e)?;
        // Users without an obfuscated id get one derived from their email (observed: the
        // decimal `String.hashCode()` of the email).
        for p in e.property.iter_mut().chain(e.raw_property.iter_mut()) {
            if let Some(user) = &mut p.value.uservalue
                && user.obfuscated_gaiaid.is_none()
            {
                let hash = oxide_model::java_string_hash(&text(&user.email));
                user.obfuscated_gaiaid = Some(hash.to_string().into());
            }
        }
        let mut key = entity::key_of(&e.key, project);
        self.engine.complete_key(&mut key);
        e.key = entity::reference(&key, project);
        if e.entity_group.element.is_empty() {
            e.entity_group = entity::path_of(&key.path[..1]);
        }
        let e = entity::read_form(e);
        let mut stored = entity::view_of(&e);
        stored.v3 = Some(e.encode_to_vec().into());
        Ok(stored)
    }

    fn put(&self, r: v3::PutRequest) -> Outcome {
        let Some(first) = r.entity.first() else {
            return Ok(Vec::new());
        };
        if first.key.app.is_empty() {
            return Err(self.invalid("A OnestoreEntity.Reference must set app."));
        }
        let project = self.project(&first.key.app)?;
        let app = self.resolver.to_app_id(&project);
        let mut writes = Vec::with_capacity(r.entity.len());
        for e in r.entity {
            let entity = self.stored(e, &project)?;
            writes.push((
                entity.key.clone().expect("stored entities have keys"),
                Some(entity),
            ));
        }
        let response = self.write(&project, &app, r.transaction.as_ref(), writes)?;
        Ok(v3::PutResponse {
            key: response.0,
            cost: Some(response.1),
            version: response.2,
            ..Default::default()
        }
        .encode_to_vec())
    }

    fn delete(&self, r: v3::DeleteRequest) -> Outcome {
        let Some(first) = r.key.first() else {
            return Ok(Vec::new());
        };
        let project = self.project(&first.app)?;
        let app = self.resolver.to_app_id(&project);
        let mut writes = Vec::with_capacity(r.key.len());
        for k in &r.key {
            self.project(&k.app)?;
            if let Some(e) = k.path.element.iter().find(|e| entity::incomplete(e)) {
                return Err(bad_request(format!(
                    "Incomplete key.path.element: type: {}\n",
                    oxide_engine::debug_quote(&text(&e.r#type))
                )));
            }
            writes.push((entity::key_of(k, &project), None));
        }
        let response = self.write(&project, &app, r.transaction.as_ref(), writes)?;
        Ok(v3::DeleteResponse {
            cost: Some(response.1),
            version: response.2,
            ..Default::default()
        }
        .encode_to_vec())
    }

    /// Writes (`None` = delete) outside a transaction, or buffers them in one. Returns the keys,
    /// the cost and the versions.
    fn write(
        &self,
        project: &str,
        app: &str,
        txn: Option<&v3::Transaction>,
        writes: Vec<(Key, Option<Entity>)>,
    ) -> Result<(Vec<os::Reference>, v3::Cost, Vec<i64>), Fail> {
        match txn {
            Some(t) => {
                let keys = self.engine.buffer_writes(project, t.handle, writes)?;
                Ok((
                    keys.iter().map(|k| entity::reference(k, app)).collect(),
                    v3::Cost::default(),
                    Vec::new(),
                ))
            }
            None => {
                let result = self.engine.write_groups(project, writes);
                Ok((
                    result
                        .keys
                        .iter()
                        .map(|k| entity::reference(k, app))
                        .collect(),
                    cost(result.cost.index_writes, result.cost.entity_writes),
                    result.versions,
                ))
            }
        }
    }

    fn run_query(&self, q: v3::Query) -> Outcome {
        if q.app.is_empty() {
            return Err(self.invalid("The query must set app."));
        }
        let project = self.project(&q.app)?;
        let app = self.resolver.to_app_id(&project);
        if q.count.is_some_and(|c| c < 0) {
            return Err(self.invalid("The query count must not be negative."));
        }
        if q.limit.is_some_and(|l| l < 0) {
            return Err(self.invalid("The query limit must not be negative."));
        }
        let mut converted = q.clone();
        converted.app = project.clone().into();
        if let Some(ancestor) = &mut converted.ancestor {
            ancestor.app = self.project(&ancestor.app)?.into();
        }
        for f in &mut converted.filter {
            for p in &mut f.property {
                if let Some(r) = &mut p.value.referencevalue {
                    r.app = self.project(&r.app)?.into();
                }
            }
        }
        let mut mode = ReadMode::Strong;
        if let Some(t) = &converted.transaction {
            let txn_project = self.project(&t.app)?;
            if txn_project != project {
                return Err(Fail::App(
                    INTERNAL_ERROR,
                    format!("Can't query app {project}in a transaction on app {txn_project}"),
                ));
            }
            if converted.ancestor.is_none() && !self.engine.firestore_mode() {
                return Err(bad_request(
                    "Only ancestor queries are allowed inside transactions.",
                ));
            }
            mode = ReadMode::Transaction(t.handle);
        }
        let read_time = if self.superset && converted.transaction.is_none() {
            converted.read_time_epoch_micros.or(converted.read_time_us)
        } else {
            None
        };
        if let Some(t) = read_time {
            mode = ReadMode::At(t);
        }
        let (engine_query, nothing) = query::convert(self, &converted, &project)?;
        let batch = self.engine.run_query(&engine_query, mode)?;
        let mut rows = if nothing { Vec::new() } else { batch.results };
        if let Some(search) = converted.search_query.as_ref().filter(|_| self.superset) {
            rows = self.search(&project, &text(search), rows, mode)?;
        }
        let mut live = LiveQuery::new(&project, &engine_query, rows, &converted);
        let compile = q.compile == Some(true);
        let mut batch = live.next(self, None, None, compile)?;
        batch.result.read_time_epoch_micros = read_time;
        if compile {
            batch.result.compiled_query = Some(query::compiled_query(&converted));
        }
        if batch.more {
            let handle = self.next_query.fetch_add(1, Ordering::SeqCst);
            self.queries().live.insert((project, handle), live);
            batch.result.cursor = Some(v3::Cursor {
                cursor: handle,
                app: Some(app.into()),
                database_id: None,
            });
        }
        Ok(batch.result.encode_to_vec())
    }

    fn next(&self, r: v3::NextRequest) -> Outcome {
        let expired = "query has expired or is invalid. Please restart it with the last cursor to read more results.";
        let Some(app) = &r.cursor.app else {
            return Err(self.invalid("The cursor must set app."));
        };
        let project = self.project(app)?;
        if !self.engine.has_project(&project) {
            return Err(self.invalid(expired));
        }
        if r.count.is_some_and(|c| c < 0) {
            return Err(self.invalid("The count must not be negative."));
        }
        let mut queries = self.queries();
        let key = (project.clone(), r.cursor.cursor);
        let Some(live) = queries.live.get_mut(&key) else {
            return Err(bad_request(
                "query has expired or is invalid. Please restart it with the last cursor to read more results.",
            ));
        };
        let mut batch = live.next(self, r.offset, r.count, r.compile == Some(true))?;
        if batch.more {
            let mut cursor = r.cursor.clone();
            cursor.app = Some(self.resolver.to_app_id(&project).into());
            batch.result.cursor = Some(cursor);
        } else {
            queries.live.remove(&key);
        }
        Ok(batch.result.encode_to_vec())
    }

    fn begin(&self, r: v3::BeginTransactionRequest) -> Outcome {
        if r.app.is_empty() {
            return Err(self.invalid("The transaction must set app."));
        }
        let project = self.project(&r.app)?;
        let read_only =
            r.mode == Some(v3::begin_transaction_request::TransactionMode::ReadOnly as i32);
        let handle = self.engine.begin_transaction(
            &project,
            read_only,
            r.allow_multiple_eg == Some(true),
            r.previous_transaction.as_ref().map(|t| t.handle),
        )?;
        Ok(v3::Transaction {
            handle,
            app: r.app,
            ..Default::default()
        }
        .encode_to_vec())
    }

    async fn commit(&self, t: v3::Transaction, hostport: &str) -> Outcome {
        let project = self.project(&t.app)?;
        let app = self.resolver.to_app_id(&project);
        let result = self
            .engine
            .commit_transaction(&project, t.handle, Vec::new())?;
        let response = v3::CommitResponse {
            cost: Some(cost(result.index_updates, result.entity_writes)),
            version: result
                .key_versions
                .iter()
                .map(|(key, version)| v3::commit_response::Version {
                    root_entity_key: entity::reference(key, &app),
                    version: *version,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        if !hostport.is_empty() && !result.actions.is_empty() {
            let tasks = result
                .actions
                .iter()
                .map(|bytes| {
                    let mut task = actions::Task::decode(bytes);
                    if let Some(project) = task.app_id() {
                        task.set_app_id(self.resolver.to_app_id(&text(&project)).as_bytes());
                    }
                    task.encode()
                })
                .collect();
            actions::deliver(hostport, tasks).await?;
        }
        Ok(response.encode_to_vec())
    }

    fn rollback(&self, t: v3::Transaction) -> Outcome {
        let project = self.project(&t.app)?;
        self.engine.rollback(&project, t.handle)?;
        Ok(Vec::new())
    }

    fn allocate_ids(&self, r: v3::AllocateIdsRequest) -> Outcome {
        if let Some(k) = &r.model_key {
            self.project(&k.app)?;
        }
        if r.size.is_some_and(|s| s > 1_000_000_000) {
            return Err(bad_request(
                "cannot get more than 1000000000 keys in a single call",
            ));
        }
        let (start, end) = self.engine.allocate_range(r.size, r.max.unwrap_or(0));
        Ok(v3::AllocateIdsResponse {
            start,
            end,
            cost: None,
        }
        .encode_to_vec())
    }

    fn get_indices(&self, r: v3::GetIndicesRequest) -> Outcome {
        let project = self.project(&r.app_id)?;
        let app = self.resolver.to_app_id(&project);
        let indexes: Vec<CompositeIndex> = self.engine.composite_indexes();
        Ok(v3::CompositeIndices {
            index: indexes
                .iter()
                .map(|index| os::CompositeIndex {
                    app_id: app.clone().into(),
                    id: 0,
                    definition: query::index_definition(index),
                    state: os::composite_index::State::ReadWrite as i32,
                    ..Default::default()
                })
                .collect(),
        }
        .encode_to_vec())
    }

    /// `Touch` (superset): checks the keys; there is nothing to refresh.
    fn touch(&self, r: v3::TouchRequest) -> Outcome {
        for k in &r.key {
            self.project(&k.app)?;
        }
        Ok(v3::TouchResponse {
            cost: Some(v3::Cost::default()),
        }
        .encode_to_vec())
    }

    /// `DeleteCursor` (superset): forgets a live query.
    fn delete_cursor(&self, c: v3::Cursor) -> Outcome {
        if let Some(app) = &c.app {
            let project = self.project(app)?;
            self.queries().live.remove(&(project, c.cursor));
        }
        Ok(Vec::new())
    }

    /// `CreateIndex`, `UpdateIndex` and `DeleteIndex` (superset): composite indexes kept with the
    /// Admin API's, which `--require_indexes` accepts. `CreateIndex` returns an
    /// `Integer64Proto` id; the others a `VoidProto`.
    fn index_call(&self, method: &str, request: &[u8]) -> Outcome {
        let index = os::CompositeIndex::decode(request)
            .map_err(|_| Fail::Rpc(PARSE_ERROR, wire::MALFORMED.into()))?;
        self.project(&index.app_id)?;
        let definition = query::index_from_definition(&index.definition)
            .ok_or_else(|| bad_request("The index definition is invalid."))?;
        match method {
            "CreateIndex" => {
                if !self.engine.add_admin_index(definition.clone()) {
                    return Err(bad_request("The index already exists."));
                }
                let id = self
                    .engine
                    .defined_indexes()
                    .iter()
                    .position(|i| *i == definition)
                    .map_or(0, |p| p as u64 + 1);
                let mut out = vec![0x08];
                prost::encoding::encode_varint(id, &mut out);
                Ok(out)
            }
            "DeleteIndex" => {
                self.engine.remove_admin_index(&definition);
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Full-text search (superset): rows whose entity has every word of `search` among the words
    /// of its string values, case-insensitively.
    fn search(
        &self,
        project: &str,
        search: &str,
        rows: Vec<oxide_engine::query::QueryResult>,
        mode: ReadMode,
    ) -> Result<Vec<oxide_engine::query::QueryResult>, Fail> {
        let wanted = query::words(search);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let entity = match &row.entity {
                Some((e, _)) => Some(e.clone()),
                None => match self
                    .engine
                    .lookup(project, std::slice::from_ref(&row.key), mode)?
                    .pop()
                {
                    Some(LookupResult::Found { entity, .. }) => Some(entity),
                    _ => None,
                },
            };
            if entity.is_some_and(|e| {
                let have = query::entity_words(&e);
                wanted.iter().all(|w| have.contains(w))
            }) {
                out.push(row);
            }
        }
        Ok(out)
    }

    fn add_actions(&self, request: &[u8]) -> Outcome {
        let mut bulk = actions::parse(request)?;
        if bulk.tasks.is_empty() {
            return Ok(Vec::new());
        }
        let (project, handle) = match bulk.transaction.take() {
            Some(actions::TaskTransaction::Bytes(bytes)) => {
                let t = v3::Transaction::decode(bytes.as_slice())
                    .map_err(|_| bad_request("Invalid transaction"))?;
                (text(&t.app), t.handle)
            }
            Some(actions::TaskTransaction::Message(bytes)) => {
                let t = v3::Transaction::decode(bytes.as_slice()).map_err(|_| Fail::Unknown)?;
                (self.project(&t.app)?, t.handle)
            }
            None => return Err(Fail::Unknown),
        };
        if !self.engine.has_project(&project) {
            return Err(Fail::Unknown);
        }
        let mut tasks = Vec::with_capacity(bulk.tasks.len());
        for mut task in bulk.tasks {
            if let Some(app) = task.app_id() {
                let project = self.project(&app)?;
                task.set_app_id(project.as_bytes());
            }
            tasks.push(task.encode());
        }
        self.engine.add_actions(&project, handle, tasks)?;
        Ok(Vec::new())
    }
}

fn cost(index_writes: i64, entity_writes: i64) -> v3::Cost {
    v3::Cost {
        index_writes: Some(i32::try_from(index_writes).unwrap_or(i32::MAX)),
        entity_writes: Some(i32::try_from(entity_writes).unwrap_or(i32::MAX)),
        ..Default::default()
    }
}

#[tonic::async_trait]
impl das::call_handler_server::CallHandler for Service {
    async fn handle_call(
        &self,
        request: Request<das::Request>,
    ) -> Result<Response<das::Response>, Status> {
        let r = request.into_inner();
        let response = match self
            .call(&r.method, &r.request, &r.txn_add_task_callback_hostport)
            .await
        {
            Ok(bytes) => das::Response {
                response: Bytes::from(bytes),
                ..Default::default()
            },
            Err(Fail::App(code, detail)) => das::Response {
                application_error: Some(das::ApplicationError { code, detail }),
                ..Default::default()
            },
            Err(Fail::Rpc(code, detail)) => das::Response {
                rpc_error: Some(das::RpcError { code, detail }),
                ..Default::default()
            },
            Err(Fail::Unknown) => return Err(Status::unknown("")),
        };
        Ok(Response::new(response))
    }
}
