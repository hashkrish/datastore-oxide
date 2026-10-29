//! Load generator: runs one workload over gRPC against a running emulator (ours or Java's) and
//! prints operations per second and latency percentiles.
//!
//!     cargo run --release -p bench -- <host:port> <workload> [concurrency] [seconds]
//!
//! Workloads (project `bench`, kind `K`, entities `e0..e999` with an indexed `n`):
//! - `commit`: non-transactional upserts of a random entity;
//! - `lookup`: lookups of a random entity (the entities are written first);
//! - `query`: `n = <random>` queries, about 10 results each;
//! - `txn`: begin, lookup, upsert, commit of a random entity (contention aborts count as errors).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use oxide_proto::google::datastore::v1 as pb;
use pb::datastore_client::DatastoreClient;
use tonic::transport::Channel;

const PROJECT: &str = "bench";
const ENTITIES: u64 = 1000;

fn key(i: u64) -> pb::Key {
    pb::Key {
        partition_id: None,
        path: vec![pb::key::PathElement {
            kind: "K".into(),
            id_type: Some(pb::key::path_element::IdType::Name(format!("e{i}"))),
        }],
    }
}

fn entity(i: u64, n: i64) -> pb::Entity {
    let value = |v| pb::Value {
        value_type: Some(v),
        ..Default::default()
    };
    pb::Entity {
        key: Some(key(i)),
        properties: [
            (
                "n".to_string(),
                value(pb::value::ValueType::IntegerValue(n)),
            ),
            (
                "s".to_string(),
                value(pb::value::ValueType::StringValue(format!("value {i}"))),
            ),
        ]
        .into_iter()
        .collect(),
    }
}

fn upsert(i: u64, n: i64) -> pb::Mutation {
    pb::Mutation {
        operation: Some(pb::mutation::Operation::Upsert(entity(i, n))),
        ..Default::default()
    }
}

/// A tiny xorshift generator (deterministic per worker).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

async fn one(client: &mut DatastoreClient<Channel>, workload: &str, rng: &mut Rng) -> Result<()> {
    let i = rng.below(ENTITIES);
    match workload {
        "commit" => {
            client
                .commit(pb::CommitRequest {
                    project_id: PROJECT.into(),
                    mode: pb::commit_request::Mode::NonTransactional as i32,
                    mutations: vec![upsert(i, (i % 100) as i64)],
                    ..Default::default()
                })
                .await?;
        }
        "lookup" => {
            client
                .lookup(pb::LookupRequest {
                    project_id: PROJECT.into(),
                    keys: vec![key(i)],
                    ..Default::default()
                })
                .await?;
        }
        "query" => {
            let filter = pb::Filter {
                filter_type: Some(pb::filter::FilterType::PropertyFilter(pb::PropertyFilter {
                    property: Some(pb::PropertyReference { name: "n".into() }),
                    op: pb::property_filter::Operator::Equal as i32,
                    value: Some(pb::Value {
                        value_type: Some(pb::value::ValueType::IntegerValue((i % 100) as i64)),
                        ..Default::default()
                    }),
                })),
            };
            client
                .run_query(pb::RunQueryRequest {
                    project_id: PROJECT.into(),
                    query_type: Some(pb::run_query_request::QueryType::Query(pb::Query {
                        kind: vec![pb::KindExpression { name: "K".into() }],
                        filter: Some(filter),
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .await?;
        }
        "txn" => {
            let txn = client
                .begin_transaction(pb::BeginTransactionRequest {
                    project_id: PROJECT.into(),
                    ..Default::default()
                })
                .await?
                .into_inner()
                .transaction;
            client
                .lookup(pb::LookupRequest {
                    project_id: PROJECT.into(),
                    keys: vec![key(i)],
                    read_options: Some(pb::ReadOptions {
                        consistency_type: Some(pb::read_options::ConsistencyType::Transaction(
                            txn.clone(),
                        )),
                    }),
                    ..Default::default()
                })
                .await?;
            client
                .commit(pb::CommitRequest {
                    project_id: PROJECT.into(),
                    mode: pb::commit_request::Mode::Transactional as i32,
                    transaction_selector: Some(
                        pb::commit_request::TransactionSelector::Transaction(txn),
                    ),
                    mutations: vec![upsert(i, (i % 100) as i64)],
                    ..Default::default()
                })
                .await?;
        }
        other => bail!("unknown workload {other}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [host, workload, rest @ ..] = args.as_slice() else {
        bail!("usage: bench <host:port> <commit|lookup|query|txn> [concurrency] [seconds]");
    };
    let concurrency: usize = rest.first().map_or(Ok(16), |c| c.parse())?;
    let seconds: u64 = rest.get(1).map_or(Ok(10), |s| s.parse())?;
    let channel = Channel::from_shared(format!("http://{host}"))?
        .connect()
        .await
        .context("connecting")?;
    let mut client = DatastoreClient::new(channel.clone());

    // Seed the entities (in batches of 500 mutations).
    let all: Vec<u64> = (0..ENTITIES).collect();
    for chunk in all.chunks(500) {
        client
            .commit(pb::CommitRequest {
                project_id: PROJECT.into(),
                mode: pb::commit_request::Mode::NonTransactional as i32,
                mutations: chunk.iter().map(|&i| upsert(i, (i % 100) as i64)).collect(),
                ..Default::default()
            })
            .await?;
    }

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut workers = Vec::new();
    for w in 0..concurrency {
        let mut client = DatastoreClient::new(channel.clone());
        let workload = workload.clone();
        workers.push(tokio::spawn(async move {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (w as u64 + 1));
            let (mut latencies, mut errors) = (Vec::new(), 0u64);
            while Instant::now() < deadline {
                let start = Instant::now();
                match one(&mut client, &workload, &mut rng).await {
                    Ok(()) => latencies.push(start.elapsed()),
                    Err(_) => errors += 1,
                }
            }
            (latencies, errors)
        }));
    }
    let (mut latencies, mut errors) = (Vec::new(), 0);
    for w in workers {
        let (l, e) = w.await?;
        latencies.extend(l);
        errors += e;
    }
    latencies.sort();
    let pct = |p: f64| {
        latencies
            .get(((latencies.len() as f64 * p) as usize).min(latencies.len().saturating_sub(1)))
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    println!(
        "{workload}: {:.0} ops/s ({} ok, {errors} errors, concurrency {concurrency}, {seconds}s), \
         latency p50 {:.2} ms, p99 {:.2} ms",
        latencies.len() as f64 / seconds as f64,
        latencies.len(),
        pct(0.5),
        pct(0.99),
    );
    Ok(())
}
