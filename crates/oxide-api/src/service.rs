//! `google.datastore.v1.Datastore` over the engine.

use std::collections::HashSet;
use std::sync::Arc;

use oxide_engine::{Code, Engine, LookupResult, Mutation, Op, ReadMode};
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

    fn begin(
        &self,
        scope: &Scope,
        options: Option<&pb::TransactionOptions>,
    ) -> Result<u64, Status> {
        use pb::transaction_options::Mode;
        let (read_only, previous) = match options.and_then(|o| o.mode.as_ref()) {
            Some(Mode::ReadOnly(_)) => (true, None),
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

/// Validates one mutation and turns it into an engine mutation.
fn mutation_from_proto(m: &pb::Mutation, scope: &Scope) -> Result<Mutation, Status> {
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
    Ok(Mutation { op, base_version })
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
        let keys = r
            .keys
            .iter()
            .map(|k| convert::key_from_proto(k, &scope, KeyUse::Read))
            .collect::<Result<Vec<_>, _>>()?;
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        let mut response = pb::LookupResponse {
            transaction: transaction.unwrap_or_default().into(),
            ..Default::default()
        };
        for result in self
            .engine
            .lookup(&scope.project_id, &keys, mode)
            .map_err(engine_status)?
        {
            match result {
                LookupResult::Found { entity, version } => response.found.push(pb::EntityResult {
                    entity: Some(convert::entity_to_proto(&entity)),
                    version,
                    ..Default::default()
                }),
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
        let namespace = crate::query::query_namespace(r.partition_id.as_ref(), &scope)?;
        let (query, echo) = match &r.query_type {
            Some(pb::run_query_request::QueryType::Query(q)) => {
                (crate::query::query_from_proto(q, &scope, namespace)?, None)
            }
            Some(pb::run_query_request::QueryType::GqlQuery(g)) => {
                let q = crate::gql::parse(g, &scope)?;
                (
                    crate::query::query_from_proto(&q, &scope, namespace)?,
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
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        let batch = self.engine.run_query(&query, mode).map_err(engine_status)?;
        Ok(Response::new(pb::RunQueryResponse {
            batch: Some(crate::query::batch_to_proto(&query, batch)),
            query: echo,
            transaction: transaction.unwrap_or_default().into(),
            ..Default::default()
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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
        let mut query = crate::query::query_from_proto(nested, &scope, namespace)?;
        self.check_transactional_query(r.read_options.as_ref(), &query)?;
        // Every row up to offset + limit, in one batch; the offset is skipped below.
        query.unbatched = true;
        if !aggregation::needs_entities(&aggregations) {
            query.keys_only = true;
        }
        let (mode, transaction) = self.read_mode(r.read_options.as_ref(), &scope)?;
        let batch = self.engine.run_query(&query, mode).map_err(engine_status)?;
        let result = aggregation::compute(&aggregations, batch.results.iter().skip(query.offset));
        Ok(Response::new(pb::RunAggregationQueryResponse {
            batch: Some(pb::AggregationResultBatch {
                aggregation_results: vec![result],
                more_results: pb::query_result_batch::MoreResultsType::NoMoreResults as i32,
                read_time: None,
            }),
            query: echo,
            transaction: transaction.unwrap_or_default().into(),
            ..Default::default()
        }))
    }

    async fn begin_transaction(
        &self,
        request: Request<pb::BeginTransactionRequest>,
    ) -> Result<Response<pb::BeginTransactionResponse>, Status> {
        let r = request.into_inner();
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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

        let mutations = r
            .mutations
            .iter()
            .map(|m| mutation_from_proto(m, &scope))
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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
        let scope = Scope::new(&r.project_id, &r.database_id)?;
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
