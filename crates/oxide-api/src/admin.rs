//! The Datastore Admin v1 service: synchronous export and import in the managed-export format;
//! the index methods are unimplemented, as in Java.
//!
//! With the superset (docs/superset-backlog.md), operations get names and are kept for the
//! Operations service, exports report the overall metadata file as `output_url`, `labels` are
//! kept, `gs://bucket/path` URLs map to a local directory, and the index methods manage composite
//! indexes (which `--require_indexes` then accepts).

use std::path::PathBuf;
use std::sync::Arc;

use oxide_engine::Engine;
use oxide_proto::google::datastore::admin::v1 as admin;
use oxide_proto::google::longrunning::{Operation, operation};
use prost::Message;
use tonic::{Request, Response, Status};

use crate::convert::invalid;
use crate::export::{self, EntitySetSpec, ImportError};
use crate::operations::OperationStore;
use crate::v3;
use oxide_engine::composite::{CompositeIndex, IndexProperty};
use oxide_engine::query::Direction;

/// Entities written per import batch (each batch is validated before it is written).
const IMPORT_BATCH: usize = 5000;

pub struct AdminService {
    engine: Arc<Engine>,
    /// Set with the superset: where finished operations are kept.
    operations: Option<OperationStore>,
}

impl AdminService {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            operations: None,
        }
    }

    /// Enables the superset, keeping operations in `store`.
    pub fn with_operations(self, store: OperationStore) -> Self {
        Self {
            operations: Some(store),
            ..self
        }
    }

    /// Names and keeps `op` (superset); Java's operations are returned as they are.
    fn finish(&self, project_id: &str, mut op: Operation) -> Operation {
        match &self.operations {
            Some(store) => {
                op.name = format!("projects/{project_id}/operations/{}", operation_id());
                store.record(op)
            }
            None => op,
        }
    }

    pub fn into_server(self) -> admin::datastore_admin_server::DatastoreAdminServer<Self> {
        admin::datastore_admin_server::DatastoreAdminServer::new(self)
    }

    /// The serialized `EntityProto`s of one entity set: stored v3 protos as they are, v1 entities
    /// in their user-visible encoding. Marks them stripped (Java's export side effect).
    fn export_set(&self, project_id: &str, spec: &EntitySetSpec) -> Vec<Vec<u8>> {
        self.engine
            .export_entities(project_id, spec.namespace_filter(), spec.kind_filter())
            .iter()
            .map(|e| match &e.v3 {
                Some(stored) => stored.to_vec(),
                None => v3::encode_entity(e),
            })
            .collect()
    }
}

fn timestamp() -> prost_types::Timestamp {
    std::time::SystemTime::now().into()
}

fn any<M: Message>(type_name: &str, message: &M) -> prost_types::Any {
    prost_types::Any {
        type_url: format!("type.googleapis.com/{type_name}"),
        value: message.encode_to_vec(),
    }
}

fn common(
    start: prost_types::Timestamp,
    operation_type: admin::OperationType,
    labels: std::collections::HashMap<String, String>,
) -> admin::CommonMetadata {
    admin::CommonMetadata {
        start_time: Some(start),
        end_time: Some(timestamp()),
        operation_type: operation_type as i32,
        labels,
        state: admin::common_metadata::State::Successful as i32,
    }
}

/// Where `gs://` URLs are stored locally (superset): `DATASTORE_OXIDE_GCS_ROOT`, else `gcs` in
/// the working directory.
fn gcs_root() -> PathBuf {
    std::env::var_os("DATASTORE_OXIDE_GCS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join("gcs")
        })
}

/// The local path of a `gs://bucket[/path]` URL (superset), or `None` for other URLs.
fn gcs_path(url: &str) -> Option<Result<PathBuf, Status>> {
    let rest = url.strip_prefix("gs://")?;
    let (bucket, path) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() || path.split('/').any(|p| p == "..") {
        return Some(Err(invalid(format!("Invalid Cloud Storage URL: {url}"))));
    }
    let mut local = gcs_root().join(bucket);
    for part in path.split('/').filter(|p| !p.is_empty()) {
        local.push(part);
    }
    Some(Ok(local))
}

