//! The emulator's API surface: request validation and conversion, gRPC services, and the REST
//! gateway that transcodes HTTP/JSON onto them.

pub mod admin;
pub mod aggregation;
pub mod appengine;
pub mod config;
pub mod convert;
pub mod explain;
pub mod export;
pub mod gql;
pub mod persist;
pub mod query;
pub mod rest;
pub mod service;
pub mod v3;

pub use rest::RestGateway;
pub use service::DatastoreService;
