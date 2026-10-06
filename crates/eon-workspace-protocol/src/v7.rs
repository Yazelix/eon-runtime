//! EONW v7 permits independent pending Project pickers in separate tabs.

use super::v6;

pub use super::v6::{
    ALT, Action, Availability, CTRL, CodexQuota, CodexQuotaState, CodexQuotaWindow, Direction,
    Error, Failure, HEADER_BYTES, InvokeIntent, LifecycleResponse, MAX_CODEX_QUOTA_WINDOWS,
    MAX_DETAIL_BYTES, MAX_DIRECTORY_BYTES, MAX_ENTRIES, MAX_ENTRY_ID_BYTES, MAX_KEY_BYTES,
    MAX_LABEL_BYTES, MAX_SESSIONS, MAX_TABS, PROJECT_ENTRY, Pane, Popup, PopupEntry, PopupGeometry,
    PopupTarget, Request, Response, Runtime, SHIFT, SUPER, Shortcut, Snapshot, Stopped, Tab,
    WorkspaceAction, declared_message_len,
};

pub const VERSION: u16 = 7;
pub type Result<T> = std::result::Result<T, Error>;

pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    let mut bytes = v6::encode_request(request)?;
    v6::set_version(&mut bytes, VERSION);
    Ok(bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    let mut bytes = v6::reframe(bytes, VERSION)?;
    v6::set_version(&mut bytes, v6::VERSION);
    v6::decode_request(&bytes)
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>> {
    v6::encode_response_version(response, VERSION, true)
}

pub fn decode_response(bytes: &[u8]) -> Result<Response> {
    v6::decode_response_version(bytes, VERSION, true)
}

pub fn encode_lifecycle_response(response: &LifecycleResponse) -> Result<Vec<u8>> {
    let mut bytes = v6::encode_lifecycle_response(response)?;
    v6::set_version(&mut bytes, VERSION);
    Ok(bytes)
}

pub fn decode_lifecycle_response(bytes: &[u8]) -> Result<LifecycleResponse> {
    let mut bytes = v6::reframe(bytes, VERSION)?;
    v6::set_version(&mut bytes, v6::VERSION);
    v6::decode_lifecycle_response(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_pending_tabs_roundtrip_without_relaxing_v6_or_identity_checks() {
        let pending = |number: usize| Tab {
            id: format!("t{number}"),
            directory: b"/tmp".to_vec(),
            pending: true,
            selected_pane: None,
            selected_popup: Some(format!("u{number}")),
            panes: vec![],
            popups: vec![Popup {
                id: format!("u{number}"),
                entry: PROJECT_ENTRY.into(),
                session: format!("directory-picker-{number}"),
                endpoint: format!("/run/eon/k{number}.sock").into_bytes(),
            }],
        };
        let snapshot = Snapshot {
            active_tab: "t2".into(),
            geometry: PopupGeometry {
                side_margin: 8.0,
                vertical_margin: 4.0,
            },
            entries: vec![PopupEntry {
                id: PROJECT_ENTRY.into(),
                label: "Project".into(),
                shortcut: Shortcut {
                    modifiers: ALT,
                    key: "KeyZ".into(),
                },
            }],
            tabs: vec![pending(1), pending(2)],
            codex_quota: None,
        };
        let response = Response::Snapshot(snapshot.clone());
        let bytes = encode_response(&response).unwrap();
        assert_eq!(decode_response(&bytes).unwrap(), response);
        assert!(v6::decode_response(&bytes).is_err());
        assert!(v6::encode_response(&response).is_err());

        let mut aliased = snapshot.clone();
        aliased.tabs[1].popups[0].endpoint = aliased.tabs[0].popups[0].endpoint.clone();
        assert!(encode_response(&Response::Snapshot(aliased)).is_err());

        let request = Request {
            id: "create".into(),
            action: Action::Workspace(WorkspaceAction::CreateTab),
        };
        let bytes = encode_request(&request).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), request);
        assert!(v6::decode_request(&bytes).is_err());
    }
}
