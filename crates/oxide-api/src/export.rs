//! The managed-export file format (Admin v1 export/import): LevelDB logs of v3 `EntityProto`s, a
//! `Backup` proto per entity set and an `.overall_export_metadata` log pointing at them.

use std::path::{Path, PathBuf};

use prost::Message;

const BLOCK_SIZE: usize = 32768;
const HEADER_LEN: usize = 7;
const FULL: u8 = 1;
const FIRST: u8 = 2;
const MIDDLE: u8 = 3;
const LAST: u8 = 4;
/// Entities per `output-N` shard.
const SHARD_SIZE: usize = 5000;
const VERSION: &[u8] = b"3";

const KIND_ONE: i32 = 1;
const KIND_ALL: i32 = 2;
const NS_DEFAULT: i32 = 1;
const NS_ONE: i32 = 2;
const NS_ALL: i32 = 3;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32C of the type byte followed by the payload, masked as LevelDB stores it.
fn masked_crc(kind: u8, payload: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in std::iter::once(&kind).chain(payload) {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    let c = !c;
    c.rotate_right(15).wrapping_add(0xA282_EAD8)
}

/// Writes records in the LevelDB log format, without padding the last block.
#[derive(Default)]
pub struct LogWriter {
    out: Vec<u8>,
}

impl LogWriter {
    /// Appends one record and returns the number of bytes it took (headers and trailers included).
    pub fn write(&mut self, payload: &[u8]) -> usize {
        let start = self.out.len();
        let mut rest = payload;
        let mut first = true;
        loop {
            let left = BLOCK_SIZE - self.out.len() % BLOCK_SIZE;
            let frag = rest.len().min(left - HEADER_LEN);
            let end = frag == rest.len();
            let kind = match (first, end) {
                (true, true) => FULL,
                (true, false) => FIRST,
                (false, true) => LAST,
                (false, false) => MIDDLE,
            };
            let (chunk, tail) = rest.split_at(frag);
            self.out
                .extend_from_slice(&masked_crc(kind, chunk).to_le_bytes());
            self.out.extend_from_slice(&(frag as u16).to_le_bytes());
            self.out.push(kind);
            self.out.extend_from_slice(chunk);
            let left_after = left - HEADER_LEN - frag;
            if left_after < HEADER_LEN {
                self.out.resize(self.out.len() + left_after, 0);
            }
            rest = tail;
            first = false;
            if end {
                return self.out.len() - start;
            }
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Reads the records of a LevelDB log, with the Java reader's checks and messages. A fragmented
/// record cut off by the end of the file is dropped silently.
pub fn read_records(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    fn zeros_until_block_end(bytes: &[u8], pos: usize) -> Result<usize, String> {
        let end = bytes.len().min((pos / BLOCK_SIZE + 1) * BLOCK_SIZE);
        if let Some(i) = bytes[pos..end].iter().position(|&b| b != 0) {
            return Err(format!(
                "Found a non-zero byte: {} before the end of the block {i} bytes after encountering a RecordType of NONE",
                bytes[pos + i] as i8
            ));
        }
        Ok(end)
    }

    let mut records = Vec::new();
    let mut partial: Option<Vec<u8>> = None;
    let mut pos = 0;
    while pos < bytes.len() {
        let block_left = BLOCK_SIZE - pos % BLOCK_SIZE;
        if block_left < HEADER_LEN {
            pos = zeros_until_block_end(bytes, pos)?;
            continue;
        }
        let Some(header) = bytes.get(pos..pos + HEADER_LEN) else {
            return Err("Record header length not valid".into());
        };
        let crc = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let len = usize::from(u16::from_le_bytes([header[4], header[5]]));
        let kind = header[6];
        if len > block_left {
            return Err(format!("Record length is too large:{len}"));
        }
        let start = pos + HEADER_LEN;
        let Some(payload) = bytes.get(start..start + len) else {
            return Err("Invalid record".into());
        };
        pos = start + len;
        if !(crc == 0 && kind == 0 && len == 0) {
            // Unknown types are checked as Java's UNKNOWN record type (-1).
            let checked = if kind <= LAST { kind } else { 0xff };
            if masked_crc(checked, payload) != crc {
                return Err("Checksum doesn't validate.".into());
            }
        }
        match kind {
            0 => {
                pos = zeros_until_block_end(bytes, pos)?;
                partial = None;
            }
            FULL | FIRST if partial.is_some() => {
                let name = if kind == FULL { "FULL" } else { "FIRST" };
                return Err(format!("Invalid RecordType: {name}"));
            }
            FULL => records.push(payload.to_vec()),
            FIRST => partial = Some(payload.to_vec()),
            MIDDLE | LAST => {
                let Some(record) = partial.as_mut() else {
                    let name = if kind == MIDDLE { "MIDDLE" } else { "LAST" };
                    return Err(format!("Invalid RecordType: {name}"));
                };
                record.extend_from_slice(payload);
                if kind == LAST {
                    records.extend(partial.take());
                }
            }
            _ => return Err("Invalid RecordType: -1".into()),
        }
    }
    Ok(records)
}

/// `EntitySetSpec`: one kind (or all) in one namespace (or all).
#[derive(Clone, PartialEq, Message)]
pub struct EntitySetSpec {
    #[prost(int32, optional, tag = "1")]
    pub kind_type: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub kind: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub namespace_type: Option<i32>,
    #[prost(string, optional, tag = "4")]
    pub namespace: Option<String>,
}

impl EntitySetSpec {
    /// The kind to export, or `None` for all kinds.
    pub fn kind_filter(&self) -> Option<&str> {
        (self.kind_type != Some(KIND_ALL)).then(|| self.kind.as_deref().unwrap_or(""))
    }

    /// The namespace to export, or `None` for all namespaces.
    pub fn namespace_filter(&self) -> Option<&str> {
        match self.namespace_type {
            Some(NS_ALL) => None,
            Some(NS_DEFAULT) => Some(""),
            _ => Some(self.namespace.as_deref().unwrap_or("")),
        }
    }

    fn kind_all(&self) -> bool {
        self.kind_type == Some(KIND_ALL)
    }

    fn namespace_all(&self) -> bool {
        self.namespace_type == Some(NS_ALL)
    }

    /// Whether this requested spec can serve an exported spec (ALL only serves ALL).
    fn serves(&self, exported: &EntitySetSpec) -> bool {
        let ns = if exported.namespace_all() {
            self.namespace_all()
        } else {
            self.namespace_all()
                || (self.namespace_type == exported.namespace_type
                    && self.namespace == exported.namespace)
        };
        let kind = if exported.kind_all() {
            self.kind_all()
        } else {
            self.kind_all() || (self.kind_type == exported.kind_type && self.kind == exported.kind)
        };
        ns && kind
    }

    fn dir_names(&self) -> (String, String) {
        let ns = match self.namespace_type {
            Some(NS_ALL) => "all_namespaces".to_string(),
            Some(NS_DEFAULT) => "default_namespace".to_string(),
            _ => format!("namespace_{}", self.namespace.as_deref().unwrap_or("")),
        };
        let kind = match self.kind_type {
            Some(KIND_ALL) => "all_kinds".to_string(),
            _ => format!("kind_{}", self.kind.as_deref().unwrap_or("")),
        };
        (ns, kind)
    }
}

/// The entity sets of an `EntityFilter`: namespace-major, in request order, without dedup.
pub fn entity_set_specs(kinds: &[String], namespace_ids: &[String]) -> Vec<EntitySetSpec> {
    let namespaces: Vec<(i32, &str)> = if namespace_ids.is_empty() {
        vec![(NS_ALL, "")]
    } else {
        namespace_ids
            .iter()
            .map(|ns| (if ns.is_empty() { NS_DEFAULT } else { NS_ONE }, ns.as_str()))
            .collect()
    };
    let kinds: Vec<(i32, &str)> = if kinds.is_empty() {
        vec![(KIND_ALL, "")]
    } else {
        kinds.iter().map(|k| (KIND_ONE, k.as_str())).collect()
    };
    namespaces
        .iter()
        .flat_map(|&(nt, ns)| {
            kinds.iter().map(move |&(kt, kind)| EntitySetSpec {
                kind_type: Some(kt),
                kind: Some(kind.into()),
                namespace_type: Some(nt),
                namespace: Some(ns.into()),
            })
        })
        .collect()
}

#[derive(Clone, PartialEq, Message)]
struct OverallExportMetadata {
    #[prost(message, repeated, tag = "1")]
    export_pointers: Vec<ExportPointer>,
}

#[derive(Clone, PartialEq, Message)]
struct ExportPointer {
    #[prost(message, optional, tag = "1")]
    spec: Option<EntitySetSpec>,
    #[prost(string, optional, tag = "2")]
    export_metadata_file: Option<String>,
    #[prost(int64, optional, tag = "3")]
    num_entities: Option<i64>,
    #[prost(int64, optional, tag = "4")]
    num_bytes: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
struct Backup {
    #[prost(message, optional, tag = "1")]
    backup_info: Option<BackupInfo>,
    #[prost(message, repeated, tag = "2")]
    kind_info: Vec<KindBackupInfo>,
}

#[derive(Clone, PartialEq, Message)]
struct BackupInfo {
    #[prost(string, optional, tag = "1")]
    backup_name: Option<String>,
    #[prost(int64, optional, tag = "2")]
    start_timestamp: Option<i64>,
    #[prost(int64, optional, tag = "3")]
    end_timestamp: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
struct KindBackupInfo {
    #[prost(string, optional, tag = "1")]
    kind: Option<String>,
    #[prost(string, repeated, tag = "2")]
    file: Vec<String>,
}

/// Microseconds since the Unix epoch.
pub fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

/// The export directory name: UTC `yyyy-MM-ddTHH:mm:ss_SSS` (milliseconds truncated).
pub fn export_name(micros: i64) -> String {
    let millis = micros.div_euclid(1000);
    let secs = millis.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}_{:03}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        millis.rem_euclid(1000)
    )
}

/// Writes `<prefix>/<name>/` for the given entity sets (each a spec and its serialized
/// `EntityProto`s): per set a `Backup` file and `output-N` shards, then the overall metadata.
pub fn write_export(
    prefix: &Path,
    name: &str,
    sets: &[(EntitySetSpec, Vec<Vec<u8>>)],
) -> std::io::Result<()> {
    let root = prefix.join(name);
    std::fs::create_dir_all(&root)?;
    let mut pointers = Vec::with_capacity(sets.len());
    for (spec, entities) in sets {
        let (ns_dir, kind_dir) = spec.dir_names();
        let dir = root.join(&ns_dir).join(&kind_dir);
        std::fs::create_dir_all(&dir)?;
        let start = now_micros();
        let mut files = Vec::new();
        let mut num_bytes = 0;
        for (i, shard) in entities.chunks(SHARD_SIZE).enumerate() {
            let mut log = LogWriter::default();
            for entity in shard {
                log.write(entity);
            }
            let bytes = log.into_bytes();
            num_bytes += bytes.len() as i64;
            let file = format!("output-{i}");
            std::fs::write(dir.join(&file), bytes)?;
            files.push(file);
        }
        let backup = Backup {
            backup_info: Some(BackupInfo {
                backup_name: Some(name.into()),
                start_timestamp: Some(start),
                end_timestamp: Some(now_micros()),
            }),
            kind_info: vec![KindBackupInfo {
                kind: Some(spec.kind.clone().unwrap_or_default()),
                file: files,
            }],
        };
        let metadata_name = format!("{ns_dir}_{kind_dir}.export_metadata");
        std::fs::write(dir.join(&metadata_name), backup.encode_to_vec())?;
        pointers.push(ExportPointer {
            spec: Some(spec.clone()),
            export_metadata_file: Some(format!("{ns_dir}/{kind_dir}/{metadata_name}")),
            num_entities: Some(entities.len() as i64),
            num_bytes: Some(num_bytes),
        });
    }
    let mut log = LogWriter::default();
    log.write(VERSION);
    log.write(
        &OverallExportMetadata {
            export_pointers: pointers,
        }
        .encode_to_vec(),
    );
    std::fs::write(
        root.join(format!("{name}.overall_export_metadata")),
        log.into_bytes(),
    )
}

/// Why an import failed: a 400 with a message, or a bare UNKNOWN.
#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    Invalid(String),
    Unknown,
}

/// Reads every entity record of the export whose overall metadata file is `input`, keeping only
/// the entity sets the requested specs select (all of them for the empty filter).
pub fn read_export(input: &Path, requested: &[EntitySetSpec]) -> Result<Vec<Vec<u8>>, ImportError> {
    use ImportError::{Invalid, Unknown};
    let unparsable = || Invalid("Failed to parse overall export metadata file".into());
    let bytes = std::fs::read(input).map_err(|_| unparsable())?;
    let mut records = read_records(&bytes).map_err(Invalid)?.into_iter();
    match records.next() {
        None => return Err(Unknown),
        Some(version) if version != VERSION => {
            return Err(Invalid(
                "Overall export metadata file verion not supported".into(),
            ));
        }
        Some(_) => {}
    }
    let overall = records.next().ok_or(Unknown)?;
    let overall = OverallExportMetadata::decode(overall.as_slice()).map_err(|_| unparsable())?;

    let everything = matches!(requested, [r] if r.kind_all() && r.namespace_all());
    let mut matched = vec![false; requested.len()];
    let mut pointers = Vec::new();
    for pointer in overall.export_pointers {
        if everything {
            pointers.push(pointer);
            continue;
        }
        let spec = pointer.spec.clone().unwrap_or_default();
        if let Some(i) = requested.iter().position(|r| r.serves(&spec)) {
            matched[i] = true;
            pointers.push(pointer);
        }
    }
    if !everything && matched.contains(&false) {
        return Err(Invalid(
            "The requested kinds/namespaces are not available".into(),
        ));
    }

    let base = input.parent().unwrap_or(Path::new(""));
    let mut entities = Vec::new();
    for pointer in pointers {
        let path = base.join(pointer.export_metadata_file.unwrap_or_default());
        let bytes = std::fs::read(&path).map_err(|e| Invalid(io_message(&path, &e)))?;
        // protobuf-java's message for the common case; prost's errors differ.
        let backup = Backup::decode(bytes.as_slice())
            .map_err(|_| Invalid("Protocol message tag had invalid wire type.".into()))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        for file in backup.kind_info.iter().flat_map(|k| &k.file) {
            let path: PathBuf = dir.join(file);
            let failed = || Invalid(format!("Failed parse entity file:{}", path.display()));
            let bytes = std::fs::read(&path).map_err(|_| failed())?;
            for record in read_records(&bytes).map_err(Invalid)? {
                oxide_proto::storage_onestore_v3::EntityProto::decode(record.as_slice())
                    .map_err(|_| failed())?;
                entities.push(record);
            }
        }
    }
    Ok(entities)
}

/// Java's `FileNotFoundException` message: `<path> (<reason>)`.
fn io_message(path: &Path, e: &std::io::Error) -> String {
    let reason = match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        std::io::ErrorKind::IsADirectory => "Is a directory".to_string(),
        _ => {
            let text = e.to_string();
            match text.find(" (os error") {
                Some(i) => text[..i].to_string(),
                None => text,
            }
        }
    };
    format!("{} ({reason})", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(records: &[&[u8]]) -> Vec<u8> {
        let mut w = LogWriter::default();
        for r in records {
            w.write(r);
        }
        w.into_bytes()
    }

    fn header(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = masked_crc(kind, payload).to_le_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.push(kind);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn version_record_matches_java() {
        assert_eq!(
            log(&[b"3"]),
            [0xb8, 0x6d, 0x44, 0x4e, 0x01, 0x00, 0x01, 0x33]
        );
        assert_eq!(log(&[b""]), [0x05, 0x2b, 0x28, 0x43, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn round_trips_across_blocks() {
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let records: Vec<&[u8]> = vec![b"a", &big, b"", &big[..32_000], b"z"];
        let bytes = log(&records);
        assert_eq!(read_records(&bytes).unwrap(), records);
    }

    #[test]
    fn writer_fills_blocks_like_leveldb() {
        // Leaves exactly 7 bytes in the first block: the next record starts with an empty FIRST.
        let first = vec![1u8; BLOCK_SIZE - 2 * HEADER_LEN];
        let mut w = LogWriter::default();
        assert_eq!(w.write(&first), BLOCK_SIZE - HEADER_LEN);
        assert_eq!(w.write(b"xy"), HEADER_LEN + HEADER_LEN + 2);
        let bytes = w.into_bytes();
        assert_eq!(
            bytes[BLOCK_SIZE - HEADER_LEN + 4..BLOCK_SIZE],
            [0, 0, FIRST]
        );
        assert_eq!(bytes[BLOCK_SIZE + 6], LAST);
        assert_eq!(read_records(&bytes).unwrap(), vec![first, b"xy".to_vec()]);

        // Leaves 3 bytes: they become a zero trailer.
        let mut w = LogWriter::default();
        let n = w.write(&vec![2u8; BLOCK_SIZE - HEADER_LEN - 3]);
        assert_eq!(n, BLOCK_SIZE);
        w.write(b"q");
        let bytes = w.into_bytes();
        assert_eq!(bytes[BLOCK_SIZE - 3..BLOCK_SIZE], [0, 0, 0]);
        assert_eq!(bytes.len(), BLOCK_SIZE + HEADER_LEN + 1);
        assert_eq!(read_records(&bytes).unwrap().len(), 2);
    }

    #[test]
    fn reader_errors() {
        let good = log(&[b"hello"]);
        assert_eq!(
            read_records(&good[..3]),
            Err("Record header length not valid".into())
        );
        assert_eq!(read_records(&good[..9]), Err("Invalid record".into()));
        let mut bad = good.clone();
        bad[8] ^= 1;
        assert_eq!(read_records(&bad), Err("Checksum doesn't validate.".into()));
        let mut long = good.clone();
        long[4..6].copy_from_slice(&40_000u16.to_le_bytes());
        assert_eq!(
            read_records(&long),
            Err("Record length is too large:40000".into())
        );
        assert_eq!(
            read_records(&header(MIDDLE, b"x")),
            Err("Invalid RecordType: MIDDLE".into())
        );
        let twice = [header(FIRST, b"a"), header(FULL, b"b")].concat();
        assert_eq!(read_records(&twice), Err("Invalid RecordType: FULL".into()));
        assert_eq!(
            read_records(&header(9, b"x")),
            Err("Checksum doesn't validate.".into())
        );
        assert_eq!(
            read_records(&header(0xff, b"x")),
            Err("Invalid RecordType: -1".into())
        );
        assert_eq!(read_records(b"not an export"), Err("Invalid record".into()));
    }

    #[test]
    fn reader_edge_cases() {
        // A cut-off fragmented record is dropped silently.
        assert_eq!(read_records(&header(FIRST, b"a")), Ok(vec![]));
        assert_eq!(read_records(b""), Ok(vec![]));
        // A NONE record skips the zeros up to the block end and resets the fragment state.
        let mut bytes = header(FIRST, b"a");
        bytes.resize(BLOCK_SIZE, 0);
        bytes.extend(header(FULL, b"b"));
        assert_eq!(read_records(&bytes), Ok(vec![b"b".to_vec()]));
        let mut bytes = vec![0u8; 20];
        bytes[10] = 5;
        assert_eq!(
            read_records(&bytes),
            Err("Found a non-zero byte: 5 before the end of the block 3 bytes after encountering a RecordType of NONE".into())
        );
        // A short trailer at the end of the file is fine.
        let mut bytes = header(FULL, &vec![0u8; BLOCK_SIZE - HEADER_LEN - 3]);
        bytes.push(0);
        assert_eq!(read_records(&bytes).unwrap().len(), 1);
    }

    #[test]
    fn specs_are_namespace_major_with_all_fields() {
        let specs = entity_set_specs(&["A".into(), "B".into()], &["".into(), "n".into()]);
        let names: Vec<_> = specs.iter().map(|s| s.dir_names()).collect();
        assert_eq!(
            names,
            [
                ("default_namespace".into(), "kind_A".into()),
                ("default_namespace".into(), "kind_B".into()),
                ("namespace_n".into(), "kind_A".into()),
                ("namespace_n".into(), "kind_B".into()),
            ]
        );
        assert_eq!(specs[2].namespace_filter(), Some("n"));
        assert_eq!(specs[0].namespace_filter(), Some(""));
        assert_eq!(specs[1].kind_filter(), Some("B"));
        let all = entity_set_specs(&[], &[]);
        assert_eq!(all[0].encode_to_vec(), [0x08, 2, 0x12, 0, 0x18, 3, 0x22, 0]);
        assert_eq!(
            (all[0].kind_filter(), all[0].namespace_filter()),
            (None, None)
        );
    }

    #[test]
    fn names_use_utc_milliseconds() {
        assert_eq!(
            export_name(1_761_582_733_769_999),
            "2025-10-27T16:32:13_769"
        );
        assert_eq!(export_name(0), "1970-01-01T00:00:00_000");
        assert_eq!(export_name(-1), "1969-12-31T23:59:59_999");
    }

    #[test]
    fn export_round_trip_and_filters() {
        let dir = std::env::temp_dir().join(format!("oxide-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let specs = entity_set_specs(&["K".into(), "L".into()], &[]);
        let entity = |n: u8| {
            oxide_proto::storage_onestore_v3::EntityProto {
                kind: Some(i32::from(n)),
                ..Default::default()
            }
            .encode_to_vec()
        };
        let sets = vec![
            (specs[0].clone(), vec![entity(1), entity(2)]),
            (specs[1].clone(), vec![entity(3)]),
        ];
        write_export(&dir, "x", &sets).unwrap();
        let meta = dir.join("x/x.overall_export_metadata");
        let all = entity_set_specs(&[], &[]);
        assert_eq!(
            read_export(&meta, &all).unwrap(),
            vec![entity(1), entity(2), entity(3)]
        );
        assert_eq!(read_export(&meta, &specs[1..]).unwrap(), vec![entity(3)]);
        let unavailable = Err(ImportError::Invalid(
            "The requested kinds/namespaces are not available".into(),
        ));
        assert_eq!(
            read_export(&meta, &entity_set_specs(&["M".into()], &[])),
            unavailable
        );
        let twice = entity_set_specs(&["K".into(), "K".into()], &[]);
        assert_eq!(read_export(&meta, &twice), unavailable);
        assert_eq!(
            read_export(&dir.join("x"), &all),
            Err(ImportError::Invalid(
                "Failed to parse overall export metadata file".into()
            ))
        );
        std::fs::remove_file(dir.join("x/all_namespaces/kind_L/output-0")).unwrap();
        assert_eq!(
            read_export(&meta, &all),
            Err(ImportError::Invalid(format!(
                "Failed parse entity file:{}",
                dir.join("x/all_namespaces/kind_L/output-0").display()
            )))
        );
        let only_version = log(&[b"3"]);
        std::fs::write(&meta, only_version).unwrap();
        assert_eq!(read_export(&meta, &all), Err(ImportError::Unknown));
        std::fs::write(&meta, log(&[b"2", b""])).unwrap();
        assert_eq!(
            read_export(&meta, &all),
            Err(ImportError::Invalid(
                "Overall export metadata file verion not supported".into()
            ))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