/// A stable index id derived from its definition.
fn index_id(index: &CompositeIndex) -> String {
    use std::hash::{Hash, Hasher};
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut h = std::collections::hash_map::DefaultHasher::new();
    index.kind.hash(&mut h);
    index.ancestor.hash(&mut h);
    for p in &index.properties {
        p.name.hash(&mut h);
        p.direction.map(|d| d == Direction::Descending).hash(&mut h);
    }
    let mut n = h.finish();
    let mut id = String::from("CI");
    for _ in 0..10 {
        id.push(ALPHABET[(n % 64) as usize] as char);
        n /= 64;
    }
    id
}

fn index_to_proto(project_id: &str, index: &CompositeIndex) -> admin::Index {
    use admin::index::{AncestorMode, Direction as D, IndexedProperty, State};
    admin::Index {
        project_id: project_id.to_string(),
        index_id: index_id(index),
        kind: index.kind.clone(),
        ancestor: if index.ancestor {
            AncestorMode::AllAncestors
        } else {
            AncestorMode::None
        } as i32,
        properties: index
            .properties
            .iter()
            .map(|p| IndexedProperty {
                name: p.name.clone(),
                direction: match p.direction {
                    Some(Direction::Descending) => D::Descending,
                    _ => D::Ascending,
                } as i32,
            })
            .collect(),
        state: State::Ready as i32,
    }
}

