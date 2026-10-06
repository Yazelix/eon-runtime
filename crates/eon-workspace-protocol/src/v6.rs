//! EONW v6 adds bounded, presentation-safe Codex quota facts.

use super::{v2, v5};

pub use super::v2::{
    Availability, Direction, Error, Failure, HEADER_BYTES, LifecycleResponse, MAX_DETAIL_BYTES,
    MAX_DIRECTORY_BYTES, MAX_TABS, Pane, Runtime, Stopped, declared_message_len,
};
pub use super::v5::{
    ALT, Action, CTRL, InvokeIntent, MAX_ENTRIES, MAX_ENTRY_ID_BYTES, MAX_KEY_BYTES,
    MAX_LABEL_BYTES, MAX_SESSIONS, PROJECT_ENTRY, Popup, PopupEntry, PopupGeometry, PopupTarget,
    Request, SHIFT, SUPER, Shortcut, Tab, WorkspaceAction,
};

pub const VERSION: u16 = 6;
pub const MAX_CODEX_QUOTA_WINDOWS: usize = 2;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexQuotaState {
    Fresh,
    Stale,
    Blocked,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexQuotaWindow {
    pub duration_minutes: u32,
    pub remaining_percent: u8,
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexQuota {
    pub state: CodexQuotaState,
    pub observed_at: u64,
    pub windows: Vec<CodexQuotaWindow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub active_tab: String,
    pub geometry: PopupGeometry,
    pub entries: Vec<PopupEntry>,
    pub tabs: Vec<Tab>,
    pub codex_quota: Option<CodexQuota>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Snapshot(Snapshot),
    Failure(Failure),
}

impl Snapshot {
    pub fn check_popup_action(&self, action: &Action) -> Result<()> {
        v5::check_popup_action(&self.entries, &self.tabs, action)
    }
}

pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    let mut bytes = v5::encode_request(request)?;
    set_version(&mut bytes, VERSION);
    Ok(bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    let mut bytes = reframe(bytes, VERSION)?;
    set_version(&mut bytes, v5::VERSION);
    v5::decode_request(&bytes)
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>> {
    encode_response_version(response, VERSION, false)
}

pub(super) fn encode_response_version(
    response: &Response,
    version: u16,
    multiple_pending: bool,
) -> Result<Vec<u8>> {
    let mut payload = v2::Encoder::default();
    let kind = match response {
        Response::Snapshot(snapshot) => {
            let workspace = as_v5(snapshot);
            v5::validate_snapshot_with_pending(&workspace, multiple_pending)?;
            validate_quota(snapshot.codex_quota.as_ref())?;
            v5::encode_workspace(&mut payload, &workspace);
            encode_quota(&mut payload, snapshot.codex_quota.as_ref());
            v2::SNAPSHOT
        }
        Response::Failure(failure) => {
            v2::encode_failure(&mut payload, failure)?;
            v2::FAILURE
        }
    };
    v2::frame_version(version, kind, payload.bytes)
}

pub fn decode_response(bytes: &[u8]) -> Result<Response> {
    decode_response_version(bytes, VERSION, false)
}

pub(super) fn decode_response_version(
    bytes: &[u8],
    version: u16,
    multiple_pending: bool,
) -> Result<Response> {
    let (kind, payload) = v2::unframe_version(bytes, version)?;
    let mut decoder = v2::Decoder::new(payload);
    let response = match kind {
        v2::SNAPSHOT => {
            let workspace = v5::decode_workspace(&mut decoder)?;
            let codex_quota = decode_quota(&mut decoder)?;
            v5::validate_snapshot_with_pending(&workspace, multiple_pending)?;
            validate_quota(codex_quota.as_ref())?;
            let snapshot = Snapshot {
                active_tab: workspace.active_tab,
                geometry: workspace.geometry,
                entries: workspace.entries,
                tabs: workspace.tabs,
                codex_quota,
            };
            Response::Snapshot(snapshot)
        }
        v2::FAILURE => Response::Failure(v2::decode_failure(&mut decoder)?),
        value => return Err(Error::InvalidKind { value }),
    };
    decoder.finish()?;
    Ok(response)
}

pub fn encode_lifecycle_response(response: &LifecycleResponse) -> Result<Vec<u8>> {
    let mut bytes = v5::encode_lifecycle_response(response)?;
    set_version(&mut bytes, VERSION);
    Ok(bytes)
}

pub fn decode_lifecycle_response(bytes: &[u8]) -> Result<LifecycleResponse> {
    let mut bytes = reframe(bytes, VERSION)?;
    set_version(&mut bytes, v5::VERSION);
    v5::decode_lifecycle_response(&bytes)
}

fn as_v5(snapshot: &Snapshot) -> v5::Snapshot {
    v5::Snapshot {
        active_tab: snapshot.active_tab.clone(),
        geometry: snapshot.geometry,
        entries: snapshot.entries.clone(),
        tabs: snapshot.tabs.clone(),
    }
}

fn validate_quota(quota: Option<&CodexQuota>) -> Result<()> {
    let Some(quota) = quota else {
        return Ok(());
    };
    if quota.observed_at == 0 || quota.windows.len() > MAX_CODEX_QUOTA_WINDOWS {
        return Err(Error::InvalidSnapshot {
            field: "Codex quota",
        });
    }
    let carries_windows = matches!(quota.state, CodexQuotaState::Fresh | CodexQuotaState::Stale);
    if carries_windows == quota.windows.is_empty() {
        return Err(Error::InvalidSnapshot {
            field: "Codex quota state",
        });
    }
    for window in &quota.windows {
        if window.duration_minutes == 0
            || window.remaining_percent > 100
            || (quota.state == CodexQuotaState::Stale && window.resets_at.is_none())
            || window
                .resets_at
                .is_some_and(|reset| reset <= quota.observed_at)
        {
            return Err(Error::InvalidSnapshot {
                field: "Codex quota window",
            });
        }
    }
    Ok(())
}

fn encode_quota(encoder: &mut v2::Encoder, quota: Option<&CodexQuota>) {
    encoder.byte(u8::from(quota.is_some()));
    let Some(quota) = quota else { return };
    encoder.byte(match quota.state {
        CodexQuotaState::Fresh => 0,
        CodexQuotaState::Stale => 1,
        CodexQuotaState::Blocked => 2,
        CodexQuotaState::Unknown => 3,
    });
    encoder
        .bytes
        .extend_from_slice(&quota.observed_at.to_le_bytes());
    encoder.count(quota.windows.len());
    for window in &quota.windows {
        encoder
            .bytes
            .extend_from_slice(&window.duration_minutes.to_le_bytes());
        encoder.byte(window.remaining_percent);
        encoder.byte(u8::from(window.resets_at.is_some()));
        if let Some(reset) = window.resets_at {
            encoder.bytes.extend_from_slice(&reset.to_le_bytes());
        }
    }
}

fn decode_quota(decoder: &mut v2::Decoder<'_>) -> Result<Option<CodexQuota>> {
    if !decode_bool(decoder, "Codex quota presence")? {
        return Ok(None);
    }
    let state = match decoder.byte()? {
        0 => CodexQuotaState::Fresh,
        1 => CodexQuotaState::Stale,
        2 => CodexQuotaState::Blocked,
        3 => CodexQuotaState::Unknown,
        value => {
            return Err(Error::InvalidTag {
                field: "Codex quota state",
                value,
            });
        }
    };
    let observed_at = decode_u64(decoder)?;
    let count = decoder.count("Codex quota windows", MAX_CODEX_QUOTA_WINDOWS)?;
    let mut windows = Vec::with_capacity(count);
    for _ in 0..count {
        windows.push(CodexQuotaWindow {
            duration_minutes: decode_u32(decoder)?,
            remaining_percent: decoder.byte()?,
            resets_at: decode_bool(decoder, "Codex quota reset")?
                .then(|| decode_u64(decoder))
                .transpose()?,
        });
    }
    Ok(Some(CodexQuota {
        state,
        observed_at,
        windows,
    }))
}

fn decode_bool(decoder: &mut v2::Decoder<'_>, field: &'static str) -> Result<bool> {
    match decoder.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::InvalidValue { field }),
    }
}

fn decode_u32(decoder: &mut v2::Decoder<'_>) -> Result<u32> {
    Ok(u32::from(decoder.number()?) | (u32::from(decoder.number()?) << 16))
}

fn decode_u64(decoder: &mut v2::Decoder<'_>) -> Result<u64> {
    let mut value = 0;
    for shift in [0, 16, 32, 48] {
        value |= u64::from(decoder.number()?) << shift;
    }
    Ok(value)
}

pub(super) fn reframe(bytes: &[u8], version: u16) -> Result<Vec<u8>> {
    v2::unframe_version(bytes, version)?;
    Ok(bytes.to_vec())
}

pub(super) fn set_version(bytes: &mut [u8], version: u16) {
    bytes[4..6].copy_from_slice(&version.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            active_tab: "t1".into(),
            geometry: PopupGeometry {
                side_margin: 8.0,
                vertical_margin: 4.0,
            },
            entries: vec![],
            tabs: vec![Tab {
                id: "t1".into(),
                directory: b"/tmp".to_vec(),
                pending: false,
                selected_pane: Some("p1".into()),
                selected_popup: None,
                panes: vec![Pane {
                    id: "p1".into(),
                    session: "session-1".into(),
                    endpoint: b"/run/eon/orbit.sock".to_vec(),
                    live: true,
                }],
                popups: vec![],
            }],
            codex_quota: Some(CodexQuota {
                state: CodexQuotaState::Fresh,
                observed_at: 1_800_000_000,
                windows: vec![
                    CodexQuotaWindow {
                        duration_minutes: 300,
                        remaining_percent: 73,
                        resets_at: Some(1_800_003_600),
                    },
                    CodexQuotaWindow {
                        duration_minutes: 300,
                        remaining_percent: 41,
                        resets_at: None,
                    },
                ],
            }),
        }
    }

    #[test]
    fn quota_is_bounded_and_v6_is_explicitly_incompatible() {
        let state = snapshot();
        let bytes = encode_response(&Response::Snapshot(state.clone())).unwrap();
        assert_eq!(
            decode_response(&bytes).unwrap(),
            Response::Snapshot(state.clone())
        );
        assert!(v5::decode_response(&bytes).is_err());

        for change in [
            |quota: &mut CodexQuota| quota.observed_at = 0,
            |quota: &mut CodexQuota| quota.windows[0].remaining_percent = 101,
            |quota: &mut CodexQuota| quota.windows[0].duration_minutes = 0,
            |quota: &mut CodexQuota| quota.windows[0].resets_at = Some(quota.observed_at),
            |quota: &mut CodexQuota| quota.windows.push(quota.windows[0].clone()),
            |quota: &mut CodexQuota| quota.state = CodexQuotaState::Stale,
            |quota: &mut CodexQuota| quota.state = CodexQuotaState::Blocked,
        ] {
            let mut invalid = state.clone();
            change(invalid.codex_quota.as_mut().unwrap());
            assert!(encode_response(&Response::Snapshot(invalid)).is_err());
        }

        for state in [CodexQuotaState::Blocked, CodexQuotaState::Unknown] {
            let mut snapshot = snapshot();
            let quota = snapshot.codex_quota.as_mut().unwrap();
            quota.state = state;
            quota.windows.clear();
            let response = Response::Snapshot(snapshot);
            assert_eq!(
                decode_response(&encode_response(&response).unwrap()).unwrap(),
                response
            );
        }

        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_response(&trailing).is_err());

        let request = Request {
            id: "client-1".into(),
            action: Action::Workspace(WorkspaceAction::Inspect),
        };
        let bytes = encode_request(&request).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), request);
        assert!(v5::decode_request(&bytes).is_err());

        let lifecycle = LifecycleResponse::Runtime(Runtime {
            generation: "g1-test".into(),
            eon_version: "0.1.0".into(),
            workspace_protocol: VERSION,
            component_report: "eon-alpha x86_64-linux".into(),
            sessions: vec![],
            attach: Availability {
                available: true,
                reason: "available".into(),
            },
            stop: Availability {
                available: true,
                reason: "available".into(),
            },
        });
        let bytes = encode_lifecycle_response(&lifecycle).unwrap();
        assert_eq!(decode_lifecycle_response(&bytes).unwrap(), lifecycle);
        assert!(v5::decode_lifecycle_response(&bytes).is_err());
    }
}
