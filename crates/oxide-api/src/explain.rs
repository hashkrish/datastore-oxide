//! Query explain (superset only; Java's REST schema rejects `explain_options` and its gRPC
//! service ignores them): the plan summary of a query, and execution statistics when analyzed.

use std::time::Duration;

use oxide_engine::query::{Direction, Query, indexes_used};
use oxide_proto::google::datastore::v1 as pb;
use prost_types::{Struct, Value, value::Kind};
use tonic::Status;

use crate::service::engine_status;

fn string(s: impl Into<String>) -> Value {
    Value {
        kind: Some(Kind::StringValue(s.into())),
    }
}

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Struct {
    Struct {
        fields: fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }
}

/// The plan summary: one index per disjunction, like
/// `{"query_scope": "Kind", "properties": "(n ASC, __key__ ASC)"}`.
pub fn plan_summary(q: &Query) -> Result<pb::PlanSummary, Status> {
    let indexes = indexes_used(q).map_err(engine_status)?;
    let scope = if q.kind.is_some() { "Kind" } else { "Kindless" };
    Ok(pb::PlanSummary {
        indexes_used: indexes
            .into_iter()
            .map(|index| {
                let properties: Vec<String> = index
                    .iter()
                    .map(|(p, d)| {
                        let d = match d {
                            Direction::Ascending => "ASC",
                            Direction::Descending => "DESC",
                        };
                        format!("{p} {d}")
                    })
                    .collect();
                object([
                    ("query_scope", string(scope)),
                    ("properties", string(format!("({})", properties.join(", ")))),
                ])
            })
            .collect(),
    })
}

/// Execution statistics: `results` returned after scanning `scanned` matching entities, for
/// `read_operations` billed reads.
pub fn execution_stats(
    results: usize,
    scanned: usize,
    read_operations: usize,
    elapsed: Duration,
) -> pb::ExecutionStats {
    let count = |n: usize| string(n.to_string());
    pb::ExecutionStats {
        results_returned: results as i64,
        execution_duration: Some(prost_types::Duration {
            seconds: elapsed.as_secs() as i64,
            nanos: elapsed.subsec_nanos() as i32,
        }),
        read_operations: read_operations as i64,
        debug_stats: Some(object([
            ("documents_scanned", count(scanned)),
            ("index_entries_scanned", count(scanned)),
        ])),
    }
}
