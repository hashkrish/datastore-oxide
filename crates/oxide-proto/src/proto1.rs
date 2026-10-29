/// Adapts the App Engine v3 packages (`storage_onestore_v3`, `apphosting_datastore_v3`) to how
/// their Java runtime (proto1) encodes them:
/// - `string` fields become `bytes`: proto1 strings are byte strings (blob, byte-string and
///   embedded-entity values travel in `PropertyValue.stringValue`, and names need not be UTF-8);
/// - fields are ordered by number, so prost serializes them in field-number order as proto1 does.
pub fn as_proto1(fds: &mut prost_types::FileDescriptorSet) {
    use prost_types::field_descriptor_proto::Type;
    fn patch(m: &mut prost_types::DescriptorProto) {
        for f in &mut m.field {
            if f.r#type() == Type::String {
                f.set_type(Type::Bytes);
            }
        }
        m.field.sort_by_key(|f| f.number());
        m.nested_type.iter_mut().for_each(patch);
    }
    for file in &mut fds.file {
        if matches!(
            file.package(),
            "storage_onestore_v3" | "apphosting_datastore_v3"
        ) {
            file.message_type.iter_mut().for_each(patch);
        }
    }
}
