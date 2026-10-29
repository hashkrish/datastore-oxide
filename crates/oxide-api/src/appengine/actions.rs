//! Transactional tasks: `AddActions` takes a taskqueue `TaskQueueBulkAddRequest`, and Commit
//! delivers each task as a taskqueue `Add` call to the CallHandler at the request's
//! `txn_add_task_callback_hostport`.
//!
//! The taskqueue messages are handled as raw fields (they are not among the generated protos):
//! `TaskQueueBulkAddRequest { repeated TaskQueueAddRequest add_request = 1; }`, and of
//! `TaskQueueAddRequest` the required `queue_name` = 1, `task_name` = 2, `eta_usec` = 3, plus
//! `transaction` = 10, `app_id` = 11 and `datastore_transaction` = 21.

use oxide_proto::apphosting::tools::devappserver2 as das;

use super::Fail;

const QUEUE_NAME: u32 = 1;
const TASK_NAME: u32 = 2;
const ETA_USEC: u32 = 3;
const TRANSACTION: u32 = 10;
const APP_ID: u32 = 11;
const DATASTORE_TRANSACTION: u32 = 21;

/// protobuf-java's message for input that ends inside a field.
const TRUNCATED: &str = "While parsing a protocol message, the input ended unexpectedly in the middle of a field.  This could mean either that the input has been truncated or that an embedded message misreported its own length.";

pub struct BulkAdd {
    pub tasks: Vec<Task>,
    pub transaction: Option<TaskTransaction>,
}

/// A `TaskQueueAddRequest` as its fields: number and raw encoding (tag included), in wire order.
#[derive(Clone, Debug, Default)]
pub struct Task {
    fields: Vec<(u32, Vec<u8>)>,
}

/// The transaction of the first task: `datastore_transaction` bytes or a `transaction` message,
/// both a serialized v3 `Transaction`.
pub enum TaskTransaction {
    Message(Vec<u8>),
    Bytes(Vec<u8>),
}

fn parse_error(message: &str) -> Fail {
    Fail::Rpc(
        das::rpc_error::ErrorCode::ParseError as i32,
        message.to_string(),
    )
}

/// A raw protobuf field: number, wire type, its whole encoding and (if length-delimited) its
/// payload.
struct Field<'a> {
    number: u32,
    raw: &'a [u8],
    payload: Option<&'a [u8]>,
}

/// Splits a message into its fields, with protobuf-java's error texts.
fn fields(mut buf: &[u8]) -> Result<Vec<Field<'_>>, Fail> {
    let varint = |b: &mut &[u8]| -> Result<u64, Fail> {
        if b.iter().take(10).all(|byte| byte & 0x80 != 0) {
            return Err(parse_error(if b.len() < 10 {
                TRUNCATED
            } else {
                "CodedInputStream encountered a malformed varint."
            }));
        }
        prost::encoding::decode_varint(b).map_err(|_| parse_error(TRUNCATED))
    };
    fn skip_group(
        buf: &mut &[u8],
        number: u32,
        varint: &dyn Fn(&mut &[u8]) -> Result<u64, Fail>,
    ) -> Result<(), Fail> {
        loop {
            if buf.is_empty() {
                return Err(parse_error(TRUNCATED));
            }
            let key = varint(buf)?;
            let (n, wire) = ((key >> 3) as u32, key & 7);
            if key >> 3 == 0 {
                return Err(parse_error(
                    "Protocol message contained an invalid tag (zero).",
                ));
            }
            match wire {
                0 => {
                    varint(buf)?;
                }
                1 | 5 => {
                    let n = if wire == 1 { 8 } else { 4 };
                    if buf.len() < n {
                        return Err(parse_error(TRUNCATED));
                    }
                    *buf = &buf[n..];
                }
                2 => {
                    let len = varint(buf)? as usize;
                    if buf.len() < len {
                        return Err(parse_error(TRUNCATED));
                    }
                    *buf = &buf[len..];
                }
                3 => skip_group(buf, n, varint)?,
                4 if n == number => return Ok(()),
                4 => {
                    return Err(parse_error(
                        "Protocol message end-group tag did not match expected tag.",
                    ));
                }
                _ => return Err(parse_error("Protocol message tag had invalid wire type.")),
            }
        }
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        let start = buf;
        let key = varint(&mut buf)?;
        let (number, wire) = ((key >> 3) as u32, key & 7);
        if number == 0 {
            return Err(parse_error(
                "Protocol message contained an invalid tag (zero).",
            ));
        }
        let mut payload = None;
        match wire {
            0 => {
                varint(&mut buf)?;
            }
            1 | 5 => {
                let n = if wire == 1 { 8 } else { 4 };
                if buf.len() < n {
                    return Err(parse_error(TRUNCATED));
                }
                buf = &buf[n..];
            }
            2 => {
                let len = varint(&mut buf)? as usize;
                if buf.len() < len {
                    return Err(parse_error(TRUNCATED));
                }
                payload = Some(&buf[..len]);
                buf = &buf[len..];
            }
            3 => skip_group(&mut buf, number, &varint)?,
            4 => {
                return Err(parse_error(
                    "Protocol message end-group tag did not match expected tag.",
                ));
            }
            _ => return Err(parse_error("Protocol message tag had invalid wire type.")),
        }
        out.push(Field {
            number,
            raw: &start[..start.len() - buf.len()],
            payload,
        });
    }
    Ok(out)
}