fn index_from_proto(index: &admin::Index) -> Result<CompositeIndex, Status> {
    use admin::index::{AncestorMode, Direction as D};
    if !index.index_id.is_empty() {
        return Err(invalid(
            "The index_id is assigned by the server and must not be set.",
        ));
    }
    if index.kind.is_empty() {
        return Err(invalid("The index must have a kind."));
    }
    let ancestor = match AncestorMode::try_from(index.ancestor) {
        Ok(AncestorMode::AllAncestors) => true,
        Ok(AncestorMode::None) => false,
        _ => return Err(invalid("The index must specify an ancestor mode.")),
    };
    let properties = index
        .properties
        .iter()
        .map(|p| {
            if p.name.is_empty() {
                return Err(invalid("An indexed property must have a name."));
            }
            let direction = match D::try_from(p.direction) {
                Ok(D::Ascending) => Direction::Ascending,
                Ok(D::Descending) => Direction::Descending,
                _ => return Err(invalid("An indexed property must specify a direction.")),
            };
            Ok(IndexProperty {
                name: p.name.clone(),
                direction: Some(direction),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if properties.len() < 2 && !(ancestor && properties.len() == 1) {
        return Err(invalid(
            "A composite index needs at least two properties, or one property and an ancestor.",
        ));
    }
    Ok(CompositeIndex {
        kind: index.kind.clone(),
        ancestor,
        properties,
    })
}

/// The export directory and the reported `output_url` for a raw `output_url_prefix`: a plain path
/// (no scheme handling) with repeated and trailing `/` removed, relative to the working
/// directory. An empty prefix writes at the filesystem root and reports the working directory.
fn output_location(raw: &str) -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut cleaned = String::with_capacity(raw.len());
    for c in raw.chars() {
        if !(c == '/' && cleaned.ends_with('/')) {
            cleaned.push(c);
        }
    }
    if cleaned.len() > 1 && cleaned.ends_with('/') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        return (PathBuf::from("/"), cwd);
    }
    let path = cwd.join(cleaned);
    (path.clone(), path)
}

/// A random operation id: 32 lowercase hex digits.
fn operation_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", half(1), half(2))
}

impl AdminService {
    fn find_index(&self, id: &str) -> Result<CompositeIndex, Status> {
        self.engine
            .defined_indexes()
            .into_iter()
            .find(|i| index_id(i) == id)
            .ok_or_else(|| Status::not_found(format!("Index {id} not found.")))
    }

    fn index_operation(
        &self,
        project_id: &str,
        start: prost_types::Timestamp,
        operation_type: admin::OperationType,
        index: &CompositeIndex,
    ) -> Operation {
        let proto = index_to_proto(project_id, index);
        let metadata = admin::IndexOperationMetadata {
            common: Some(common(start, operation_type, Default::default())),
            progress_entities: None,
            index_id: proto.index_id.clone(),
        };
        self.finish(
            project_id,
            Operation {
                name: String::new(),
                metadata: Some(any(
                    "google.datastore.admin.v1.IndexOperationMetadata",
                    &metadata,
                )),
                done: true,
                result: Some(operation::Result::Response(any(
                    "google.datastore.admin.v1.Index",
                    &proto,
                ))),
            },
        )
    }
}

/// The v3 put checks an imported entity must pass; failures are bare UNKNOWN as in Java.
fn check_imported(record: &[u8], entity: &oxide_model::Entity) -> Result<(), Status> {
    let unknown = || Status::unknown("");
    let key = entity.key.as_ref().ok_or_else(unknown)?;
    let (_, ancestors) = key.path.split_last().ok_or_else(unknown)?;
    if ancestors.iter().any(|e| e.id.is_none()) {
        return Err(unknown());
    }
    let proto =
        oxide_proto::storage_onestore_v3::EntityProto::decode(record).map_err(|_| unknown())?;
    if proto.entity_group.element.len() > 1 {
        return Err(unknown());
    }
    Ok(())
}

#[tonic::async_trait]
impl admin::datastore_admin_server::DatastoreAdmin for AdminService {
    async fn export_entities(
        &self,
        request: Request<admin::ExportEntitiesRequest>,
    ) -> Result<Response<Operation>, Status> {
        let start = timestamp();
        let req = request.into_inner();
        let raw = &req.output_url_prefix;
        let superset = self.operations.is_some();
        let gcs = superset.then(|| gcs_path(raw)).flatten().transpose()?;
        let (dir, output_url) = match &gcs {
            Some(local) => (local.clone(), PathBuf::from(raw.trim_end_matches('/'))),
            None => output_location(raw),
        };
        // Production nests each export in a folder of its own, so a gs:// prefix may exist.
        if gcs.is_none() && !raw.is_empty() && dir.exists() {
            return Err(crate::convert::invalid(format!(
                "The output_url_prefix: {raw} already exists"
            )));
        }
        let filter = req.entity_filter.clone().unwrap_or_default();
        // Like Java: after each set, a fresh directory holding every set so far.
        let mut sets: Vec<(EntitySetSpec, Vec<Vec<u8>>)> = Vec::new();
        let mut last_name = String::new();
        for spec in export::entity_set_specs(&filter.kinds, &filter.namespace_ids) {
            let entities = self.export_set(&req.project_id, &spec);
            if !entities.is_empty() {
                match sets.iter_mut().find(|(s, _)| *s == spec) {
                    Some((_, existing)) => existing.extend(entities),
                    None => sets.push((spec, entities)),
                }
            }
            let name = export::export_name(export::now_micros());
            export::write_export(&dir, &name, &sets)
                .map_err(|_| Status::internal("Failed to export entities."))?;
            last_name = name;
        }
        // Production reports the overall metadata file, which imports take as input_url.
        let output_url = if superset {
            output_url
                .join(&last_name)
                .join(format!("{last_name}.overall_export_metadata"))
        } else {
            output_url
        };
        let labels = if superset {
            req.labels.clone()
        } else {
            Default::default()
        };
        let metadata = admin::ExportEntitiesMetadata {
            common: Some(common(start, admin::OperationType::ExportEntities, labels)),
            progress_entities: None,
            progress_bytes: None,
            entity_filter: Some(filter),
            output_url_prefix: req.output_url_prefix.clone(),
        };
        let response = admin::ExportEntitiesResponse {
            output_url: output_url.to_string_lossy().into_owned(),
        };
        Ok(Response::new(self.finish(
            &req.project_id,
            Operation {
                name: String::new(),
                metadata: Some(any(
                    "google.datastore.admin.v1.ExportEntitiesMetadata",
                    &metadata,
                )),
                done: true,
                result: Some(operation::Result::Response(any(
                    "google.datastore.admin.v1.ExportEntitiesResponse",
                    &response,
                ))),
            },
        )))
    }

    async fn import_entities(
        &self,
        request: Request<admin::ImportEntitiesRequest>,
    ) -> Result<Response<Operation>, Status> {
        let start = timestamp();
        let req = request.into_inner();
        let filter = req.entity_filter.clone().unwrap_or_default();
        let specs = export::entity_set_specs(&filter.kinds, &filter.namespace_ids);
        let superset = self.operations.is_some();
        let input = match superset.then(|| gcs_path(&req.input_url)).flatten() {
            Some(local) => local?,
            None => PathBuf::from(&req.input_url),
        };
        let records = export::read_export(&input, &specs).map_err(|e| match e {
            ImportError::Invalid(message) => crate::convert::invalid(message),
            ImportError::Unknown => Status::unknown(""),
        })?;
        for batch in records.chunks(IMPORT_BATCH) {
            let mut entities = Vec::with_capacity(batch.len());
            for record in batch {
                let entity = crate::appengine::entity::imported(record, &req.project_id)
                    .ok_or_else(|| Status::unknown(""))?;
                check_imported(record, &entity)?;
                entities.push(entity);
            }
            self.engine.import_entities(&req.project_id, entities);
        }
        let labels = if superset {
            req.labels.clone()
        } else {
            Default::default()
        };
        let metadata = admin::ImportEntitiesMetadata {
            common: Some(common(start, admin::OperationType::ImportEntities, labels)),
            progress_entities: None,
            progress_bytes: None,
            entity_filter: Some(filter),
            input_url: req.input_url.clone(),
        };
        Ok(Response::new(self.finish(
            &req.project_id,
            Operation {
                name: format!("projects/{}/operations/{}", req.project_id, operation_id()),
                metadata: Some(any(
                    "google.datastore.admin.v1.ImportEntitiesMetadata",
                    &metadata,
                )),
                done: true,
                result: Some(operation::Result::Response(any(
                    "google.protobuf.Empty",
                    &(),
                ))),
            },
        )))
    }

    async fn create_index(
        &self,
        request: Request<admin::CreateIndexRequest>,
    ) -> Result<Response<Operation>, Status> {
        if self.operations.is_none() {
            return Err(Status::unimplemented(""));
        }
        let start = timestamp();
        let r = request.into_inner();
        let index = index_from_proto(&r.index.unwrap_or_default())?;
        if !self.engine.add_admin_index(index.clone()) {
            return Err(Status::already_exists("The index already exists."));
        }
        Ok(Response::new(self.index_operation(
            &r.project_id,
            start,
            admin::OperationType::CreateIndex,
            &index,
        )))
    }

    async fn delete_index(
        &self,
        request: Request<admin::DeleteIndexRequest>,
    ) -> Result<Response<Operation>, Status> {
        if self.operations.is_none() {
            return Err(Status::unimplemented(""));
        }
        let start = timestamp();
        let r = request.into_inner();
        let index = self.find_index(&r.index_id)?;
        if !self.engine.remove_admin_index(&index) {
            return Err(Status::failed_precondition(
                "Only indexes created through the Admin API can be deleted; the others come from the index configuration.",
            ));
        }
        Ok(Response::new(self.index_operation(
            &r.project_id,
            start,
            admin::OperationType::DeleteIndex,
            &index,
        )))
    }

    async fn get_index(
        &self,
        request: Request<admin::GetIndexRequest>,
    ) -> Result<Response<admin::Index>, Status> {
        if self.operations.is_none() {
            return Err(Status::unimplemented(""));
        }
        let r = request.into_inner();
        let index = self.find_index(&r.index_id)?;
        Ok(Response::new(index_to_proto(&r.project_id, &index)))
    }

    async fn list_indexes(
        &self,
        request: Request<admin::ListIndexesRequest>,
    ) -> Result<Response<admin::ListIndexesResponse>, Status> {
        if self.operations.is_none() {
            return Err(Status::unimplemented(""));
        }
        let r = request.into_inner();
        let all: Vec<admin::Index> = self
            .engine
            .defined_indexes()
            .iter()
            .map(|i| index_to_proto(&r.project_id, i))
            .collect();
        let (indexes, next_page_token) = crate::operations::page(&all, r.page_size, &r.page_token)?;
        Ok(Response::new(admin::ListIndexesResponse {
            indexes,
            next_page_token,
        }))
    }
}
