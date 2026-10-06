//! Eon-owned values and bounded EONW v2 codec for local workspace clients.

use std::{collections::HashSet, fmt, str};

pub const VERSION: u16 = 2;
pub const HEADER_BYTES: usize = 12;
const MAGIC: &[u8; 4] = b"EONW";
const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_ID_BYTES: usize = 128;
pub(crate) const MAX_ENDPOINT_BYTES: usize = 4096;
const MAX_VERSION_BYTES: usize = 128;
const MAX_COMPONENT_REPORT_BYTES: usize = 32 * 1024;
pub const MAX_DETAIL_BYTES: usize = 1024;
pub const MAX_TABS: usize = 64;
pub const MAX_PANES: usize = 256;
pub const MAX_DIRECTORY_BYTES: usize = 4096;

pub(crate) const REQUEST: u8 = 1;
pub(crate) const SNAPSHOT: u8 = 129;
pub(crate) const FAILURE: u8 = 130;
const RUNTIME: u8 = 131;
const STOPPED: u8 = 132;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Inspect,
    InspectRuntime,
    InspectPresentation,
    Present { workspace: bool },
    CreateTab,
    CreatePane,
    FocusId(String),
    Focus(Direction),
    Stop { generation: String },
    SetTabDirectory { tab: String, directory: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: String,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    pub session: String,
    pub endpoint: Vec<u8>,
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: String,
    pub directory: Vec<u8>,
    pub selected_pane: String,
    pub panes: Vec<Pane>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub active_tab: String,
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Snapshot(Snapshot),
    Failure(Failure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    pub available: bool,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    pub generation: String,
    pub eon_version: String,
    pub workspace_protocol: u16,
    pub component_report: String,
    pub sessions: Vec<String>,
    pub attach: Availability,
    pub stop: Availability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stopped {
    pub generation: String,
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleResponse {
    Runtime(Runtime),
    Stopped(Stopped),
    Failure(Failure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    InvalidMagic,
    UnsupportedVersion { version: u16 },
    InvalidFlags { value: u8 },
    MessageTooLarge { size: usize },
    Truncated,
    TrailingBytes { count: usize },
    InvalidKind { value: u8 },
    InvalidTag { field: &'static str, value: u8 },
    InvalidUtf8 { field: &'static str },
    InvalidValue { field: &'static str },
    FieldTooLong { field: &'static str, length: usize },
    InvalidSnapshot { field: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => formatter.write_str("invalid EONW message magic"),
            Self::UnsupportedVersion { version } => {
                write!(formatter, "unsupported EONW version {version}")
            }
            Self::InvalidFlags { value } => write!(formatter, "invalid EONW flags {value:#x}"),
            Self::MessageTooLarge { size } => write!(
                formatter,
                "EONW message is {size} bytes; maximum is {MAX_MESSAGE_BYTES}"
            ),
            Self::Truncated => formatter.write_str("truncated EONW message"),
            Self::TrailingBytes { count } => {
                write!(formatter, "EONW message has {count} trailing bytes")
            }
            Self::InvalidKind { value } => write!(formatter, "invalid EONW message kind {value}"),
            Self::InvalidTag { field, value } => {
                write!(formatter, "invalid EONW {field} tag {value}")
            }
            Self::InvalidUtf8 { field } => write!(formatter, "EONW {field} is not UTF-8"),
            Self::InvalidValue { field } => write!(formatter, "invalid EONW {field}"),
            Self::FieldTooLong { field, length } => {
                write!(formatter, "EONW {field} is too long: {length} bytes")
            }
            Self::InvalidSnapshot { field } => {
                write!(formatter, "invalid EONW snapshot {field}")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    identity("request id", &request.id)?;
    let mut payload = Encoder::default();
    payload.string(&request.id);
    match &request.action {
        Action::Inspect => payload.byte(0),
        Action::CreateTab => payload.byte(1),
        Action::CreatePane => payload.byte(2),
        Action::FocusId(id) => {
            nonempty("focus id", id, MAX_ID_BYTES)?;
            payload.byte(3);
            payload.string(id);
        }
        Action::Focus(Direction::Left) => payload.byte(4),
        Action::Focus(Direction::Right) => payload.byte(5),
        Action::Focus(Direction::Up) => payload.byte(6),
        Action::Focus(Direction::Down) => payload.byte(7),
        Action::InspectRuntime => payload.byte(8),
        Action::Stop { generation } => {
            identity("generation", generation)?;
            payload.byte(9);
            payload.string(generation);
        }
        Action::Present { workspace } => {
            payload.byte(10);
            payload.byte(u8::from(*workspace));
        }
        Action::InspectPresentation => payload.byte(11),
        Action::SetTabDirectory { tab, directory } => {
            identity("tab id", tab)?;
            validate_directory(directory)?;
            payload.byte(12);
            payload.string(tab);
            payload.raw(directory);
        }
    }
    frame(REQUEST, payload.bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    let (kind, payload) = unframe(bytes)?;
    if kind != REQUEST {
        return Err(Error::InvalidKind { value: kind });
    }
    let mut decoder = Decoder::new(payload);
    let id = decoder.string("request id", MAX_ID_BYTES)?;
    identity("request id", &id)?;
    let action = match decoder.byte()? {
        0 => Action::Inspect,
        1 => Action::CreateTab,
        2 => Action::CreatePane,
        3 => {
            let id = decoder.string("focus id", MAX_ID_BYTES)?;
            nonempty("focus id", &id, MAX_ID_BYTES)?;
            Action::FocusId(id)
        }
        4 => Action::Focus(Direction::Left),
        5 => Action::Focus(Direction::Right),
        6 => Action::Focus(Direction::Up),
        7 => Action::Focus(Direction::Down),
        8 => Action::InspectRuntime,
        9 => {
            let generation = decoder.string("generation", MAX_ID_BYTES)?;
            identity("generation", &generation)?;
            Action::Stop { generation }
        }
        10 => Action::Present {
            workspace: match decoder.byte()? {
                0 => false,
                1 => true,
                _ => {
                    return Err(Error::InvalidValue {
                        field: "presentation mode",
                    });
                }
            },
        },
        11 => Action::InspectPresentation,
        12 => {
            let tab = decoder.string("tab id", MAX_ID_BYTES)?;
            identity("tab id", &tab)?;
            let directory = decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec();
            validate_directory(&directory)?;
            Action::SetTabDirectory { tab, directory }
        }
        value => {
            return Err(Error::InvalidTag {
                field: "action",
                value,
            });
        }
    };
    decoder.finish()?;
    Ok(Request { id, action })
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>> {
    let mut payload = Encoder::default();
    let kind = match response {
        Response::Snapshot(snapshot) => {
            validate_snapshot(snapshot)?;
            encode_workspace(&mut payload, &snapshot.active_tab, &snapshot.tabs);
            SNAPSHOT
        }
        Response::Failure(failure) => {
            encode_failure(&mut payload, failure)?;
            FAILURE
        }
    };
    frame(kind, payload.bytes)
}

pub fn decode_response(bytes: &[u8]) -> Result<Response> {
    let (kind, payload) = unframe(bytes)?;
    let mut decoder = Decoder::new(payload);
    let response = match kind {
        SNAPSHOT => {
            let (active_tab, tabs) = decode_workspace(&mut decoder)?;
            let snapshot = Snapshot { active_tab, tabs };
            validate_snapshot(&snapshot)?;
            Response::Snapshot(snapshot)
        }
        FAILURE => Response::Failure(decode_failure(&mut decoder)?),
        value => return Err(Error::InvalidKind { value }),
    };
    decoder.finish()?;
    Ok(response)
}

pub fn encode_lifecycle_response(response: &LifecycleResponse) -> Result<Vec<u8>> {
    encode_lifecycle_response_version(response, VERSION, false)
}

pub(crate) fn encode_lifecycle_response_version(
    response: &LifecycleResponse,
    version: u16,
    allow_empty_sessions: bool,
) -> Result<Vec<u8>> {
    let mut payload = Encoder::default();
    let kind = match response {
        LifecycleResponse::Runtime(runtime) => {
            validate_runtime(runtime, allow_empty_sessions)?;
            payload.string(&runtime.generation);
            payload.string(&runtime.eon_version);
            payload.number(runtime.workspace_protocol);
            payload.string(&runtime.component_report);
            encode_sessions(&mut payload, &runtime.sessions);
            encode_availability(&mut payload, &runtime.attach);
            encode_availability(&mut payload, &runtime.stop);
            RUNTIME
        }
        LifecycleResponse::Stopped(stopped) => {
            validate_stopped(stopped, allow_empty_sessions)?;
            payload.string(&stopped.generation);
            encode_sessions(&mut payload, &stopped.sessions);
            STOPPED
        }
        LifecycleResponse::Failure(failure) => {
            encode_failure(&mut payload, failure)?;
            FAILURE
        }
    };
    frame_version(version, kind, payload.bytes)
}

pub fn decode_lifecycle_response(bytes: &[u8]) -> Result<LifecycleResponse> {
    decode_lifecycle_response_version(bytes, VERSION, false)
}

pub(crate) fn decode_lifecycle_response_version(
    bytes: &[u8],
    version: u16,
    allow_empty_sessions: bool,
) -> Result<LifecycleResponse> {
    let (kind, payload) = unframe_version(bytes, version)?;
    let mut decoder = Decoder::new(payload);
    let response = match kind {
        RUNTIME => {
            let runtime = Runtime {
                generation: decoder.string("generation", MAX_ID_BYTES)?,
                eon_version: decoder.string("Eon version", MAX_VERSION_BYTES)?,
                workspace_protocol: decoder.number()?,
                component_report: decoder.string("component report", MAX_COMPONENT_REPORT_BYTES)?,
                sessions: decode_sessions(&mut decoder)?,
                attach: decode_availability(&mut decoder, "attach reason")?,
                stop: decode_availability(&mut decoder, "stop reason")?,
            };
            validate_runtime(&runtime, allow_empty_sessions)?;
            LifecycleResponse::Runtime(runtime)
        }
        STOPPED => {
            let stopped = Stopped {
                generation: decoder.string("generation", MAX_ID_BYTES)?,
                sessions: decode_sessions(&mut decoder)?,
            };
            validate_stopped(&stopped, allow_empty_sessions)?;
            LifecycleResponse::Stopped(stopped)
        }
        FAILURE => LifecycleResponse::Failure(decode_failure(&mut decoder)?),
        value => return Err(Error::InvalidKind { value }),
    };
    decoder.finish()?;
    Ok(response)
}

/// Returns the bounded length declared by a complete EONW header.
pub fn declared_message_len(header: &[u8]) -> Result<usize> {
    if header.len() < HEADER_BYTES {
        return Err(Error::Truncated);
    }
    let payload_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let size = HEADER_BYTES
        .checked_add(payload_len)
        .ok_or(Error::MessageTooLarge { size: usize::MAX })?;
    if size > MAX_MESSAGE_BYTES {
        return Err(Error::MessageTooLarge { size });
    }
    Ok(size)
}

pub(crate) fn encode_workspace(encoder: &mut Encoder, active_tab: &str, tabs: &[Tab]) {
    encoder.string(active_tab);
    encoder.count(tabs.len());
    for tab in tabs {
        encoder.string(&tab.id);
        encoder.raw(&tab.directory);
        encoder.string(&tab.selected_pane);
        encoder.count(tab.panes.len());
        for pane in &tab.panes {
            encoder.string(&pane.id);
            encoder.string(&pane.session);
            encoder.raw(&pane.endpoint);
            encoder.byte(u8::from(pane.live));
        }
    }
}

pub(crate) fn decode_workspace(decoder: &mut Decoder<'_>) -> Result<(String, Vec<Tab>)> {
    let active_tab = decoder.string("active tab", MAX_ID_BYTES)?;
    let tab_count = decoder.count("tabs", MAX_TABS)?;
    let mut tabs = Vec::with_capacity(tab_count);
    let mut pane_count = 0;
    for _ in 0..tab_count {
        let id = decoder.string("tab id", MAX_ID_BYTES)?;
        let directory = decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec();
        let selected_pane = decoder.string("selected pane", MAX_ID_BYTES)?;
        let count = decoder.count("panes", MAX_PANES)?;
        pane_count += count;
        if pane_count > MAX_PANES {
            return Err(Error::InvalidSnapshot { field: "panes" });
        }
        let mut panes = Vec::with_capacity(count);
        for _ in 0..count {
            panes.push(Pane {
                id: decoder.string("pane id", MAX_ID_BYTES)?,
                session: decoder.string("session id", MAX_ID_BYTES)?,
                endpoint: decoder.raw("endpoint", MAX_ENDPOINT_BYTES)?.to_vec(),
                live: match decoder.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::InvalidValue { field: "liveness" }),
                },
            });
        }
        tabs.push(Tab {
            id,
            directory,
            selected_pane,
            panes,
        });
    }
    Ok((active_tab, tabs))
}

pub(crate) fn validate_directory(directory: &[u8]) -> Result<()> {
    if directory.is_empty() || directory.contains(&0) {
        return Err(Error::InvalidValue { field: "directory" });
    }
    if directory.len() > MAX_DIRECTORY_BYTES {
        return Err(Error::FieldTooLong {
            field: "directory",
            length: directory.len(),
        });
    }
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    validate_workspace(&snapshot.active_tab, &snapshot.tabs)
}

pub(crate) fn validate_workspace(active_tab: &str, tabs: &[Tab]) -> Result<()> {
    if tabs.is_empty() || tabs.len() > MAX_TABS {
        return Err(Error::InvalidSnapshot { field: "tabs" });
    }
    identity("active tab", active_tab)?;

    let mut focus_ids = HashSet::new();
    let mut sessions = HashSet::new();
    let mut endpoints = HashSet::new();
    let mut pane_count = 0;
    let mut active_tab_present = false;
    for tab in tabs {
        identity("tab id", &tab.id)?;
        validate_directory(&tab.directory)?;
        identity("selected pane", &tab.selected_pane)?;
        if !focus_ids.insert(tab.id.as_str()) {
            return Err(Error::InvalidSnapshot { field: "tab id" });
        }
        active_tab_present |= tab.id == active_tab;
        if tab.panes.is_empty() {
            return Err(Error::InvalidSnapshot { field: "panes" });
        }
        pane_count += tab.panes.len();
        if pane_count > MAX_PANES {
            return Err(Error::InvalidSnapshot { field: "panes" });
        }
        let mut selected = false;
        for pane in &tab.panes {
            identity("pane id", &pane.id)?;
            identity("session id", &pane.session)?;
            if !focus_ids.insert(pane.id.as_str()) {
                return Err(Error::InvalidSnapshot { field: "pane id" });
            }
            if !sessions.insert(pane.session.as_str()) {
                return Err(Error::InvalidSnapshot {
                    field: "session id",
                });
            }
            validate_endpoint(&pane.endpoint, "endpoint")?;
            if !endpoints.insert(pane.endpoint.as_slice()) {
                return Err(Error::InvalidSnapshot { field: "endpoint" });
            }
            selected |= pane.id == tab.selected_pane;
        }
        if !selected {
            return Err(Error::InvalidSnapshot {
                field: "selected_pane",
            });
        }
    }
    if !active_tab_present {
        return Err(Error::InvalidSnapshot {
            field: "active_tab",
        });
    }
    Ok(())
}

fn validate_runtime(runtime: &Runtime, allow_empty_sessions: bool) -> Result<()> {
    identity("generation", &runtime.generation)?;
    nonempty("Eon version", &runtime.eon_version, MAX_VERSION_BYTES)?;
    if runtime.workspace_protocol == 0 {
        return Err(Error::InvalidValue {
            field: "workspace protocol",
        });
    }
    nonempty(
        "component report",
        &runtime.component_report,
        MAX_COMPONENT_REPORT_BYTES,
    )?;
    validate_sessions(&runtime.sessions, allow_empty_sessions)?;
    validate_availability(&runtime.attach, "attach reason")?;
    validate_availability(&runtime.stop, "stop reason")
}

fn validate_stopped(stopped: &Stopped, allow_empty_sessions: bool) -> Result<()> {
    identity("generation", &stopped.generation)?;
    validate_sessions(&stopped.sessions, allow_empty_sessions)
}

fn validate_sessions(sessions: &[String], allow_empty: bool) -> Result<()> {
    if (!allow_empty && sessions.is_empty()) || sessions.len() > MAX_PANES {
        return Err(Error::InvalidValue { field: "sessions" });
    }
    let mut identities = HashSet::new();
    for session in sessions {
        identity("session id", session)?;
        if !identities.insert(session) {
            return Err(Error::InvalidValue { field: "sessions" });
        }
    }
    Ok(())
}

fn validate_availability(availability: &Availability, field: &'static str) -> Result<()> {
    nonempty(field, &availability.reason, MAX_DETAIL_BYTES)
}

fn encode_sessions(encoder: &mut Encoder, sessions: &[String]) {
    encoder.count(sessions.len());
    for session in sessions {
        encoder.string(session);
    }
}

fn decode_sessions(decoder: &mut Decoder<'_>) -> Result<Vec<String>> {
    let count = decoder.count("sessions", MAX_PANES)?;
    (0..count)
        .map(|_| decoder.string("session id", MAX_ID_BYTES))
        .collect()
}

fn encode_availability(encoder: &mut Encoder, availability: &Availability) {
    encoder.byte(u8::from(availability.available));
    encoder.string(&availability.reason);
}

fn decode_availability(decoder: &mut Decoder<'_>, field: &'static str) -> Result<Availability> {
    let available = match decoder.byte()? {
        0 => false,
        1 => true,
        _ => {
            return Err(Error::InvalidValue {
                field: "availability",
            });
        }
    };
    Ok(Availability {
        available,
        reason: decoder.string(field, MAX_DETAIL_BYTES)?,
    })
}

pub(crate) fn validate_endpoint(endpoint: &[u8], field: &'static str) -> Result<()> {
    if endpoint.is_empty() {
        return Err(Error::InvalidSnapshot { field });
    }
    if endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(Error::FieldTooLong {
            field,
            length: endpoint.len(),
        });
    }
    Ok(())
}

pub(crate) fn encode_failure(encoder: &mut Encoder, failure: &Failure) -> Result<()> {
    identity("failure code", &failure.code)?;
    nonempty("failure detail", &failure.detail, MAX_DETAIL_BYTES)?;
    encoder.string(&failure.code);
    encoder.string(&failure.detail);
    Ok(())
}

pub(crate) fn decode_failure(decoder: &mut Decoder<'_>) -> Result<Failure> {
    let code = decoder.string("failure code", MAX_ID_BYTES)?;
    identity("failure code", &code)?;
    let detail = decoder.string("failure detail", MAX_DETAIL_BYTES)?;
    nonempty("failure detail", &detail, MAX_DETAIL_BYTES)?;
    Ok(Failure { code, detail })
}

pub(crate) fn identity(field: &'static str, value: &str) -> Result<()> {
    nonempty(field, value, MAX_ID_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::InvalidValue { field });
    }
    Ok(())
}

pub(crate) fn nonempty(field: &'static str, value: &str, maximum: usize) -> Result<()> {
    if value.len() > maximum {
        return Err(Error::FieldTooLong {
            field,
            length: value.len(),
        });
    }
    if value.is_empty() {
        return Err(Error::InvalidValue { field });
    }
    Ok(())
}

fn frame(kind: u8, payload: Vec<u8>) -> Result<Vec<u8>> {
    frame_version(VERSION, kind, payload)
}

pub(crate) fn frame_version(version: u16, kind: u8, payload: Vec<u8>) -> Result<Vec<u8>> {
    let size = HEADER_BYTES + payload.len();
    if size > MAX_MESSAGE_BYTES {
        return Err(Error::MessageTooLarge { size });
    }
    let mut message = Vec::with_capacity(size);
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&version.to_le_bytes());
    message.push(kind);
    message.push(0);
    message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    message.extend_from_slice(&payload);
    Ok(message)
}

fn unframe(bytes: &[u8]) -> Result<(u8, &[u8])> {
    unframe_version(bytes, VERSION)
}

pub(crate) fn unframe_version(bytes: &[u8], expected_version: u16) -> Result<(u8, &[u8])> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Error::MessageTooLarge { size: bytes.len() });
    }
    if bytes.len() < HEADER_BYTES {
        return Err(Error::Truncated);
    }
    if &bytes[..4] != MAGIC {
        return Err(Error::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != expected_version {
        return Err(Error::UnsupportedVersion { version });
    }
    if bytes[7] != 0 {
        return Err(Error::InvalidFlags { value: bytes[7] });
    }
    let declared = declared_message_len(bytes)?;
    if bytes.len() < declared {
        return Err(Error::Truncated);
    }
    if bytes.len() > declared {
        return Err(Error::TrailingBytes {
            count: bytes.len() - declared,
        });
    }
    Ok((bytes[6], &bytes[HEADER_BYTES..]))
}

#[derive(Default)]
pub(crate) struct Encoder {
    pub(crate) bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn count(&mut self, value: usize) {
        let value = u16::try_from(value).expect("validated EONW count");
        self.number(value);
    }

    pub(crate) fn number(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.raw(value.as_bytes());
    }

    pub(crate) fn raw(&mut self, value: &[u8]) {
        let length = u16::try_from(value.len()).expect("validated EONW field length");
        self.bytes.extend_from_slice(&length.to_le_bytes());
        self.bytes.extend_from_slice(value);
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn count(&mut self, field: &'static str, maximum: usize) -> Result<usize> {
        let value = self.number()? as usize;
        if value > maximum {
            return Err(Error::InvalidSnapshot { field });
        }
        Ok(value)
    }

    pub(crate) fn number(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn string(&mut self, field: &'static str, maximum: usize) -> Result<String> {
        str::from_utf8(self.raw(field, maximum)?)
            .map(str::to_owned)
            .map_err(|_| Error::InvalidUtf8 { field })
    }

    pub(crate) fn raw(&mut self, field: &'static str, maximum: usize) -> Result<&'a [u8]> {
        let bytes = self.take(2)?;
        let length = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        if length > maximum {
            return Err(Error::FieldTooLong { field, length });
        }
        self.take(length)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(length).ok_or(Error::Truncated)?;
        let value = self.bytes.get(self.offset..end).ok_or(Error::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    pub(crate) fn finish(self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::TrailingBytes {
                count: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            active_tab: "t1".into(),
            tabs: vec![Tab {
                id: "t1".into(),
                directory: b"/tmp/eon-\xff".to_vec(),
                selected_pane: "p1".into(),
                panes: vec![Pane {
                    id: "p1".into(),
                    session: "session-1".into(),
                    endpoint: b"/run/eon/orbit.sock".to_vec(),
                    live: true,
                }],
            }],
        }
    }

    #[test]
    fn request_actions_have_stable_v2_wire_bytes() {
        let frame_request = |action_payload: &[u8]| {
            let mut payload = Encoder::default();
            payload.string("client-1");
            payload.bytes.extend_from_slice(action_payload);
            frame(REQUEST, payload.bytes).unwrap()
        };
        let cases: [(Action, &[u8]); 14] = [
            (Action::Inspect, &[0]),
            (Action::CreateTab, &[1]),
            (Action::CreatePane, &[2]),
            (Action::FocusId("p1".into()), &[3, 2, 0, b'p', b'1']),
            (Action::Focus(Direction::Left), &[4]),
            (Action::Focus(Direction::Right), &[5]),
            (Action::Focus(Direction::Up), &[6]),
            (Action::Focus(Direction::Down), &[7]),
            (Action::InspectRuntime, &[8]),
            (
                Action::Stop {
                    generation: "g1".into(),
                },
                &[9, 2, 0, b'g', b'1'],
            ),
            (Action::Present { workspace: false }, &[10, 0]),
            (Action::Present { workspace: true }, &[10, 1]),
            (Action::InspectPresentation, &[11]),
            (
                Action::SetTabDirectory {
                    tab: "t1".into(),
                    directory: b"/x\xff".to_vec(),
                },
                &[12, 2, 0, b't', b'1', 3, 0, b'/', b'x', 0xff],
            ),
        ];
        for (action, action_payload) in cases {
            let request = Request {
                id: "client-1".into(),
                action,
            };
            let encoded = frame_request(action_payload);
            assert_eq!(encode_request(&request).unwrap(), encoded);
            assert_eq!(decode_request(&encoded).unwrap(), request);
        }

        let malformed: [(&[u8], Error); 7] = [
            (
                &[13],
                Error::InvalidTag {
                    field: "action",
                    value: 13,
                },
            ),
            (
                &[10, 2],
                Error::InvalidValue {
                    field: "presentation mode",
                },
            ),
            (&[3, 0, 0], Error::InvalidValue { field: "focus id" }),
            (
                &[3, (MAX_ID_BYTES + 1) as u8, 0],
                Error::FieldTooLong {
                    field: "focus id",
                    length: MAX_ID_BYTES + 1,
                },
            ),
            (
                &[12, 0, 0, 1, 0, b'/'],
                Error::InvalidValue { field: "tab id" },
            ),
            (
                &[12, 2, 0, b't', b'1', 0, 0],
                Error::InvalidValue { field: "directory" },
            ),
            (&[0, 0], Error::TrailingBytes { count: 1 }),
        ];
        for (action_payload, error) in malformed {
            assert_eq!(decode_request(&frame_request(action_payload)), Err(error));
        }
    }

    #[test]
    fn round_trips_v2_bounds_and_rejects_invalid_inputs() {
        let mut v1 = encode_request(&Request {
            id: "client-1".into(),
            action: Action::Inspect,
        })
        .unwrap();
        v1[4..6].copy_from_slice(&1_u16.to_le_bytes());
        assert_eq!(
            decode_request(&v1),
            Err(Error::UnsupportedVersion { version: 1 })
        );

        let response = Response::Snapshot(snapshot());
        let encoded_response = encode_response(&response).unwrap();
        assert_eq!(decode_response(&encoded_response).unwrap(), response);
        let runtime = LifecycleResponse::Runtime(Runtime {
            generation: "g1-0123456789abcdef0123456789abcdef".into(),
            eon_version: "0.1.0".into(),
            workspace_protocol: VERSION,
            component_report: "eon-alpha x86_64-linux".into(),
            sessions: vec!["session-1".into()],
            attach: Availability {
                available: true,
                reason: "compatible EONW v2 supervisor".into(),
            },
            stop: Availability {
                available: true,
                reason: "generation-aware supervisor".into(),
            },
        });
        assert_eq!(
            decode_lifecycle_response(&encode_lifecycle_response(&runtime).unwrap()).unwrap(),
            runtime
        );

        let identity = |prefix: &str, number: usize| {
            let suffix = number.to_string();
            format!(
                "{prefix}{}{suffix}",
                "x".repeat(MAX_ID_BYTES - prefix.len() - suffix.len())
            )
        };
        let panes_per_tab = MAX_PANES / MAX_TABS;
        let maximal = Response::Snapshot(Snapshot {
            active_tab: identity("t", 1),
            tabs: (1..=MAX_TABS)
                .map(|tab_number| {
                    let first_pane = (tab_number - 1) * panes_per_tab + 1;
                    Tab {
                        id: identity("t", tab_number),
                        directory: vec![b'/'; MAX_DIRECTORY_BYTES],
                        selected_pane: identity("p", first_pane),
                        panes: (first_pane..first_pane + panes_per_tab)
                            .map(|pane_number| Pane {
                                id: identity("p", pane_number),
                                session: identity("session-", pane_number),
                                endpoint: vec![pane_number as u8; MAX_ENDPOINT_BYTES],
                                live: true,
                            })
                            .collect(),
                    }
                })
                .collect(),
        });
        let encoded_maximal = encode_response(&maximal).unwrap();
        assert_eq!(decode_response(&encoded_maximal).unwrap(), maximal);

        for directory in [Vec::new(), b"/tmp/eon\0bad".to_vec()] {
            let invalid = Request {
                id: "client-1".into(),
                action: Action::SetTabDirectory {
                    tab: "t1".into(),
                    directory,
                },
            };
            assert_eq!(
                encode_request(&invalid),
                Err(Error::InvalidValue { field: "directory" })
            );
        }

        let mut oversized = snapshot();
        oversized.tabs[0].directory = vec![b'x'; MAX_DIRECTORY_BYTES + 1];
        assert_eq!(
            encode_response(&Response::Snapshot(oversized)),
            Err(Error::FieldTooLong {
                field: "directory",
                length: MAX_DIRECTORY_BYTES + 1,
            })
        );
    }
}