/// Parses a `TaskQueueBulkAddRequest`. A task without its required fields is a bare UNKNOWN.
pub fn parse(bytes: &[u8]) -> Result<BulkAdd, Fail> {
    let mut bulk = BulkAdd {
        tasks: Vec::new(),
        transaction: None,
    };
    for f in fields(bytes)? {
        let Some(payload) = f.payload.filter(|_| f.number == 1) else {
            continue;
        };
        let task_fields = fields(payload)?;
        if [QUEUE_NAME, TASK_NAME, ETA_USEC]
            .iter()
            .any(|n| !task_fields.iter().any(|f| f.number == *n))
        {
            return Err(Fail::Unknown);
        }
        if bulk.tasks.is_empty() {
            let field = |n: u32| {
                task_fields
                    .iter()
                    .rev()
                    .find(|f| f.number == n)
                    .and_then(|f| f.payload)
            };
            bulk.transaction = match (field(DATASTORE_TRANSACTION), field(TRANSACTION)) {
                (Some(b), _) => Some(TaskTransaction::Bytes(b.to_vec())),
                (None, Some(m)) => Some(TaskTransaction::Message(m.to_vec())),
                (None, None) => None,
            };
        }
        bulk.tasks.push(Task {
            fields: task_fields
                .into_iter()
                .filter(|f| f.number != TRANSACTION && f.number != DATASTORE_TRANSACTION)
                .map(|f| (f.number, f.raw.to_vec()))
                .collect(),
        });
    }
    Ok(bulk)
}

impl Task {
    pub fn app_id(&self) -> Option<Vec<u8>> {
        self.fields
            .iter()
            .rev()
            .find(|(n, _)| *n == APP_ID)
            .and_then(|(_, raw)| fields(raw).ok()?.pop()?.payload.map(<[u8]>::to_vec))
    }

    pub fn set_app_id(&mut self, app: &[u8]) {
        let mut raw = Vec::new();
        prost::encoding::encode_key(APP_ID, prost::encoding::WireType::LengthDelimited, &mut raw);
        prost::encoding::encode_varint(app.len() as u64, &mut raw);
        raw.extend_from_slice(app);
        match self.fields.iter_mut().find(|(n, _)| *n == APP_ID) {
            Some(slot) => slot.1 = raw,
            None => self.fields.push((APP_ID, raw)),
        }
        let mut seen = false;
        self.fields.retain(|(n, _)| {
            let keep = *n != APP_ID || !seen;
            seen |= *n == APP_ID;
            keep
        });
    }

    pub fn encode(&self) -> Vec<u8> {
        self.fields
            .iter()
            .flat_map(|(_, raw)| raw.iter().copied())
            .collect()
    }

    /// A task encoded by [`Task::encode`].
    pub fn decode(bytes: &[u8]) -> Task {
        Task {
            fields: fields(bytes)
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f.number, f.raw.to_vec()))
                .collect(),
        }
    }
}

/// Sends each task as a taskqueue `Add` call to the CallHandler at `hostport`. An error reply is
/// only logged; a failed call fails (the commit has already been applied).
pub async fn deliver(hostport: &str, tasks: Vec<Vec<u8>>) -> Result<(), Fail> {
    let mut client =
        das::call_handler_client::CallHandlerClient::connect(format!("http://{hostport}"))
            .await
            .map_err(|_| Fail::Unknown)?;
    for task in tasks {
        let reply = client
            .handle_call(das::Request {
                service_name: "taskqueue".into(),
                method: "Add".into(),
                request: task.into(),
                ..Default::default()
            })
            .await
            .map_err(|_| Fail::Unknown)?
            .into_inner();
        if let Some(e) = reply.application_error {
            tracing::warn!(
                "Transactional task delivery failed: {} {}",
                e.code,
                e.detail
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(number: u32, value: &[u8]) -> Vec<u8> {
        let mut out = vec![(number << 3 | 2) as u8, value.len() as u8];
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn keeps_tasks_without_their_transactions() {
        let mut task = field(1, b"default");
        task.extend(field(2, b"t1"));
        task.extend([3 << 3, 123]);
        task.extend(field(10, b"\x11\x01\x00\x00\x00\x00\x00\x00\x00"));
        task.extend(field(11, b"s~p"));
        let bulk = parse(&field(1, &task)).unwrap();
        assert!(matches!(
            bulk.transaction,
            Some(TaskTransaction::Message(_))
        ));
        let mut t = Task::decode(&bulk.tasks[0].encode());
        assert_eq!(t.app_id().as_deref(), Some(&b"s~p"[..]));
        t.set_app_id(b"p");
        let mut expected = field(1, b"default");
        expected.extend(field(2, b"t1"));
        expected.extend([3 << 3, 123]);
        expected.extend(field(11, b"p"));
        assert_eq!(t.encode(), expected);
    }

    #[test]
    fn reports_truncation_and_missing_fields() {
        assert!(matches!(parse(&[0x0a, 0x05, 0x0a]), Err(Fail::Rpc(2, m)) if m == TRUNCATED));
        assert!(matches!(
            parse(&field(1, &field(1, b"q"))),
            Err(Fail::Unknown)
        ));
    }
}
