//! `google.datastore.v1.Datastore` over the engine.

use std::collections::HashSet;
use std::sync::Arc;

use oxide_engine::{Code, Engine, LookupResult, Mutation, OnConflict, Op, ReadMode};
use oxide_model::Entity;
use oxide_proto::google::datastore::v1 as pb;
use pb::datastore_server::Datastore;
use tonic::{Request, Response, Status};

use crate::convert::{self, KeyUse, Scope, invalid};

pub struct DatastoreService {
    engine: Arc<Engine>,
    /// Production features the Java emulator lacks (off with `--no_superset`,
    /// docs/superset-backlog.md).
    superset: bool,
}

impl DatastoreService {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            superset: false,
        }
    }

    pub fn with_superset(self, superset: bool) -> Self {
        Self { superset, ..self }
    }

    fn scope(&self, project_id: &str, database_id: &str) -> Result<Scope, Status> {
        let mut scope = Scope::new(project_id, database_id)?;
        scope.superset = self.superset;
        Ok(scope)
    }

    pub fn into_server(self) -> pb::datastore_server::DatastoreServer<Self> {
        pb::datastore_server::DatastoreServer::new(self)
            .max_decoding_message_size(usize::MAX)
            .max_encoding_message_size(usize::MAX)
    }

    /// Resolves read options into a read mode, beginning a transaction for `new_transaction`.
    /// Returns the new transaction's handle bytes when one was started.
    fn read_mode(
        &self,
        options: Option<&pb::ReadOptions>,
        scope: &Scope,
    ) -> Result<(ReadMode, Option<Vec<u8>>), Status> {
        use pb::read_options::{ConsistencyType, ReadConsistency};
        Ok(match options.and_then(|o| o.consistency_type.as_ref()) {
            Some(ConsistencyType::ReadConsistency(c)) if *c == ReadConsistency::Eventual as i32 => {
                (ReadMode::Eventual, None)
            }
            Some(ConsistencyType::Transaction(t)) => {
                (ReadMode::Transaction(decode_handle(t)?), None)
            }
            Some(ConsistencyType::NewTransaction(options)) => {
                let handle = self.begin(scope, Some(options))?;
                (ReadMode::Transaction(handle), Some(encode_handle(handle)))
            }
            // Java accepts a read time and ignores it.
            Some(ConsistencyType::ReadTime(t)) if self.superset => {
                (ReadMode::At(convert::timestamp_to_micros(t)?), None)
            }
            _ => (ReadMode::Strong, None),
        })
    }

    /// Datastore mode only allows ancestor queries inside transactions.
    fn check_transactional_query(
        &self,
        options: Option<&pb::ReadOptions>,
        query: &oxide_engine::query::Query,
    ) -> Result<(), Status> {
        let transactional = matches!(
            options.and_then(|o| o.consistency_type.as_ref()),
            Some(
                pb::read_options::ConsistencyType::Transaction(_)
                    | pb::read_options::ConsistencyType::NewTransaction(_)
            )
        );
        if transactional && query.ancestor.is_none() && !self.engine.firestore_mode() {
            return Err(invalid(
                "Only ancestor queries are allowed inside transactions.",
            ));
        }
        Ok(())
    }

    /// A vector search: the query's rows, ranked by distance (see [`crate::vector`]).
    fn find_nearest(
        &self,
        r: &pb::RunQueryRequest,
        scope: &Scope,
        query: oxide_engine::query::Query,
        nearest: crate::vector::Nearest,
    ) -> Result<Response<pb::RunQueryResponse>, Status> {
        if !query.projection.is_empty() || !query.group_by.is_empty() {
            return Err(invalid(
                "find_nearest supports no projection other than __key__ and no DISTINCT ON.",
            ));
        }
        let mut full = query.clone();
        full.keys_only = false;
        full.unbatched = true;
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), scope)?;
        let batch = self.engine.run_query(&full, mode).map_err(engine_status)?;
        let rows = batch.results.into_iter().skip(full.offset).collect();
        let selected = nearest.select(rows);
        let distances: Vec<f64> = selected.iter().map(|(_, d)| *d).collect();
        let results: Vec<_> = selected.into_iter().map(|(row, _)| row).collect();
        let end_cursor = results.last().map(|r| r.cursor.clone()).unwrap_or_default();
        let mut out = crate::query::batch_to_proto(
            &query,
            oxide_engine::query::QueryBatch {
                results,
                skipped_results: 0,
                skipped_cursor: None,
                end_cursor,
                more_results: false,
                snapshot_version: batch.snapshot_version,
                matched: Vec::new(),
            },
        );
        out.more_results = pb::query_result_batch::MoreResultsType::NoMoreResults as i32;
        out.read_time = read_time_of(mode);
        if let Some(property) = nearest
            .result_property
            .as_ref()
            .filter(|_| !query.keys_only)
        {
            for (result, d) in out.entity_results.iter_mut().zip(distances) {
                if let Some(e) = &mut result.entity {
                    e.properties.insert(
                        property.clone(),
                        pb::Value {
                            value_type: Some(pb::value::ValueType::DoubleValue(d)),
                            ..Default::default()
                        },
                    );
                }
            }
        }
        Ok(Response::new(pb::RunQueryResponse {
            batch: Some(out),
            transaction: transaction.unwrap_or_default().into(),
            ..Default::default()
        }))
    }

    fn begin(
        &self,
        scope: &Scope,
        options: Option<&pb::TransactionOptions>,
    ) -> Result<u64, Status> {
        use pb::transaction_options::Mode;
        let (read_only, previous) = match options.and_then(|o| o.mode.as_ref()) {
            Some(Mode::ReadOnly(ro)) => {
                if let Some(t) = ro.read_time.as_ref().filter(|_| self.superset) {
                    return self
                        .engine
                        .begin_read_only_at(&scope.project_id, convert::timestamp_to_micros(t)?)
                        .map_err(engine_status);
                }
                (true, None)
            }
            Some(Mode::ReadWrite(rw)) if !rw.previous_transaction.is_empty() => {
                // Java reports a malformed previous transaction as an internal error.
                let previous = decode_handle(&rw.previous_transaction)
                    .map_err(|e| Status::internal(e.message().to_string()))?;
                (false, Some(previous))
            }
            _ => (false, None),
        };
        self.engine
            .begin_transaction(&scope.project_id, read_only, true, previous)
            .map_err(engine_status)
    }
}

