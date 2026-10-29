//! The Datastore Admin v1 service: synchronous export and import in the managed-export format;
//! the index methods are unimplemented, as in Java.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use oxide_engine::Engine;
use oxide_proto::google::datastore::admin::v1 as admin;
use oxide_proto::google::longrunning::{Operation, operation};
use prost::Message;
use tonic::{Request, Response, Status};

use crate::export::{self, EntitySetSpec, ImportError};
use crate::v3;

/// Entities written per import batch (each batch is validated before it is written).
const IMPORT_BATCH: usize = 5000;

pub struct AdminService {
    engine: Arc<Engine>,
}

impl AdminService {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
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
) -> admin::CommonMetadata {
    admin::CommonMetadata {
        start_time: Some(start),
        end_time: Some(timestamp()),
        operation_type: operation_type as i32,
        labels: Default::default(),
        state: admin::common_metadata::State::Successful as i32,
    }
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
        let (dir, output_url) = output_location(raw);
        if !raw.is_empty() && dir.exists() {
            return Err(crate::convert::invalid(format!(
                "The output_url_prefix: {raw} already exists"
            )));
        }
        let filter = req.entity_filter.clone().unwrap_or_default();
        // Like Java: after each set, a fresh directory holding every set so far.
        let mut sets: Vec<(EntitySetSpec, Vec<Vec<u8>>)> = Vec::new();
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
        }
        let metadata = admin::ExportEntitiesMetadata {
            common: Some(common(start, admin::OperationType::ExportEntities)),
            progress_entities: None,
            progress_bytes: None,
            entity_filter: Some(filter),
            output_url_prefix: req.output_url_prefix.clone(),
        };
        let response = admin::ExportEntitiesResponse {
            output_url: output_url.to_string_lossy().into_owned(),
        };
        Ok(Response::new(Operation {
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
        }))
    }

    async fn import_entities(
        &self,
        request: Request<admin::ImportEntitiesRequest>,
    ) -> Result<Response<Operation>, Status> {
        let start = timestamp();
        let req = request.into_inner();
        let filter = req.entity_filter.clone().unwrap_or_default();
        let specs = export::entity_set_specs(&filter.kinds, &filter.namespace_ids);
        let records =
            export::read_export(Path::new(&req.input_url), &specs).map_err(|e| match e {
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
        let metadata = admin::ImportEntitiesMetadata {
            common: Some(common(start, admin::OperationType::ImportEntities)),
            progress_entities: None,
            progress_bytes: None,
            entity_filter: Some(filter),
            input_url: req.input_url.clone(),
        };
        Ok(Response::new(Operation {
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
        }))
    }

    async fn create_index(
        &self,
        _: Request<admin::CreateIndexRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(Status::unimplemented(""))
    }

    async fn delete_index(
        &self,
        _: Request<admin::DeleteIndexRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(Status::unimplemented(""))
    }

    async fn get_index(
        &self,
        _: Request<admin::GetIndexRequest>,
    ) -> Result<Response<admin::Index>, Status> {
        Err(Status::unimplemented(""))
    }

    async fn list_indexes(
        &self,
        _: Request<admin::ListIndexesRequest>,
    ) -> Result<Response<admin::ListIndexesResponse>, Status> {
        Err(Status::unimplemented(""))
    }
}
