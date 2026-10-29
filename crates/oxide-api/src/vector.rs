//! Vector search (`Query.find_nearest`, superset only: Java's REST schema rejects it, its gRPC
//! service ignores it, and it rejects vector values).
//!
//! Runs after the rest of the query (filters, cursors, offset, limit): entities whose property
//! holds a vector of the query vector's dimension are ranked by distance, those beyond the
//! threshold are dropped, and the nearest `limit` are returned, optionally with their distance
//! in a result property.

use oxide_engine::query::QueryResult;
use oxide_proto::google::datastore::v1 as pb;
use pb::find_nearest::DistanceMeasure;
use pb::value::ValueType;
use tonic::Status;

use crate::convert::{MAX_VECTOR_DIMENSIONS, invalid, vector_components};

/// Production's limit on `find_nearest.limit`.
const MAX_LIMIT: i32 = 100;

pub struct Nearest {
    property: String,
    query: Vec<f64>,
    measure: DistanceMeasure,
    limit: usize,
    pub result_property: Option<String>,
    threshold: Option<f64>,
}

impl Nearest {
    pub fn from_proto(f: &pb::FindNearest) -> Result<Self, Status> {
        let property = f
            .vector_property
            .as_ref()
            .map(|p| p.name.clone())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| invalid("find_nearest requires a vector_property."))?;
        let query: Vec<f64> = match f.query_vector.as_ref().and_then(|v| v.value_type.as_ref()) {
            Some(ValueType::ArrayValue(a)) => a
                .values
                .iter()
                .map(|v| match v.value_type {
                    Some(ValueType::DoubleValue(d)) => Some(d),
                    _ => None,
                })
                .collect::<Option<_>>()
                .ok_or_else(|| invalid("find_nearest.query_vector must contain only doubles."))?,
            _ => return Err(invalid("find_nearest requires a query_vector.")),
        };
        if query.is_empty() || query.len() > MAX_VECTOR_DIMENSIONS {
            return Err(invalid(format!(
                "find_nearest.query_vector must have between 1 and {MAX_VECTOR_DIMENSIONS} dimensions."
            )));
        }
        let measure = match DistanceMeasure::try_from(f.distance_measure) {
            Ok(DistanceMeasure::Unspecified) | Err(_) => {
                return Err(invalid("find_nearest requires a distance_measure."));
            }
            Ok(m) => m,
        };
        let limit = match f.limit {
            Some(l) if (1..=MAX_LIMIT).contains(&l) => l as usize,
            _ => {
                return Err(invalid(format!(
                    "find_nearest.limit must be between 1 and {MAX_LIMIT}."
                )));
            }
        };
        let result_property = Some(f.distance_result_property.clone()).filter(|p| !p.is_empty());
        if result_property
            .as_ref()
            .is_some_and(|p| p.starts_with("__") && p.ends_with("__"))
        {
            return Err(invalid(
                "find_nearest.distance_result_property must not be a reserved name.",
            ));
        }
        Ok(Self {
            property,
            query,
            measure,
            limit,
            result_property,
            threshold: f.distance_threshold,
        })
    }

    fn distance(&self, v: &[f64]) -> Option<f64> {
        let dot: f64 = self.query.iter().zip(v).map(|(a, b)| a * b).sum();
        Some(match self.measure {
            DistanceMeasure::Euclidean => self
                .query
                .iter()
                .zip(v)
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f64>()
                .sqrt(),
            DistanceMeasure::Cosine => {
                let norm = |x: &[f64]| x.iter().map(|c| c * c).sum::<f64>().sqrt();
                let norms = norm(&self.query) * norm(v);
                if norms == 0.0 {
                    return None;
                }
                1.0 - dot / norms
            }
            DistanceMeasure::DotProduct => dot,
            DistanceMeasure::Unspecified => return None,
        })
    }

    /// The nearest rows, nearest first, with their distances.
    pub fn select(&self, rows: Vec<QueryResult>) -> Vec<(QueryResult, f64)> {
        let larger_is_nearer = self.measure == DistanceMeasure::DotProduct;
        let mut ranked: Vec<(QueryResult, f64)> = rows
            .into_iter()
            .filter_map(|row| {
                let (entity, _) = row.entity.as_ref()?;
                let vector = vector_components(entity.properties.get(&self.property)?)?;
                if vector.len() != self.query.len() {
                    return None;
                }
                let d = self.distance(&vector)?;
                let within = self
                    .threshold
                    .is_none_or(|t| if larger_is_nearer { d >= t } else { d <= t });
                within.then_some((row, d))
            })
            .collect();
        ranked.sort_by(|a, b| {
            let ord = a.1.total_cmp(&b.1);
            if larger_is_nearer { ord.reverse() } else { ord }
        });
        ranked.truncate(self.limit);
        ranked
    }
}