/// The read time a response reports: the requested one, for point-in-time reads.
fn read_time_of(mode: ReadMode) -> Option<prost_types::Timestamp> {
    match mode {
        ReadMode::At(t) => Some(convert::micros_to_timestamp(t)),
        _ => None,
    }
}

pub(crate) fn engine_status(e: oxide_engine::Error) -> Status {
    match e.code {
        Code::InvalidArgument => Status::invalid_argument(e.message),
        Code::NotFound => Status::not_found(e.message),
        Code::AlreadyExists => Status::already_exists(e.message),
        Code::Aborted => Status::aborted(e.message),
        Code::FailedPrecondition => Status::failed_precondition(e.message),
        Code::Unknown => Status::unknown(e.message),
    }
}

/// Transaction handles are serialized App Engine v3 `Transaction` protos: `fixed64 handle = 2`.
pub fn encode_handle(handle: u64) -> Vec<u8> {
    let mut out = vec![0x11];
    out.extend_from_slice(&handle.to_le_bytes());
    out
}

fn decode_handle(bytes: &[u8]) -> Result<u64, Status> {
    use prost::encoding::{WireType, decode_key, decode_varint};
    if bytes.is_empty() {
        return Err(invalid("a transaction cannot be the empty string."));
    }
    let bad = || invalid("Invalid transaction.");
    let mut buf = bytes;
    let mut handle = 0;
    while !buf.is_empty() {
        let (tag, wire) = decode_key(&mut buf).map_err(|_| bad())?;
        match wire {
            WireType::SixtyFourBit if buf.len() >= 8 => {
                let (value, rest) = buf.split_at(8);
                if tag == 2 {
                    handle = u64::from_le_bytes(value.try_into().expect("8 bytes"));
                }
                buf = rest;
            }
            WireType::Varint => {
                decode_varint(&mut buf).map_err(|_| bad())?;
            }
            WireType::LengthDelimited => {
                let len = decode_varint(&mut buf).map_err(|_| bad())? as usize;
                if len > buf.len() {
                    return Err(bad());
                }
                buf = &buf[len..];
            }
            WireType::ThirtyTwoBit if buf.len() >= 4 => buf = &buf[4..],
            _ => return Err(bad()),
        }
    }
    Ok(handle)
}

