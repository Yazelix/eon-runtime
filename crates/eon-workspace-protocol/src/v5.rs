//! Shared tab-scoped popup protocol used by Eon and Venus.

use super::{v2, v3};
use std::collections::HashSet;

pub use super::v2::{
    Availability, Direction, Error, Failure, HEADER_BYTES, LifecycleResponse, MAX_DETAIL_BYTES,
    MAX_DIRECTORY_BYTES, MAX_TABS, Pane, Runtime, Stopped, declared_message_len,
};
pub use super::v3::Action as WorkspaceAction;

pub const VERSION: u16 = 5;
pub const MAX_ENTRIES: usize = 32;
pub const MAX_ENTRY_ID_BYTES: usize = 64;
pub const MAX_LABEL_BYTES: usize = 128;
pub const MAX_KEY_BYTES: usize = 16;
pub const MAX_SESSIONS: usize = v2::MAX_PANES;
pub const PROJECT_ENTRY: &str = "project";
pub const SHIFT: u8 = 1;
pub const CTRL: u8 = 2;
pub const ALT: u8 = 4;
pub const SUPER: u8 = 8;
pub type Result<T> = std::result::Result<T, Error>;

/// Canonical physical key names, independent of keyboard layout or display text.
/// KeyA..KeyZ, Digit0..Digit9, F1..F24 and the named keys in `validate` are supported.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub modifiers: u8,
    pub key: String,
}

