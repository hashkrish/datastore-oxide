//! Generated protobuf types and gRPC services.
//!
//! Module paths follow proto packages, e.g. `google::datastore::v1`,
//! `google::datastore::emulator::config::v1`, `apphosting_datastore_v3`.

#![allow(clippy::all, clippy::pedantic, rustdoc::all, missing_docs)]

include!(concat!(env!("OUT_DIR"), "/mod.rs"));

/// Serialized `FileDescriptorSet` covering every generated file, with options intact.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/descriptors.binpb"));

include!("proto1.rs");