/// Validates one mutation and turns it into an engine mutation. With `superset`, its property mask
/// and property transforms apply (Java ignores both).
fn mutation_from_proto(
    m: &pb::Mutation,
    scope: &Scope,
    superset: bool,
) -> Result<Mutation, Status> {
    use pb::mutation::{ConflictDetectionStrategy, Operation};
    let entity = |e: &pb::Entity, allow_incomplete: bool| -> Result<Entity, Status> {
        if e.key.is_none() {
            return Err(invalid("Entity is missing key."));
        }
        convert::entity_from_proto(e, scope, KeyUse::Write { allow_incomplete })
    };
    let op = match &m.operation {
        None => return Err(invalid("Mutation is missing operation.")),
        Some(Operation::Insert(e)) => Op::Insert(entity(e, true)?),
        Some(Operation::Update(e)) => Op::Update(entity(e, false)?),
        Some(Operation::Upsert(e)) => Op::Upsert(entity(e, true)?),
        Some(Operation::Delete(k)) => Op::Delete(convert::key_from_proto(
            k,
            scope,
            KeyUse::Write {
                allow_incomplete: false,
            },
        )?),
    };
    let base_version = match &m.conflict_detection_strategy {
        None => None,
        Some(ConflictDetectionStrategy::BaseVersion(v)) if *v < 0 => {
            return Err(invalid(format!(
                "Invalid base_version: {v}, it should be >= 0"
            )));
        }
        Some(ConflictDetectionStrategy::BaseVersion(v)) => Some(*v),
        // An update time is compared as a version: versions are commit timestamps in micros.
        Some(ConflictDetectionStrategy::UpdateTime(ts)) => {
            Some(ts.seconds * 1_000_000 + i64::from(ts.nanos / 1000))
        }
    };
    if matches!(op, Op::Insert(_)) && base_version.is_some_and(|v| v > 0) {
        return Err(invalid(
            "Cannot insert an entity with a base version greater than zero",
        ));
    }
    if m.conflict_resolution_strategy != 0 && base_version.is_none() {
        return Err(invalid(
            "cannot set a conflict resolution strategy if no detection strategy was set",
        ));
    }
    let partial = if superset {
        partial_write_from_proto(m, &op, scope)?
    } else {
        Default::default()
    };
    Ok(Mutation {
        op,
        base_version,
        partial,
        on_conflict: match pb::mutation::ConflictResolutionStrategy::try_from(
            m.conflict_resolution_strategy,
        ) {
            Ok(pb::mutation::ConflictResolutionStrategy::Fail) => OnConflict::Fail,
            Ok(_) => OnConflict::ServerValue,
            Err(_) => OnConflict::Unrecognized,
        },
    })
}

/// The paths of a read's property mask (superset; Java returns every property).
fn read_mask(mask: Option<&pb::PropertyMask>) -> Result<Option<Vec<Vec<String>>>, Status> {
    mask.map(|m| {
        m.paths
            .iter()
            .map(|p| {
                oxide_engine::transform::parse_path(p)
                    .ok_or_else(|| invalid(format!("Invalid property mask path \"{p}\".")))
            })
            .collect()
    })
    .transpose()
}

/// Keeps only the properties `paths` reference (a path into an embedded entity keeps that
/// entity with only the referenced properties).
fn apply_read_mask(entity: &mut pb::Entity, paths: &[Vec<String>]) {
    use pb::value::ValueType;
    let mut nested: std::collections::HashMap<&str, Vec<Vec<String>>> = Default::default();
    let mut whole: HashSet<&str> = HashSet::new();
    for p in paths {
        match p.split_first() {
            Some((first, [])) => {
                whole.insert(first);
            }
            Some((first, rest)) => nested.entry(first).or_default().push(rest.to_vec()),
            None => {}
        }
    }
    entity.properties.retain(|name, value| {
        if whole.contains(name.as_str()) {
            return true;
        }
        match (nested.get(name.as_str()), &mut value.value_type) {
            (Some(sub), Some(ValueType::EntityValue(e))) => {
                apply_read_mask(e, sub);
                true
            }
            _ => false,
        }
    });
}

