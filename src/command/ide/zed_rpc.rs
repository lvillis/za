use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

const ZED_RPC_TIMEOUT: Duration = Duration::from_millis(180);
const ZED_RPC_MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;
const ENVELOPE_ID_FIELD: u64 = 1;
const ENVELOPE_RESPONDING_TO_FIELD: u64 = 2;
const ENVELOPE_ACK_FIELD: u64 = 5;
const ENVELOPE_PING_FIELD: u64 = 7;
const ENVELOPE_UPDATE_PROJECT_FIELD: u64 = 44;
const ENVELOPE_FLUSH_BUFFERED_MESSAGES_FIELD: u64 = 267;
const ENVELOPE_REMOTE_STARTED_FIELD: u64 = 381;
const REMOTE_SERVER_PROJECT_ID: u64 = 0;

#[derive(Debug, Clone, Default)]
pub(super) struct ZedRpcProbe {
    pub responsive: bool,
    pub projects: Vec<String>,
}

#[derive(Debug)]
struct Envelope {
    id: u32,
    responding_to: Option<u32>,
    payload_field: Option<u64>,
    payload: Vec<u8>,
}

pub(super) fn probe(
    stdin_socket: Option<&Path>,
    stdout_socket: Option<&Path>,
    stderr_socket: Option<&Path>,
) -> ZedRpcProbe {
    probe_impl(stdin_socket, stdout_socket, stderr_socket).unwrap_or_default()
}

fn probe_impl(
    stdin_socket: Option<&Path>,
    stdout_socket: Option<&Path>,
    stderr_socket: Option<&Path>,
) -> Option<ZedRpcProbe> {
    let stdin_socket = stdin_socket?;
    let stdout_socket = stdout_socket?;
    let stderr_socket = stderr_socket?;

    let mut stdin = UnixStream::connect(stdin_socket).ok()?;
    let mut stdout = UnixStream::connect(stdout_socket).ok()?;
    let _stderr = UnixStream::connect(stderr_socket).ok()?;
    configure_stream(&stdin);
    configure_stream(&stdout);

    write_envelope(&mut stdin, ENVELOPE_FLUSH_BUFFERED_MESSAGES_FIELD, 1, None).ok()?;
    write_envelope(&mut stdin, ENVELOPE_PING_FIELD, 2, None).ok()?;
    let mut next_id = 3;
    let deadline = Instant::now() + ZED_RPC_TIMEOUT;
    let mut probe = ZedRpcProbe::default();

    while Instant::now() < deadline {
        let envelope = match read_envelope(&mut stdout) {
            Ok(envelope) => envelope,
            Err(_) => break,
        };
        probe.responsive = true;
        match envelope.payload_field {
            Some(ENVELOPE_ACK_FIELD)
                if envelope.responding_to == Some(2) && !probe.projects.is_empty() =>
            {
                break;
            }
            Some(ENVELOPE_ACK_FIELD) if envelope.responding_to == Some(2) => {}
            Some(ENVELOPE_REMOTE_STARTED_FIELD) => {
                let _ = write_envelope(&mut stdin, ENVELOPE_ACK_FIELD, next_id, Some(envelope.id));
                next_id += 1;
            }
            Some(ENVELOPE_UPDATE_PROJECT_FIELD) => {
                merge_projects(
                    &mut probe.projects,
                    parse_update_project_paths(&envelope.payload),
                );
            }
            _ => {}
        }
    }

    Some(probe)
}

fn configure_stream(stream: &UnixStream) {
    let _ = stream.set_read_timeout(Some(ZED_RPC_TIMEOUT));
    let _ = stream.set_write_timeout(Some(ZED_RPC_TIMEOUT));
}

fn write_envelope(
    stream: &mut UnixStream,
    payload_field: u64,
    id: u32,
    responding_to: Option<u32>,
) -> std::io::Result<()> {
    let mut body = Vec::new();
    encode_varint_field(&mut body, ENVELOPE_ID_FIELD, u64::from(id));
    if let Some(request_id) = responding_to {
        encode_varint_field(
            &mut body,
            ENVELOPE_RESPONDING_TO_FIELD,
            u64::from(request_id),
        );
    }
    encode_len_delimited_field(&mut body, payload_field, &[]);
    stream.write_all(&(body.len() as u32).to_le_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

fn read_envelope(stream: &mut UnixStream) -> std::io::Result<Envelope> {
    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > ZED_RPC_MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zed rpc frame is too large",
        ));
    }
    let mut body = vec![0_u8; len as usize];
    stream.read_exact(&mut body)?;
    parse_envelope(&body).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid zed rpc envelope")
    })
}