impl Shortcut {
    /// Validate normalized wire syntax. The config owner also checks collisions
    /// against fixed workspace/native bindings before publishing the catalog.
    pub fn validate(&self) -> Result<()> {
        let key = self.key.as_str();
        let valid_key = match key.as_bytes() {
            [b'K', b'e', b'y', b'A'..=b'Z'] | [b'D', b'i', b'g', b'i', b't', b'0'..=b'9'] => true,
            _ => {
                key.strip_prefix('F')
                    .and_then(|n| n.parse::<u8>().ok())
                    .is_some_and(|n| (1..=24).contains(&n) && key == format!("F{n}"))
                    || matches!(
                        key,
                        "Escape"
                            | "Tab"
                            | "Enter"
                            | "Backspace"
                            | "Space"
                            | "Insert"
                            | "Delete"
                            | "Home"
                            | "End"
                            | "PageUp"
                            | "PageDown"
                            | "ArrowLeft"
                            | "ArrowRight"
                            | "ArrowUp"
                            | "ArrowDown"
                            | "Backquote"
                            | "Backslash"
                            | "BracketLeft"
                            | "BracketRight"
                            | "Comma"
                            | "Equal"
                            | "Minus"
                            | "Period"
                            | "Quote"
                            | "Semicolon"
                            | "Slash"
                    )
            }
        };
        if self.modifiers == 0
            || self.modifiers & !(SHIFT | CTRL | ALT | SUPER) != 0
            || !valid_key
            || (self.modifiers == CTRL && matches!(key, "KeyC" | "Backslash"))
        {
            return Err(Error::InvalidValue { field: "shortcut" });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupEntry {
    pub id: String,
    pub label: String,
    pub shortcut: Shortcut,
}

/// One live instance. A replacement receives a fresh id and endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popup {
    pub id: String,
    pub entry: String,
    pub session: String,
    pub endpoint: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupGeometry {
    pub side_margin: f32,
    pub vertical_margin: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: String,
    pub directory: Vec<u8>,
    pub pending: bool,
    pub selected_pane: Option<String>,
    /// Retained when another tab is active; not native keyboard-focus state.
    pub selected_popup: Option<String>,
    pub panes: Vec<Pane>,
    pub popups: Vec<Popup>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub active_tab: String,
    pub geometry: PopupGeometry,
    pub entries: Vec<PopupEntry>,
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupTarget {
    pub tab: String,
    pub instance: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvokeIntent {
    Toggle,
    /// A visible but unfocused popup receives focus instead of being hidden.
    Focus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Existing common actions, except PickTabDirectory (retired in v5).
    /// SetTabDirectory is explicit retargeting, not chooser completion.
    Workspace(WorkspaceAction),
    InvokePopup {
        tab: String,
        entry: String,
        /// None asserts absence; Some must equal the currently owned instance.
        expected_instance: Option<String>,
        intent: InvokeIntent,
    },
    DismissPopup(PopupTarget),
    CommitDirectory {
        target: PopupTarget,
        directory: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: String,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Snapshot(Snapshot),
    Failure(Failure),
}

impl Snapshot {
    /// Check popup preconditions against the owner's current, valid snapshot
    /// immediately before mutation. This does not execute lifecycle policy or
    /// validate ordinary workspace actions. Decoding alone cannot detect staleness.
    pub fn check_popup_action(&self, action: &Action) -> Result<()> {
        check_popup_action(&self.entries, &self.tabs, action)
    }
}

pub(super) fn check_popup_action(
    entries: &[PopupEntry],
    tabs: &[Tab],
    action: &Action,
) -> Result<()> {
    validate_popup_action(action)?;
    let matches = match action {
        Action::Workspace(_) => return Ok(()),
        Action::InvokePopup {
            tab,
            entry,
            expected_instance,
            ..
        } => {
            entries.iter().any(|item| item.id == *entry)
                && tabs.iter().find(|item| item.id == *tab).is_some_and(|tab| {
                    tab.popups
                        .iter()
                        .find(|popup| popup.entry == *entry)
                        .map(|popup| &popup.id)
                        == expected_instance.as_ref()
                })
        }
        Action::DismissPopup(target) | Action::CommitDirectory { target, .. } => tabs
            .iter()
            .find(|tab| tab.id == target.tab)
            .is_some_and(|tab| {
                tab.popups.iter().any(|popup| {
                    popup.id == target.instance
                        && (!matches!(action, Action::CommitDirectory { .. })
                            || (popup.entry == PROJECT_ENTRY
                                && tab.selected_popup.as_ref() == Some(&popup.id)))
                })
            }),
    };
    if matches {
        Ok(())
    } else {
        Err(Error::InvalidValue {
            field: "popup target",
        })
    }
}

pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    validate_popup_action(&request.action)?;
    if let Action::Workspace(action) = &request.action {
        return v3::encode_request_version(
            &v3::Request {
                id: request.id.clone(),
                action: action.clone(),
            },
            VERSION,
        );
    }
    v2::identity("request id", &request.id)?;
    let mut payload = v2::Encoder::default();
    payload.string(&request.id);
    match &request.action {
        Action::InvokePopup {
            tab,
            entry,
            expected_instance,
            intent,
        } => {
            payload.byte(19);
            payload.string(tab);
            payload.string(entry);
            encode_optional(&mut payload, expected_instance);
            payload.byte(match intent {
                InvokeIntent::Toggle => 0,
                InvokeIntent::Focus => 1,
            });
        }
        Action::DismissPopup(target) | Action::CommitDirectory { target, .. } => {
            payload.byte(if matches!(request.action, Action::DismissPopup(_)) {
                20
            } else {
                21
            });
            payload.string(&target.tab);
            payload.string(&target.instance);
            if let Action::CommitDirectory { directory, .. } = &request.action {
                payload.raw(directory);
            }
        }
        Action::Workspace(_) => unreachable!(),
    }
    v2::frame_version(VERSION, v2::REQUEST, payload.bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    let (kind, payload) = v2::unframe_version(bytes, VERSION)?;
    if kind != v2::REQUEST {
        return Err(Error::InvalidKind { value: kind });
    }
    let mut decoder = v2::Decoder::new(payload);
    let id = decoder.string("request id", v2::MAX_ID_BYTES)?;
    v2::identity("request id", &id)?;
    let action = match decoder.byte()? {
        19 => Action::InvokePopup {
            tab: decoder.string("tab id", v2::MAX_ID_BYTES)?,
            entry: decoder.string("entry id", MAX_ENTRY_ID_BYTES)?,
            expected_instance: decode_optional(&mut decoder, "expected instance")?,
            intent: match decoder.byte()? {
                0 => InvokeIntent::Toggle,
                1 => InvokeIntent::Focus,
                value => {
                    return Err(Error::InvalidTag {
                        field: "popup intent",
                        value,
                    });
                }
            },
        },
        tag @ (20 | 21) => {
            let target = PopupTarget {
                tab: decoder.string("tab id", v2::MAX_ID_BYTES)?,
                instance: decoder.string("popup id", v2::MAX_ID_BYTES)?,
            };
            if tag == 20 {
                Action::DismissPopup(target)
            } else {
                Action::CommitDirectory {
                    target,
                    directory: decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec(),
                }
            }
        }
        _ => {
            let request = v3::decode_request_version(bytes, VERSION)?;
            let action = Action::Workspace(request.action);
            validate_popup_action(&action)?;
            return Ok(Request { id, action });
        }
    };
    decoder.finish()?;
    validate_popup_action(&action)?;
    Ok(Request { id, action })
}

fn validate_popup_action(action: &Action) -> Result<()> {
    match action {
        Action::Workspace(WorkspaceAction::PickTabDirectory) => {
            return Err(Error::InvalidValue {
                field: "retired picker action",
            });
        }
        Action::Workspace(_) => (),
        Action::InvokePopup {
            tab,
            entry,
            expected_instance,
            ..
        } => {
            v2::identity("tab id", tab)?;
            entry_id(entry)?;
            if let Some(id) = expected_instance {
                v2::identity("popup id", id)?;
            }
        }
        Action::DismissPopup(target) | Action::CommitDirectory { target, .. } => {
            v2::identity("tab id", &target.tab)?;
            v2::identity("popup id", &target.instance)?;
            if let Action::CommitDirectory { directory, .. } = action {
                v2::validate_directory(directory)?;
            }
        }
    }
    Ok(())
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>> {
    let mut payload = v2::Encoder::default();
    let kind = match response {
        Response::Snapshot(snapshot) => {
            validate_snapshot(snapshot)?;
            encode_workspace(&mut payload, snapshot);
            v2::SNAPSHOT
        }
        Response::Failure(failure) => {
            v2::encode_failure(&mut payload, failure)?;
            v2::FAILURE
        }
    };
    v2::frame_version(VERSION, kind, payload.bytes)
}

pub fn decode_response(bytes: &[u8]) -> Result<Response> {
    let (kind, payload) = v2::unframe_version(bytes, VERSION)?;
    let mut decoder = v2::Decoder::new(payload);
    let response = match kind {
        v2::SNAPSHOT => {
            let snapshot = decode_workspace(&mut decoder)?;
            validate_snapshot(&snapshot)?;
            Response::Snapshot(snapshot)
        }
        v2::FAILURE => Response::Failure(v2::decode_failure(&mut decoder)?),
        value => return Err(Error::InvalidKind { value }),
    };
    decoder.finish()?;
    Ok(response)
}

pub fn encode_lifecycle_response(response: &LifecycleResponse) -> Result<Vec<u8>> {
    v2::encode_lifecycle_response_version(response, VERSION, true)
}

pub fn decode_lifecycle_response(bytes: &[u8]) -> Result<LifecycleResponse> {
    v2::decode_lifecycle_response_version(bytes, VERSION, true)
}

fn encode_optional(encoder: &mut v2::Encoder, value: &Option<String>) {
    encoder.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        encoder.string(value);
    }
}

fn decode_bool(decoder: &mut v2::Decoder<'_>, field: &'static str) -> Result<bool> {
    match decoder.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::InvalidValue { field }),
    }
}

fn decode_optional(decoder: &mut v2::Decoder<'_>, field: &'static str) -> Result<Option<String>> {
    if decode_bool(decoder, field)? {
        Ok(Some(decoder.string(field, v2::MAX_ID_BYTES)?))
    } else {
        Ok(None)
    }
}

pub(super) fn encode_workspace(encoder: &mut v2::Encoder, snapshot: &Snapshot) {
    encoder.string(&snapshot.active_tab);
    for margin in [
        snapshot.geometry.side_margin,
        snapshot.geometry.vertical_margin,
    ] {
        encoder.bytes.extend_from_slice(&margin.to_le_bytes());
    }
    encoder.count(snapshot.entries.len());
    for entry in &snapshot.entries {
        encoder.string(&entry.id);
        encoder.string(&entry.label);
        encoder.byte(entry.shortcut.modifiers);
        encoder.string(&entry.shortcut.key);
    }
    encoder.count(snapshot.tabs.len());
    for tab in &snapshot.tabs {
        encoder.string(&tab.id);
        encoder.raw(&tab.directory);
        encoder.byte(u8::from(tab.pending));
        encode_optional(encoder, &tab.selected_pane);
        encode_optional(encoder, &tab.selected_popup);
        encoder.count(tab.panes.len());
        for pane in &tab.panes {
            encoder.string(&pane.id);
            encoder.string(&pane.session);
            encoder.raw(&pane.endpoint);
            encoder.byte(u8::from(pane.live));
        }
        encoder.count(tab.popups.len());
        for popup in &tab.popups {
            encoder.string(&popup.id);
            encoder.string(&popup.entry);
            encoder.string(&popup.session);
            encoder.raw(&popup.endpoint);
        }
    }
}

pub(super) fn decode_workspace(decoder: &mut v2::Decoder<'_>) -> Result<Snapshot> {
    let active_tab = decoder.string("active tab", v2::MAX_ID_BYTES)?;
    let mut margin = || {
        Ok::<_, Error>(f32::from_bits(
            u32::from(decoder.number()?) | (u32::from(decoder.number()?) << 16),
        ))
    };
    let geometry = PopupGeometry {
        side_margin: margin()?,
        vertical_margin: margin()?,
    };
    let count = decoder.count("entries", MAX_ENTRIES)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(PopupEntry {
            id: decoder.string("entry id", MAX_ENTRY_ID_BYTES)?,
            label: decoder.string("entry label", MAX_LABEL_BYTES)?,
            shortcut: Shortcut {
                modifiers: decoder.byte()?,
                key: decoder.string("shortcut key", MAX_KEY_BYTES)?,
            },
        });
    }
    let count = decoder.count("tabs", MAX_TABS)?;
    let mut tabs = Vec::with_capacity(count);
    let mut sessions = 0;
    for _ in 0..count {
        let id = decoder.string("tab id", v2::MAX_ID_BYTES)?;
        let directory = decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec();
        let pending = decode_bool(decoder, "pending tab")?;
        let selected_pane = decode_optional(decoder, "selected pane")?;
        let selected_popup = decode_optional(decoder, "selected popup")?;
        let count = decoder.count("sessions", MAX_SESSIONS - sessions)?;
        sessions += count;
        let mut panes = Vec::with_capacity(count);
        for _ in 0..count {
            panes.push(Pane {
                id: decoder.string("pane id", v2::MAX_ID_BYTES)?,
                session: decoder.string("session id", v2::MAX_ID_BYTES)?,
                endpoint: decoder.raw("endpoint", v2::MAX_ENDPOINT_BYTES)?.to_vec(),
                live: decode_bool(decoder, "liveness")?,
            });
        }
        let count = decoder.count("sessions", (MAX_SESSIONS - sessions).min(MAX_ENTRIES))?;
        sessions += count;
        let mut popups = Vec::with_capacity(count);
        for _ in 0..count {
            popups.push(Popup {
                id: decoder.string("popup id", v2::MAX_ID_BYTES)?,
                entry: decoder.string("entry id", MAX_ENTRY_ID_BYTES)?,
                session: decoder.string("session id", v2::MAX_ID_BYTES)?,
                endpoint: decoder.raw("endpoint", v2::MAX_ENDPOINT_BYTES)?.to_vec(),
            });
        }
        tabs.push(Tab {
            id,
            directory,
            pending,
            selected_pane,
            selected_popup,
            panes,
            popups,
        });
    }
    Ok(Snapshot {
        active_tab,
        geometry,
        entries,
        tabs,
    })
}

fn entry_id(id: &str) -> Result<()> {
    v2::nonempty("entry id", id, MAX_ENTRY_ID_BYTES)?;
    v2::identity("entry id", id)
}

pub(super) fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    validate_snapshot_with_pending(snapshot, false)
}

pub(super) fn validate_snapshot_with_pending(
    snapshot: &Snapshot,
    multiple_pending: bool,
) -> Result<()> {
    if snapshot.tabs.is_empty() || snapshot.tabs.len() > MAX_TABS {
        return Err(Error::InvalidSnapshot { field: "tabs" });
    }
    if snapshot.entries.len() > MAX_ENTRIES {
        return Err(Error::InvalidSnapshot { field: "entries" });
    }
    for margin in [
        snapshot.geometry.side_margin,
        snapshot.geometry.vertical_margin,
    ] {
        if !margin.is_finite() || !(0.0..=128.0).contains(&margin) {
            return Err(Error::InvalidValue {
                field: "popup margin",
            });
        }
    }
    let mut entries = HashSet::new();
    let mut shortcuts = HashSet::new();
    for entry in &snapshot.entries {
        entry_id(&entry.id)?;
        v2::nonempty("entry label", &entry.label, MAX_LABEL_BYTES)?;
        if entry.label.chars().any(char::is_control) {
            return Err(Error::InvalidValue {
                field: "entry label",
            });
        }
        entry.shortcut.validate()?;
        if !entries.insert(entry.id.as_str()) || !shortcuts.insert(&entry.shortcut) {
            return Err(Error::InvalidSnapshot { field: "entries" });
        }
    }
    let mut identities = HashSet::new();
    let mut sessions = HashSet::new();
    let mut endpoints = HashSet::new();
    let mut pending = 0;
    for tab in &snapshot.tabs {
        v2::identity("tab id", &tab.id)?;
        v2::validate_directory(&tab.directory)?;
        if !identities.insert(tab.id.as_str()) {
            return Err(Error::InvalidSnapshot { field: "tab id" });
        }
        if tab.panes.is_empty() && tab.popups.is_empty() {
            return Err(Error::InvalidSnapshot { field: "empty tab" });
        }
        if tab
            .selected_pane
            .as_ref()
            .is_some_and(|id| !tab.panes.iter().any(|pane| pane.id == *id))
            || (tab.selected_pane.is_none() && !tab.panes.is_empty())
        {
            return Err(Error::InvalidSnapshot {
                field: "selected pane",
            });
        }
        if tab
            .selected_popup
            .as_ref()
            .is_some_and(|id| !tab.popups.iter().any(|popup| popup.id == *id))
        {
            return Err(Error::InvalidSnapshot {
                field: "selected popup",
            });
        }
        if tab.pending {
            pending += 1;
            if (!multiple_pending && pending > 1)
                || !tab.panes.is_empty()
                || tab.popups.len() != 1
                || tab.popups[0].entry != PROJECT_ENTRY
                || tab.selected_popup.as_ref() != Some(&tab.popups[0].id)
            {
                return Err(Error::InvalidSnapshot {
                    field: "pending tab",
                });
            }
        }
        let mut tab_entries = HashSet::new();
        for popup in &tab.popups {
            if !entries.contains(popup.entry.as_str()) || !tab_entries.insert(popup.entry.as_str())
            {
                return Err(Error::InvalidSnapshot {
                    field: "popup entry",
                });
            }
        }
        for (id, session, endpoint) in tab
            .panes
            .iter()
            .map(|p| (&p.id, &p.session, &p.endpoint))
            .chain(tab.popups.iter().map(|p| (&p.id, &p.session, &p.endpoint)))
        {
            v2::identity("surface id", id)?;
            v2::identity("session id", session)?;
            v2::validate_endpoint(endpoint, "endpoint")?;
            if !identities.insert(id.as_str())
                || !sessions.insert(session.as_str())
                || !endpoints.insert(endpoint.as_slice())
            {
                return Err(Error::InvalidSnapshot {
                    field: "surface identity",
                });
            }
            if sessions.len() > MAX_SESSIONS {
                return Err(Error::InvalidSnapshot { field: "sessions" });
            }
        }
    }
    if !snapshot
        .tabs
        .iter()
        .any(|tab| tab.id == snapshot.active_tab)
    {
        return Err(Error::InvalidSnapshot {
            field: "active tab",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, key: &str) -> PopupEntry {
        PopupEntry {
            id: id.into(),
            label: id.into(),
            shortcut: Shortcut {
                modifiers: ALT | SHIFT,
                key: key.into(),
            },
        }
    }

    fn popup(id: &str, entry: &str) -> Popup {
        Popup {
            id: id.into(),
            entry: entry.into(),
            session: format!("session-{id}"),
            endpoint: format!("/run/eon/{id}.sock").into_bytes(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            active_tab: "t1".into(),
            geometry: PopupGeometry {
                side_margin: 8.5,
                vertical_margin: 4.0,
            },
            entries: vec![entry("project", "KeyZ"), entry("agent", "KeyL")],
            tabs: vec![
                Tab {
                    id: "t1".into(),
                    directory: b"/tmp/\xff".to_vec(),
                    pending: false,
                    selected_pane: Some("p1".into()),
                    selected_popup: None,
                    panes: vec![Pane {
                        id: "p1".into(),
                        session: "session-1".into(),
                        endpoint: b"/run/\xff.sock".to_vec(),
                        live: true,
                    }],
                    popups: vec![popup("u1", "agent")],
                },
                Tab {
                    id: "t2".into(),
                    directory: b"/tmp".to_vec(),
                    pending: true,
                    selected_pane: None,
                    selected_popup: Some("u2".into()),
                    panes: vec![],
                    popups: vec![popup("u2", "project")],
                },
                Tab {
                    id: "t3".into(),
                    directory: b"/work".to_vec(),
                    pending: false,
                    selected_pane: None,
                    selected_popup: None,
                    panes: vec![],
                    popups: vec![popup("u3", "agent")],
                },
            ],
        }
    }

    #[test]
    fn popup_targets_round_trip_and_do_not_follow_replacements() {
        let mut state = snapshot();
        let actions = [
            Action::InvokePopup {
                tab: "t1".into(),
                entry: "agent".into(),
                expected_instance: Some("u1".into()),
                intent: InvokeIntent::Toggle,
            },
            Action::InvokePopup {
                tab: "t1".into(),
                entry: "agent".into(),
                expected_instance: Some("u1".into()),
                intent: InvokeIntent::Focus,
            },
            Action::DismissPopup(PopupTarget {
                tab: "t1".into(),
                instance: "u1".into(),
            }),
            Action::CommitDirectory {
                target: PopupTarget {
                    tab: "t2".into(),
                    instance: "u2".into(),
                },
                directory: b"/tmp/\xfe".to_vec(),
            },
        ];
        let mut encoded = Vec::new();
        for action in &actions {
            state.check_popup_action(action).unwrap();
            let request = Request {
                id: "client-1".into(),
                action: action.clone(),
            };
            let bytes = encode_request(&request).unwrap();
            assert_eq!(decode_request(&bytes).unwrap(), request);
            assert!(super::super::v4::decode_request(&bytes).is_err());
            encoded.push(bytes);
        }
        assert_ne!(encoded[0], encoded[1]);
        state.tabs[0].popups[0] = popup("u4", "agent");
        state.tabs[1].popups[0] = popup("u5", "project");
        state.tabs[1].selected_popup = Some("u5".into());
        for action in &actions {
            assert!(state.check_popup_action(action).is_err());
        }

        let absent = Action::InvokePopup {
            tab: "t1".into(),
            entry: "project".into(),
            expected_instance: None,
            intent: InvokeIntent::Toggle,
        };
        state.check_popup_action(&absent).unwrap();
        state.tabs[0].popups.push(popup("u6", "project"));
        assert!(state.check_popup_action(&absent).is_err());
        assert!(
            state
                .check_popup_action(&Action::CommitDirectory {
                    target: PopupTarget {
                        tab: "t1".into(),
                        instance: "u4".into()
                    },
                    directory: b"/tmp".to_vec(),
                })
                .is_err()
        );
        assert!(
            state
                .check_popup_action(&Action::DismissPopup(PopupTarget {
                    tab: "t1".into(),
                    instance: "u3".into(),
                }))
                .is_err()
        );
    }

    #[test]
    fn snapshots_preserve_hidden_and_inactive_work_and_reject_broken_ownership() {
        let state = snapshot();
        let response = Response::Snapshot(state.clone());
        let bytes = encode_response(&response).unwrap();
        assert_eq!(decode_response(&bytes).unwrap(), response);
        assert!(super::super::v4::decode_response(&bytes).is_err());
        for change in [
            |s: &mut Snapshot| s.tabs[0].popups[0].endpoint = s.tabs[0].panes[0].endpoint.clone(),
            |s: &mut Snapshot| s.tabs[0].popups[0].session = s.tabs[0].panes[0].session.clone(),
            |s: &mut Snapshot| s.tabs[0].popups[0].id = "p1".into(),
            |s: &mut Snapshot| s.tabs[0].popups.push(popup("other", "agent")),
            |s: &mut Snapshot| s.tabs[0].selected_popup = Some("u3".into()),
            |s: &mut Snapshot| s.tabs[0].selected_pane = Some("missing".into()),
            |s: &mut Snapshot| s.tabs[1].selected_popup = None,
            |s: &mut Snapshot| s.tabs[1].popups[0].entry = "agent".into(),
            |s: &mut Snapshot| s.tabs[2].popups.clear(),
            |s: &mut Snapshot| s.tabs[0].popups[0].entry = "missing".into(),
            |s: &mut Snapshot| s.entries[1].id = "project".into(),
            |s: &mut Snapshot| s.entries[1].shortcut = s.entries[0].shortcut.clone(),
            |s: &mut Snapshot| s.entries[0].shortcut.modifiers = 128,
            |s: &mut Snapshot| s.entries[0].shortcut.key = "keyz".into(),
            |s: &mut Snapshot| s.geometry.side_margin = f32::NAN,
            |s: &mut Snapshot| s.geometry.vertical_margin = 129.0,
            |s: &mut Snapshot| s.tabs[0].directory.push(0),
            |s: &mut Snapshot| s.entries[0].id = "x".repeat(MAX_ENTRY_ID_BYTES + 1),
            |s: &mut Snapshot| s.entries[0].label = "x".repeat(MAX_LABEL_BYTES + 1),
            |s: &mut Snapshot| s.entries[0].label = "tool\nnotice".into(),
            |s: &mut Snapshot| s.tabs[2].selected_pane = Some("p1".into()),
            |s: &mut Snapshot| {
                s.tabs[2].pending = true;
                s.tabs[2].popups[0].entry = "project".into();
                s.tabs[2].selected_popup = Some("u3".into());
            },
        ] {
            let mut invalid = state.clone();
            change(&mut invalid);
            assert!(encode_response(&Response::Snapshot(invalid.clone())).is_err());
            let mut payload = super::super::v2::Encoder::default();
            encode_workspace(&mut payload, &invalid);
            let bytes =
                super::super::v2::frame_version(VERSION, super::super::v2::SNAPSHOT, payload.bytes)
                    .unwrap();
            assert!(decode_response(&bytes).is_err());
        }
    }

    #[test]
    fn common_actions_stay_exact_and_new_requests_are_bounded() {
        let common = [
            WorkspaceAction::Inspect,
            WorkspaceAction::InspectRuntime,
            WorkspaceAction::InspectPresentation,
            WorkspaceAction::Present { workspace: true },
            WorkspaceAction::CreateTab,
            WorkspaceAction::CreatePane,
            WorkspaceAction::FocusId("p1".into()),
            WorkspaceAction::Focus(Direction::Left),
            WorkspaceAction::Move(Direction::Down),
            WorkspaceAction::CloseTab { tab: "t1".into() },
            WorkspaceAction::Stop {
                generation: "g1-test".into(),
            },
            WorkspaceAction::SetTabDirectory {
                tab: "t1".into(),
                directory: b"/tmp/\xff".to_vec(),
            },
        ];
        for action in common {
            let old = v3::Request {
                id: "client-1".into(),
                action: action.clone(),
            };
            let mut bytes = super::super::v4::encode_request(&old).unwrap();
            assert!(decode_request(&bytes).is_err());
            bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
            let request = Request {
                id: old.id,
                action: Action::Workspace(action),
            };
            assert_eq!(encode_request(&request).unwrap(), bytes);
            assert_eq!(decode_request(&bytes).unwrap(), request);
        }
        let retired = v3::Request {
            id: "r".into(),
            action: WorkspaceAction::PickTabDirectory,
        };
        let mut bytes = super::super::v4::encode_request(&retired).unwrap();
        assert_eq!(super::super::v4::decode_request(&bytes).unwrap(), retired);
        bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
        assert!(decode_request(&bytes).is_err());
        assert!(
            encode_request(&Request {
                id: retired.id,
                action: Action::Workspace(retired.action)
            })
            .is_err()
        );

        let request = Request {
            id: "r".into(),
            action: Action::InvokePopup {
                tab: "t".into(),
                entry: "git".into(),
                expected_instance: None,
                intent: InvokeIntent::Focus,
            },
        };
        let bytes = encode_request(&request).unwrap();
        assert_eq!(
            bytes,
            b"EONW\x05\x00\x01\x00\x0e\x00\x00\x00\x01\x00r\x13\x01\x00t\x03\x00git\x00\x01"
        );
        for end in 0..bytes.len() {
            assert!(decode_request(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_request(&trailing).is_err());
        for offset in [bytes.len() - 1, bytes.len() - 2] {
            let mut invalid = bytes.clone();
            invalid[offset] = 2;
            assert!(decode_request(&invalid).is_err());
        }
        let mut invalid = bytes;
        invalid[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(declared_message_len(&invalid).is_err());

        for response in [
            LifecycleResponse::Stopped(Stopped {
                generation: "g1-test".into(),
                sessions: vec![],
            }),
            LifecycleResponse::Runtime(Runtime {
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
            }),
            LifecycleResponse::Failure(Failure {
                code: "unavailable".into(),
                detail: "test failure".into(),
            }),
        ] {
            let bytes = encode_lifecycle_response(&response).unwrap();
            assert_eq!(decode_lifecycle_response(&bytes).unwrap(), response);
            assert!(super::super::v4::decode_lifecycle_response(&bytes).is_err());
            if let LifecycleResponse::Failure(failure) = response {
                assert_eq!(decode_response(&bytes).unwrap(), Response::Failure(failure));
            } else {
                assert!(decode_response(&bytes).is_err());
            }
        }
    }

    #[test]
    fn every_legal_snapshot_fits_the_frame_budget() {
        // Upper bound: every Session uses the larger popup representation and
        // every tab reserves both selections, even where they cannot coexist.
        let id = 2 + v2::MAX_ID_BYTES;
        let endpoint = 2 + v2::MAX_ENDPOINT_BYTES;
        let entry_bytes = 2 + MAX_ENTRY_ID_BYTES;
        let upper_bound = HEADER_BYTES
            + id
            + 8
            + 2
            + MAX_ENTRIES * (entry_bytes + 2 + MAX_LABEL_BYTES + 1 + 2 + MAX_KEY_BYTES)
            + 2
            + MAX_TABS * (id + 2 + MAX_DIRECTORY_BYTES + 1 + 2 * (1 + id) + 4)
            + MAX_SESSIONS * (id + entry_bytes + id + endpoint);
        let mut header = [0; HEADER_BYTES];
        header[8..12].copy_from_slice(&((upper_bound - HEADER_BYTES) as u32).to_le_bytes());
        assert_eq!(declared_message_len(&header).unwrap(), upper_bound);

        let identity = |prefix: &str, n: usize, length: usize| {
            let name = format!("{prefix}{n}");
            format!("{name}{}", "x".repeat(length - name.len()))
        };
        let mut state = snapshot();
        state.entries = (0..MAX_ENTRIES)
            .map(|n| PopupEntry {
                id: identity("entry", n, MAX_ENTRY_ID_BYTES),
                label: "x".repeat(MAX_LABEL_BYTES),
                shortcut: Shortcut {
                    modifiers: (n % 15 + 1) as u8,
                    key: ["BracketRight", "BracketLeft", "ArrowRight"][n / 15].into(),
                },
            })
            .collect();
        state.tabs = (0..MAX_TABS)
            .map(|n| {
                let mut surfaces = (n * 4..n * 4 + 4)
                    .map(|i| {
                        popup(
                            &identity("u", i, v2::MAX_ID_BYTES),
                            &state.entries[i % MAX_ENTRIES].id,
                        )
                    })
                    .collect::<Vec<_>>();
                for (i, surface) in surfaces.iter_mut().enumerate() {
                    surface.session = identity("s", n * 4 + i, v2::MAX_ID_BYTES);
                    surface.endpoint = vec![b'/'; v2::MAX_ENDPOINT_BYTES];
                    surface.endpoint[..2].copy_from_slice(&((n * 4 + i) as u16).to_le_bytes());
                }
                let first = surfaces.remove(0);
                Tab {
                    id: identity("t", n, v2::MAX_ID_BYTES),
                    directory: vec![b'/'; MAX_DIRECTORY_BYTES],
                    pending: false,
                    selected_pane: Some(first.id.clone()),
                    selected_popup: Some(surfaces[0].id.clone()),
                    panes: vec![Pane {
                        id: first.id,
                        session: first.session,
                        endpoint: first.endpoint,
                        live: true,
                    }],
                    popups: surfaces,
                }
            })
            .collect();
        state.active_tab = state.tabs[0].id.clone();
        let response = Response::Snapshot(state.clone());
        let bytes = encode_response(&response).unwrap();
        assert!(bytes.len() <= upper_bound);
        assert_eq!(declared_message_len(&bytes).unwrap(), bytes.len());
        assert_eq!(decode_response(&bytes).unwrap(), response);

        state.tabs[0]
            .popups
            .push(popup("extra", &state.entries[4].id));
        assert!(encode_response(&Response::Snapshot(state.clone())).is_err());
        let mut payload = v2::Encoder::default();
        encode_workspace(&mut payload, &state);
        let bytes = v2::frame_version(VERSION, v2::SNAPSHOT, payload.bytes).unwrap();
        assert!(decode_response(&bytes).is_err());
    }
}