/// A mutation's property mask and property transforms (superset).
fn partial_write_from_proto(
    m: &pb::Mutation,
    op: &Op,
    scope: &Scope,
) -> Result<oxide_engine::transform::PartialWrite, Status> {
    use oxide_engine::transform::{Number, PartialWrite, Transform, TransformOp, parse_path};
    use pb::property_transform::{ServerValue, TransformType};
    let is_delete = matches!(op, Op::Delete(_));
    let mask = match &m.property_mask {
        Some(mask) if !is_delete => {
            let mut paths = Vec::with_capacity(mask.paths.len());
            for p in &mask.paths {
                let path = parse_path(p)
                    .ok_or_else(|| invalid(format!("Invalid property mask path \"{p}\".")))?;
                if p == "__key__" {
                    continue;
                }
                if path
                    .iter()
                    .any(|n| n.starts_with("__") && n.ends_with("__"))
                {
                    return Err(invalid(format!(
                        "The property mask path \"{p}\" has a reserved name."
                    )));
                }
                paths.push(path);
            }
            Some(paths)
        }
        _ => None,
    };
    if is_delete && !m.property_transforms.is_empty() {
        return Err(invalid(
            "Property transforms can only be used with insert, update or upsert.",
        ));
    }
    let number = |what: &str, v: &pb::Value| match v.value_type {
        Some(pb::value::ValueType::IntegerValue(i)) => Ok(Number::Integer(i)),
        Some(pb::value::ValueType::DoubleValue(d)) => Ok(Number::Double(d)),
        _ => Err(invalid(format!(
            "The {what} value must be an integer or a double."
        ))),
    };
    let values = |property: &str, a: &pb::ArrayValue| {
        a.values
            .iter()
            .map(|v| convert::single_value_from_proto(property, v, scope))
            .collect::<Result<Vec<_>, _>>()
    };
    let mut transforms = Vec::with_capacity(m.property_transforms.len());
    for t in &m.property_transforms {
        let property = parse_path(&t.property).ok_or_else(|| {
            invalid(format!(
                "Invalid property path \"{}\" in a property transform.",
                t.property
            ))
        })?;
        let op = match &t.transform_type {
            None => return Err(invalid("A property transform must set a transformation.")),
            Some(TransformType::SetToServerValue(v)) => {
                if *v != ServerValue::RequestTime as i32 {
                    return Err(invalid("set_to_server_value must be REQUEST_TIME."));
                }
                TransformOp::RequestTime
            }
            Some(TransformType::Increment(v)) => TransformOp::Increment(number("increment", v)?),
            Some(TransformType::Maximum(v)) => TransformOp::Maximum(number("maximum", v)?),
            Some(TransformType::Minimum(v)) => TransformOp::Minimum(number("minimum", v)?),
            Some(TransformType::AppendMissingElements(a)) => {
                TransformOp::AppendMissing(values(&t.property, a)?)
            }
            Some(TransformType::RemoveAllFromArray(a)) => {
                TransformOp::RemoveAll(values(&t.property, a)?)
            }
        };
        transforms.push(Transform { property, op });
    }
    Ok(PartialWrite { mask, transforms })
}

/// Production's limits on a commit, which the Java emulator does not enforce.
fn check_production_limits(mutations: &[pb::Mutation]) -> Result<(), Status> {
    use pb::mutation::Operation;
    use prost::Message as _;
    /// Mutations per commit.
    const MAX_MUTATIONS: usize = 500;
    /// Serialized entity size: 1 MiB minus 4 bytes.
    const MAX_ENTITY_BYTES: usize = 1_048_572;
    if mutations.len() > MAX_MUTATIONS {
        return Err(invalid(format!(
            "cannot write more than {MAX_MUTATIONS} entities in a single call"
        )));
    }
    for m in mutations {
        if let Some(Operation::Insert(e) | Operation::Update(e) | Operation::Upsert(e)) =
            &m.operation
            && e.encoded_len() > MAX_ENTITY_BYTES
        {
            return Err(invalid("entity is too big"));
        }
    }
    Ok(())
}