fn parse_envelope(bytes: &[u8]) -> Option<Envelope> {
    let mut cursor = 0;
    let mut envelope = Envelope {
        id: 0,
        responding_to: None,
        payload_field: None,
        payload: Vec::new(),
    };
    while cursor < bytes.len() {
        let key = decode_varint(bytes, &mut cursor)?;
        let field = key >> 3;
        let wire = key & 0b111;
        match (field, wire) {
            (ENVELOPE_ID_FIELD, 0) => {
                envelope.id = u32::try_from(decode_varint(bytes, &mut cursor)?).ok()?
            }
            (ENVELOPE_RESPONDING_TO_FIELD, 0) => {
                envelope.responding_to =
                    Some(u32::try_from(decode_varint(bytes, &mut cursor)?).ok()?);
            }
            (_, 2) => {
                let len = usize::try_from(decode_varint(bytes, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let payload = bytes.get(cursor..end)?;
                cursor = end;
                if is_zed_envelope_payload(field) {
                    envelope.payload_field = Some(field);
                    envelope.payload = payload.to_vec();
                }
            }
            (_, 0) => {
                let _ = decode_varint(bytes, &mut cursor)?;
            }
            (_, 1) => cursor = cursor.checked_add(8)?,
            (_, 5) => cursor = cursor.checked_add(4)?,
            _ => return None,
        }
    }
    Some(envelope)
}

fn is_zed_envelope_payload(field: u64) -> bool {
    matches!(
        field,
        ENVELOPE_ACK_FIELD
            | ENVELOPE_PING_FIELD
            | ENVELOPE_UPDATE_PROJECT_FIELD
            | ENVELOPE_FLUSH_BUFFERED_MESSAGES_FIELD
            | ENVELOPE_REMOTE_STARTED_FIELD
    )
}

fn parse_update_project_paths(bytes: &[u8]) -> Vec<String> {
    let mut cursor = 0;
    let mut project_id = None;
    let mut paths = Vec::new();
    while cursor < bytes.len() {
        let Some(key) = decode_varint(bytes, &mut cursor) else {
            break;
        };
        let field = key >> 3;
        let wire = key & 0b111;
        match (field, wire) {
            (1, 0) => project_id = decode_varint(bytes, &mut cursor),
            (2, 2) => {
                let Some(len) =
                    decode_varint(bytes, &mut cursor).and_then(|v| usize::try_from(v).ok())
                else {
                    break;
                };
                let Some(end) = cursor.checked_add(len) else {
                    break;
                };
                let Some(worktree) = bytes.get(cursor..end) else {
                    break;
                };
                cursor = end;
                if let Some(path) = parse_worktree_abs_path(worktree) {
                    paths.push(path);
                }
            }
            (_, 0) => {
                let _ = decode_varint(bytes, &mut cursor);
            }
            (_, 2) => {
                let Some(len) =
                    decode_varint(bytes, &mut cursor).and_then(|v| usize::try_from(v).ok())
                else {
                    break;
                };
                let Some(end) = cursor.checked_add(len) else {
                    break;
                };
                cursor = end;
            }
            (_, 1) => cursor = cursor.saturating_add(8),
            (_, 5) => cursor = cursor.saturating_add(4),
            _ => break,
        }
    }
    if project_id != Some(REMOTE_SERVER_PROJECT_ID) {
        return Vec::new();
    }
    paths
}

fn parse_worktree_abs_path(bytes: &[u8]) -> Option<String> {
    let mut cursor = 0;
    while cursor < bytes.len() {
        let key = decode_varint(bytes, &mut cursor)?;
        let field = key >> 3;
        let wire = key & 0b111;
        match (field, wire) {
            (4, 2) => {
                let len = usize::try_from(decode_varint(bytes, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let raw = bytes.get(cursor..end)?;
                return std::str::from_utf8(raw)
                    .ok()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string);
            }
            (_, 0) => {
                let _ = decode_varint(bytes, &mut cursor)?;
            }
            (_, 2) => {
                let len = usize::try_from(decode_varint(bytes, &mut cursor)?).ok()?;
                cursor = cursor.checked_add(len)?;
            }
            (_, 1) => cursor = cursor.checked_add(8)?,
            (_, 5) => cursor = cursor.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn merge_projects(target: &mut Vec<String>, incoming: Vec<String>) {
    for path in incoming {
        if !target.iter().any(|existing| existing == &path) {
            target.push(path);
        }
    }
}

fn encode_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    encode_varint(out, field << 3);
    encode_varint(out, value);
}

fn encode_len_delimited_field(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    encode_varint(out, (field << 3) | 2);
    encode_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn encode_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn decode_varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    while *cursor < bytes.len() && shift < 64 {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_envelope_reads_ping_ack() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, ENVELOPE_ID_FIELD, 9);
        encode_varint_field(&mut bytes, ENVELOPE_RESPONDING_TO_FIELD, 2);
        encode_len_delimited_field(&mut bytes, ENVELOPE_ACK_FIELD, &[]);

        let envelope = parse_envelope(&bytes).expect("envelope");

        assert_eq!(envelope.id, 9);
        assert_eq!(envelope.responding_to, Some(2));
        assert_eq!(envelope.payload_field, Some(ENVELOPE_ACK_FIELD));
    }

    #[test]
    fn parse_update_project_paths_extracts_remote_worktree_paths() {
        let mut worktree = Vec::new();
        encode_varint_field(&mut worktree, 1, 7);
        encode_len_delimited_field(&mut worktree, 4, b"/opt/app/za");

        let mut update = Vec::new();
        encode_varint_field(&mut update, 1, REMOTE_SERVER_PROJECT_ID);
        encode_len_delimited_field(&mut update, 2, &worktree);

        assert_eq!(parse_update_project_paths(&update), vec!["/opt/app/za"]);
    }

    #[test]
    fn parse_update_project_paths_ignores_non_remote_project_id() {
        let mut worktree = Vec::new();
        encode_len_delimited_field(&mut worktree, 4, b"/opt/app/za");

        let mut update = Vec::new();
        encode_varint_field(&mut update, 1, 99);
        encode_len_delimited_field(&mut update, 2, &worktree);

        assert!(parse_update_project_paths(&update).is_empty());
    }

    #[test]
    fn write_envelope_uses_zed_length_prefixed_envelope_shape() {
        let mut body = Vec::new();
        encode_varint_field(&mut body, ENVELOPE_ID_FIELD, 1);
        encode_len_delimited_field(&mut body, ENVELOPE_PING_FIELD, &[]);

        let mut expected = Vec::new();
        expected.extend_from_slice(&(body.len() as u32).to_le_bytes());
        expected.extend_from_slice(&body);

        assert_eq!(expected[0..4], [4, 0, 0, 0]);
        assert_eq!(body, [8, 1, 58, 0]);
    }
}
