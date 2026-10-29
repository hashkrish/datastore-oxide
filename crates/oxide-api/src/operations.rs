//! `google.longrunning.Operations` for the Admin API's operations (superset only: Java has no
//! Operations service). Every operation finishes before it is returned, so the store only keeps
//! finished operations for `GetOperation` / `ListOperations` and lets them be deleted.

use std::sync::{Arc, Mutex};

use oxide_proto::google::longrunning::{self as lro, Operation};
use tonic::{Request, Response, Status};

use crate::convert::invalid;

/// Finished operations by name, in creation order.
#[derive(Clone, Default)]
pub struct OperationStore(Arc<Mutex<Vec<Operation>>>);

impl OperationStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Operation>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keeps `op` (it must have a name) and returns it.
    pub fn record(&self, op: Operation) -> Operation {
        self.lock().push(op.clone());
        op
    }

    fn get(&self, name: &str) -> Result<Operation, Status> {
        self.lock()
            .iter()
            .find(|o| o.name == name)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("Operation {name} not found.")))
    }

    /// Drops every operation (`/reset`).
    pub fn clear(&self) {
        self.lock().clear();
    }
}

pub struct OperationsService {
    store: OperationStore,
}

impl OperationsService {
    pub fn new(store: OperationStore) -> Self {
        Self { store }
    }

    pub fn into_server(self) -> lro::operations_server::OperationsServer<Self> {
        lro::operations_server::OperationsServer::new(self)
    }
}

/// A page of `items` from the offset in `token` (empty: the start).
pub fn page<T: Clone>(items: &[T], size: i32, token: &str) -> Result<(Vec<T>, String), Status> {
    let start: usize = if token.is_empty() {
        0
    } else {
        token
            .parse()
            .map_err(|_| invalid(format!("Invalid page token \"{token}\".")))?
    };
    let rest = items.get(start..).unwrap_or_default();
    let size = if size > 0 { size as usize } else { rest.len() };
    let next = if rest.len() > size {
        (start + size).to_string()
    } else {
        String::new()
    };
    Ok((rest.iter().take(size).cloned().collect(), next))
}

#[tonic::async_trait]
impl lro::operations_server::Operations for OperationsService {
    /// `name` is the collection's parent, `projects/{project}`.
    async fn list_operations(
        &self,
        request: Request<lro::ListOperationsRequest>,
    ) -> Result<Response<lro::ListOperationsResponse>, Status> {
        let r = request.into_inner();
        let prefix = format!("{}/operations/", r.name.trim_end_matches("/operations"));
        let matching: Vec<Operation> = self
            .store
            .lock()
            .iter()
            .filter(|o| o.name.starts_with(&prefix))
            .cloned()
            .collect();
        let (operations, next_page_token) = page(&matching, r.page_size, &r.page_token)?;
        Ok(Response::new(lro::ListOperationsResponse {
            operations,
            next_page_token,
            unreachable: Vec::new(),
        }))
    }

    async fn get_operation(
        &self,
        request: Request<lro::GetOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        Ok(Response::new(self.store.get(&request.into_inner().name)?))
    }

    async fn delete_operation(
        &self,
        request: Request<lro::DeleteOperationRequest>,
    ) -> Result<Response<()>, Status> {
        let name = request.into_inner().name;
        self.store.get(&name)?;
        self.store.lock().retain(|o| o.name != name);
        Ok(Response::new(()))
    }

    /// Operations are finished when they are returned: cancelling one has no effect.
    async fn cancel_operation(
        &self,
        request: Request<lro::CancelOperationRequest>,
    ) -> Result<Response<()>, Status> {
        self.store.get(&request.into_inner().name)?;
        Ok(Response::new(()))
    }

    async fn wait_operation(
        &self,
        request: Request<lro::WaitOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        Ok(Response::new(self.store.get(&request.into_inner().name)?))
    }
}