fn mutation_key(m: &Mutation) -> Option<&oxide_model::Key> {
    match &m.op {
        Op::Insert(e) | Op::Update(e) | Op::Upsert(e) => e.key.as_ref(),
        Op::Delete(k) => Some(k),
    }
}

#[tonic::async_trait]
impl Datastore for DatastoreService {
    async fn lookup(
        &self,
        request: Request<pb::LookupRequest>,
    ) -> Result<Response<pb::LookupResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let keys = r
            .keys
            .iter()
            .map(|k| convert::key_from_proto(k, &scope, KeyUse::Read))
            .collect::<Result<Vec<_>, _>>()?;
        let mask = if self.superset {
            read_mask(r.property_mask.as_ref())?
        } else {
            None
        };
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        let read_time = read_time_of(mode);
        let mut response = pb::LookupResponse {
            transaction: transaction.unwrap_or_default().into(),
            read_time,
            ..Default::default()
        };
        for result in self
            .engine
            .lookup(&scope.project_id, &keys, mode)
            .map_err(engine_status)?
        {
            match result {
                LookupResult::Found { entity, version } => {
                    let mut entity = convert::entity_to_proto(&entity);
                    if let Some(mask) = &mask {
                        apply_read_mask(&mut entity, mask);
                    }
                    response.found.push(pb::EntityResult {
                        entity: Some(entity),
                        version,
                        ..Default::default()
                    })
                }
                LookupResult::Missing { key, version } => response.missing.push(pb::EntityResult {
                    entity: Some(pb::Entity {
                        key: Some(convert::key_to_proto(&key)),
                        properties: Default::default(),
                    }),
                    version,
                    ..Default::default()
                }),
            }
        }
        Ok(Response::new(response))
    }

    async fn run_query(
        &self,
        request: Request<pb::RunQueryRequest>,
    ) -> Result<Response<pb::RunQueryResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let namespace = crate::query::query_namespace(r.partition_id.as_ref(), &scope)?;
        let (query, echo) = match &r.query_type {
            Some(pb::run_query_request::QueryType::Query(q)) => (
                crate::query::query_from_proto(q, &scope, namespace, self.superset)?,
                None,
            ),
            Some(pb::run_query_request::QueryType::GqlQuery(g)) => {
                let q = crate::gql::parse(g, &scope)?;
                (
                    crate::query::query_from_proto(&q, &scope, namespace, self.superset)?,
                    Some(q),
                )
            }
            None => {
                return Err(invalid(
                    "one of fields Query.query and Query.gql_query must be set",
                ));
            }
        };
        self.check_transactional_query(r.read_options.as_ref(), &query)?;
        let nearest = match &r.query_type {
            Some(pb::run_query_request::QueryType::Query(q)) if self.superset => q
                .find_nearest
                .as_ref()
                .map(crate::vector::Nearest::from_proto)
                .transpose()?,
            _ => None,
        };
        if let Some(nearest) = nearest {
            return self.find_nearest(&r, &scope, query, nearest);
        }
        let mask = if self.superset {
            read_mask(r.property_mask.as_ref())?
        } else {
            None
        };
        if mask.is_some() && (query.keys_only || !query.projection.is_empty()) {
            return Err(invalid(
                "The property mask must not be set for a projection query.",
            ));
        }
        let explain = r.explain_options.filter(|_| self.superset);
        let plan_summary = explain
            .map(|_| crate::explain::plan_summary(&query))
            .transpose()?;
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        if explain.is_some_and(|e| !e.analyze) {
            // Explain only: the plan, without running the query.
            return Ok(Response::new(pb::RunQueryResponse {
                query: echo,
                transaction: transaction.unwrap_or_default().into(),
                explain_metrics: Some(pb::ExplainMetrics {
                    plan_summary,
                    execution_stats: None,
                }),
                ..Default::default()
            }));
        }
        let read_time = read_time_of(mode);
        let started = std::time::Instant::now();
        let batch = self.engine.run_query(&query, mode).map_err(engine_status)?;
        let scanned = batch.matched.len();
        let mut batch = crate::query::batch_to_proto(&query, batch);
        batch.read_time = read_time;
        if let Some(mask) = &mask {
            for result in &mut batch.entity_results {
                if let Some(e) = &mut result.entity {
                    apply_read_mask(e, mask);
                }
            }
        }
        let explain_metrics = plan_summary.map(|plan_summary| pb::ExplainMetrics {
            plan_summary: Some(plan_summary),
            execution_stats: Some(crate::explain::execution_stats(
                batch.entity_results.len(),
                scanned,
                // Like production, a query costs at least one read.
                batch.entity_results.len().max(1),
                started.elapsed(),
            )),
        });
        Ok(Response::new(pb::RunQueryResponse {
            batch: Some(batch),
            query: echo,
            transaction: transaction.unwrap_or_default().into(),
            explain_metrics,
        }))
    }

    async fn run_aggregation_query(
        &self,
        request: Request<pb::RunAggregationQueryRequest>,
    ) -> Result<Response<pb::RunAggregationQueryResponse>, Status> {
        use crate::aggregation;
        if !self.superset {
            // Like Java's emulator; see docs/superset-backlog.md.
            return Err(Status::unimplemented(
                "Method google.datastore.v1.Datastore/RunAggregationQuery is unimplemented",
            ));
        }
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let namespace = crate::query::query_namespace(r.partition_id.as_ref(), &scope)?;
        let (aggregation_query, echo) = match r.query_type {
            Some(pb::run_aggregation_query_request::QueryType::AggregationQuery(q)) => (q, None),
            Some(pb::run_aggregation_query_request::QueryType::GqlQuery(g)) => {
                let q = crate::gql::parse_aggregation(&g, &scope)?;
                (q.clone(), Some(q))
            }
            None => {
                return Err(invalid(
                    "one of fields Query.aggregation_query and Query.gql_query must be set",
                ));
            }
        };
        let (nested, aggregations) = aggregation::from_proto(&aggregation_query)?;
        let mut query = crate::query::query_from_proto(nested, &scope, namespace, self.superset)?;
        self.check_transactional_query(r.read_options.as_ref(), &query)?;
        // Every row up to offset + limit, in one batch; the offset is skipped below.
        query.unbatched = true;
        if !aggregation::needs_entities(&aggregations) {
            query.keys_only = true;
        }
        let explain = r.explain_options;
        let plan_summary = explain
            .map(|_| crate::explain::plan_summary(&query))
            .transpose()?;
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        if explain.is_some_and(|e| !e.analyze) {
            return Ok(Response::new(pb::RunAggregationQueryResponse {
                query: echo,
                transaction: transaction.unwrap_or_default().into(),
                explain_metrics: Some(pb::ExplainMetrics {
                    plan_summary,
                    execution_stats: None,
                }),
                ..Default::default()
            }));
        }
        let read_time = read_time_of(mode);
        let started = std::time::Instant::now();
        let batch = self.engine.run_query(&query, mode).map_err(engine_status)?;
        let rows = batch.results.len().saturating_sub(query.offset);
        let result = aggregation::compute(&aggregations, batch.results.iter().skip(query.offset));
        let explain_metrics = plan_summary.map(|plan_summary| pb::ExplainMetrics {
            plan_summary: Some(plan_summary),
            // Aggregations bill one read per 1000 index entries, at least one.
            execution_stats: Some(crate::explain::execution_stats(
                1,
                rows,
                rows.div_ceil(1000).max(1),
                started.elapsed(),
            )),
        });
        Ok(Response::new(pb::RunAggregationQueryResponse {
            batch: Some(pb::AggregationResultBatch {
                aggregation_results: vec![result],
                more_results: pb::query_result_batch::MoreResultsType::NoMoreResults as i32,
                read_time,
            }),
            query: echo,
            transaction: transaction.unwrap_or_default().into(),
            explain_metrics,
        }))
    }

    async fn begin_transaction(
        &self,
        request: Request<pb::BeginTransactionRequest>,
    ) -> Result<Response<pb::BeginTransactionResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let handle = self.begin(&scope, r.transaction_options.as_ref())?;
        Ok(Response::new(pb::BeginTransactionResponse {
            transaction: encode_handle(handle).into(),
        }))
    }

    async fn commit(
        &self,
        request: Request<pb::CommitRequest>,
    ) -> Result<Response<pb::CommitResponse>, Status> {
        use pb::commit_request::{Mode, TransactionSelector};
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let handle = match (&r.transaction_selector, r.mode()) {
            (Some(_), Mode::NonTransactional) => {
                return Err(invalid(
                    "non-transaction commit cannot specify a transaction",
                ));
            }
            (None, Mode::NonTransactional) => None,
            (None, _) => return Err(invalid("transactional commit requires a transaction")),
            (Some(TransactionSelector::Transaction(t)), _) => Some(decode_handle(t)?),
            (Some(TransactionSelector::SingleUseTransaction(options)), _) => {
                if matches!(
                    options.mode,
                    Some(pb::transaction_options::Mode::ReadOnly(_))
                ) {
                    return Err(invalid(
                        "single_use_transaction in a commit request must be READ_WRITE.",
                    ));
                }
                Some(self.begin(&scope, Some(options))?)
            }
        };

        if self.superset {
            check_production_limits(&r.mutations)?;
        }
        let mutations = r
            .mutations
            .iter()
            .map(|m| mutation_from_proto(m, &scope, self.superset))
            .collect::<Result<Vec<_>, _>>()?;
        let result = match handle {
            Some(handle) => self.engine.commit_transaction(&scope.project_id, handle, mutations),
            None => {
                let mut seen = HashSet::new();
                for key in mutations.iter().filter_map(mutation_key).filter(|k| k.is_complete()) {
                    if !seen.insert(key) {
                        return Err(invalid(
                            "A non-transactional commit may not contain multiple mutations affecting the same entity.",
                        ));
                    }
                }
                self.engine.commit_non_transactional(&scope.project_id, mutations)
            }
        }
        .map_err(engine_status)?;
        Ok(Response::new(pb::CommitResponse {
            mutation_results: result
                .mutation_results
                .into_iter()
                .map(|r| pb::MutationResult {
                    key: r.key.as_ref().map(convert::key_to_proto),
                    version: r.version,
                    conflict_detected: r.conflict_detected,
                    transform_results: r
                        .transform_results
                        .iter()
                        .map(convert::value_to_proto)
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            index_updates: i32::try_from(result.index_updates).unwrap_or(i32::MAX),
            commit_time: None,
            commit_version: result.commit_version,
        }))
    }

    async fn rollback(
        &self,
        request: Request<pb::RollbackRequest>,
    ) -> Result<Response<pb::RollbackResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        if r.transaction.is_empty() {
            return Err(invalid("transaction required"));
        }
        let handle = decode_handle(&r.transaction)?;
        self.engine
            .rollback(&scope.project_id, handle)
            .map_err(engine_status)?;
        Ok(Response::new(pb::RollbackResponse {}))
    }

    async fn allocate_ids(
        &self,
        request: Request<pb::AllocateIdsRequest>,
    ) -> Result<Response<pb::AllocateIdsResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let keys = r
            .keys
            .iter()
            .map(|k| convert::key_from_proto(k, &scope, KeyUse::Allocate))
            .collect::<Result<Vec<_>, _>>()?;
        let keys = self.engine.allocate_ids(keys);
        Ok(Response::new(pb::AllocateIdsResponse {
            keys: keys.iter().map(convert::key_to_proto).collect(),
        }))
    }

    async fn reserve_ids(
        &self,
        request: Request<pb::ReserveIdsRequest>,
    ) -> Result<Response<pb::ReserveIdsResponse>, Status> {
        let r = request.into_inner();
        let scope = self.scope(&r.project_id, &r.database_id)?;
        let keys = r
            .keys
            .iter()
            .map(|k| {
                convert::key_from_proto(
                    k,
                    &scope,
                    KeyUse::Write {
                        allow_incomplete: false,
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.engine.reserve_ids(&keys);
        Ok(Response::new(pb::ReserveIdsResponse {}))
    }
}
