//! Generates Rust types and tonic services, with protoc, from:
//! - the vendored googleapis protos (`proto/googleapis`);
//! - the vendored App Engine v3 datastore protos (`proto/appengine`, from appengine-java-standard);
//! - the emulator's own services (`proto/emulator`: the config service and the CallHandler envelope).
//!
//! The combined, unmodified descriptor set is also written to `OUT_DIR/descriptors.binpb` so the
//! server can expose gRPC reflection and read `google.api.http` rules for REST routing.

use std::path::PathBuf;
use std::process::Command;

use prost::Message;

const ROOTS: &[&str] = &[
    "google/datastore/v1/datastore.proto",
    "google/datastore/admin/v1/datastore_admin.proto",
    "google/longrunning/operations.proto",
    "datastore_v3_bytes.proto",
    "emulator_config.proto",
    "grpc_service.proto",
];

fn main() -> std::io::Result<()> {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", proto_dir.display());
    println!("cargo:rerun-if-env-changed=PROTOC");

    let descriptors = out_dir.join("descriptors.binpb");
    let protoc = std::env::var("PROTOC").unwrap_or_else(|_| "protoc".into());
    let status = Command::new(&protoc)
        .arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", descriptors.display()))
        .args(
            ["googleapis", "appengine", "emulator"]
                .map(|dir| format!("-I{}", proto_dir.join(dir).display())),
        )
        .args(ROOTS)
        .status()
        .map_err(|e| std::io::Error::other(format!("running {protoc}: {e}")))?;
    if !status.success() {
        return Err(std::io::Error::other(format!("{protoc} failed: {status}")));
    }

    // Written back with the v3 additions so the server's reflection and text-format parsing see
    // them too. Only that file is re-encoded: prost drops extension options such as the
    // `google.api.http` rules the REST gateway reads from the other files.
    let mut raw = Vec::new();
    for entry in set_entries(&std::fs::read(&descriptors)?) {
        let file =
            prost_types::FileDescriptorProto::decode(entry.value).map_err(std::io::Error::other)?;
        if file.name() == "datastore_v3_bytes.proto" {
            let mut set = prost_types::FileDescriptorSet { file: vec![file] };
            add_v3_emulator_fields(&mut set.file[0]);
            raw.extend(set.encode_to_vec());
        } else {
            raw.extend_from_slice(entry.raw);
        }
    }
    std::fs::write(&descriptors, &raw)?;
    let mut fds =
        prost_types::FileDescriptorSet::decode(raw.as_slice()).map_err(std::io::Error::other)?;
    add_java_only_fields(&mut fds);
    as_proto1(&mut fds);
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .bytes(".")
        .include_file("mod.rs")
        .compile_fds(fds)
}

include!("src/proto1.rs");

/// One `file` entry of a serialized `FileDescriptorSet`: its full encoding and its value.
struct Entry<'a> {
    raw: &'a [u8],
    value: &'a [u8],
}

/// The `file` entries (field 1, length-delimited) of a serialized `FileDescriptorSet`.
fn set_entries(mut b: &[u8]) -> Vec<Entry<'_>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let start = b;
        assert_eq!(b[0], 0x0a, "FileDescriptorSet.file");
        b = &b[1..];
        let len = prost::encoding::decode_varint(&mut b).expect("length") as usize;
        let (value, rest) = b.split_at(len);
        out.push(Entry {
            raw: &start[..start.len() - rest.len()],
            value,
        });
        b = rest;
    }
    out
}

/// Fields the Java emulator's protos have but googleapis does not, added so the services can set
/// them (gRPC clients ignore them as unknown fields; REST prints them like Java).
fn add_java_only_fields(fds: &mut prost_types::FileDescriptorSet) {
    use prost_types::field_descriptor_proto::{Label, Type};
    let file = fds
        .file
        .iter_mut()
        .find(|f| f.name() == "google/datastore/v1/datastore.proto")
        .expect("datastore.proto");
    let commit_response = file
        .message_type
        .iter_mut()
        .find(|m| m.name() == "CommitResponse")
        .expect("CommitResponse");
    commit_response
        .field
        .push(prost_types::FieldDescriptorProto {
            name: Some("commit_version".into()),
            number: Some(5),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Int64 as i32),
            json_name: Some("commitVersion".into()),
            ..Default::default()
        });
}

/// v3 fields the emulator accepts that the published `datastore_v3_bytes.proto` lacks: the
/// `*_epoch_micros` read and write times and the `NOT_IN` filter operator.
fn add_v3_emulator_fields(file: &mut prost_types::FileDescriptorProto) {
    use prost_types::field_descriptor_proto::{Label, Type};
    fn message<'a>(
        messages: &'a mut [prost_types::DescriptorProto],
        path: &str,
    ) -> &'a mut prost_types::DescriptorProto {
        let (name, rest) = path
            .split_once('.')
            .map_or((path, None), |(n, r)| (n, Some(r)));
        let m = messages
            .iter_mut()
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("message {name}"));
        match rest {
            Some(rest) => message(&mut m.nested_type, rest),
            None => m,
        }
    }
    for (path, name, number, label) in [
        ("GetRequest", "read_time_epoch_micros", 6, Label::Optional),
        ("GetResponse", "read_time_epoch_micros", 7, Label::Optional),
        (
            "GetResponse.Entity",
            "update_time_epoch_micros",
            8,
            Label::Optional,
        ),
        (
            "PutResponse",
            "update_time_epoch_micros",
            4,
            Label::Repeated,
        ),
        (
            "DeleteResponse",
            "delete_time_epoch_micros",
            4,
            Label::Repeated,
        ),
        ("Query", "read_time_epoch_micros", 45, Label::Optional),
        ("QueryResult", "read_time_epoch_micros", 14, Label::Optional),
        (
            "QueryResult",
            "update_time_epoch_micros",
            15,
            Label::Repeated,
        ),
        (
            "CommitResponse",
            "commit_time_epoch_micros",
            7,
            Label::Optional,
        ),
        (
            "CommitResponse.Version",
            "update_time_epoch_micros",
            6,
            Label::Optional,
        ),
    ] {
        let m = message(&mut file.message_type, path);
        assert!(
            m.field
                .iter()
                .all(|f| f.number() != number && f.name() != name)
        );
        m.field.push(prost_types::FieldDescriptorProto {
            name: Some(name.into()),
            number: Some(number),
            label: Some(label as i32),
            r#type: Some(Type::Int64 as i32),
            ..Default::default()
        });
    }
    let op = message(&mut file.message_type, "Query.Filter")
        .enum_type
        .iter_mut()
        .find(|e| e.name() == "Operator")
        .expect("Query.Filter.Operator");
    op.value.push(prost_types::EnumValueDescriptorProto {
        name: Some("NOT_IN".into()),
        number: Some(10),
        options: None,
    });
}
