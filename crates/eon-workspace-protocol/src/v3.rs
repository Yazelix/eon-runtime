//! Eon-owned values and bounded EONW v3 codec for local workspace clients.

use super::v2::{self, Decoder, Encoder};

pub use super::v2::{
    Availability, Direction, Error, Failure, HEADER_BYTES, LifecycleResponse, MAX_DETAIL_BYTES,
    MAX_DIRECTORY_BYTES, MAX_PANES, MAX_TABS, Pane, Runtime, Stopped, Tab, declared_message_len,
};

pub const VERSION: u16 = 3;

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
    Move(Direction),
    CloseTab { tab: String },
    Stop { generation: String },
    SetTabDirectory { tab: String, directory: Vec<u8> },
    PickTabDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: String,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPicker {
    pub tab: String,
    pub endpoint: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub active_tab: String,
    pub tabs: Vec<Tab>,
    pub directory_picker: Option<DirectoryPicker>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Snapshot(Snapshot),
    Failure(Failure),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn encode_request(request: &Request) -> Result<Vec<u8>> {
    encode_request_version(request, VERSION)
}

pub(crate) fn encode_request_version(request: &Request, version: u16) -> Result<Vec<u8>> {
    v2::identity("request id", &request.id)?;
    let mut payload = Encoder::default();
    payload.string(&request.id);
    match &request.action {
        Action::Inspect => payload.byte(0),
        Action::CreateTab => payload.byte(1),
        Action::CreatePane => payload.byte(2),
        Action::FocusId(id) => {
            v2::nonempty("focus id", id, v2::MAX_ID_BYTES)?;
            payload.byte(3);
            payload.string(id);
        }
        Action::Focus(Direction::Left) => payload.byte(4),
        Action::Focus(Direction::Right) => payload.byte(5),
        Action::Focus(Direction::Up) => payload.byte(6),
        Action::Focus(Direction::Down) => payload.byte(7),
        Action::Move(direction) if version > VERSION => payload.byte(match direction {
            Direction::Left => 14,
            Direction::Right => 15,
            Direction::Up => 16,
            Direction::Down => 17,
        }),
        Action::Move(_) => return Err(Error::InvalidValue { field: "action" }),
        Action::CloseTab { tab } if version > VERSION => {
            v2::identity("tab id", tab)?;
            payload.byte(18);
            payload.string(tab);
        }
        Action::CloseTab { .. } => return Err(Error::InvalidValue { field: "action" }),
        Action::InspectRuntime => payload.byte(8),
        Action::Stop { generation } => {
            v2::identity("generation", generation)?;
            payload.byte(9);
            payload.string(generation);
        }
        Action::Present { workspace } => {
            payload.byte(10);
            payload.byte(u8::from(*workspace));
        }
        Action::InspectPresentation => payload.byte(11),
        Action::SetTabDirectory { tab, directory } => {
            v2::identity("tab id", tab)?;
            v2::validate_directory(directory)?;
            payload.byte(12);
            payload.string(tab);
            payload.raw(directory);
        }
        Action::PickTabDirectory => payload.byte(13),
    }
    v2::frame_version(version, v2::REQUEST, payload.bytes)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    decode_request_version(bytes, VERSION)
}

pub(crate) fn decode_request_version(bytes: &[u8], version: u16) -> Result<Request> {
    let (kind, payload) = v2::unframe_version(bytes, version)?;
    if kind != v2::REQUEST {
        return Err(Error::InvalidKind { value: kind });
    }
    let mut decoder = Decoder::new(payload);
    let id = decoder.string("request id", v2::MAX_ID_BYTES)?;
    v2::identity("request id", &id)?;
    let action = match decoder.byte()? {
        0 => Action::Inspect,
        1 => Action::CreateTab,
        2 => Action::CreatePane,
        3 => {
            let id = decoder.string("focus id", v2::MAX_ID_BYTES)?;
            v2::nonempty("focus id", &id, v2::MAX_ID_BYTES)?;
            Action::FocusId(id)
        }
        4 => Action::Focus(Direction::Left),
        5 => Action::Focus(Direction::Right),
        6 => Action::Focus(Direction::Up),
        7 => Action::Focus(Direction::Down),
        8 => Action::InspectRuntime,
        9 => {
            let generation = decoder.string("generation", v2::MAX_ID_BYTES)?;
            v2::identity("generation", &generation)?;
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
            let tab = decoder.string("tab id", v2::MAX_ID_BYTES)?;
            v2::identity("tab id", &tab)?;
            let directory = decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec();
            v2::validate_directory(&directory)?;
            Action::SetTabDirectory { tab, directory }
        }
        13 => Action::PickTabDirectory,
        14 if version > VERSION => Action::Move(Direction::Left),
        15 if version > VERSION => Action::Move(Direction::Right),
        16 if version > VERSION => Action::Move(Direction::Up),
        17 if version > VERSION => Action::Move(Direction::Down),
        18 if version > VERSION => {
            let tab = decoder.string("tab id", v2::MAX_ID_BYTES)?;
            v2::identity("tab id", &tab)?;
            Action::CloseTab { tab }
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
            v2::encode_workspace(&mut payload, &snapshot.active_tab, &snapshot.tabs);
            match &snapshot.directory_picker {
                Some(picker) => {
                    payload.byte(1);
                    payload.string(&picker.tab);
                    payload.raw(&picker.endpoint);
                }
                None => payload.byte(0),
            }
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
    let mut decoder = Decoder::new(payload);
    let response = match kind {
        v2::SNAPSHOT => {
            let (active_tab, tabs) = v2::decode_workspace(&mut decoder)?;
            let directory_picker = match decoder.byte()? {
                0 => None,
                1 => Some(DirectoryPicker {
                    tab: decoder.string("directory picker tab", v2::MAX_ID_BYTES)?,
                    endpoint: decoder
                        .raw("directory picker endpoint", v2::MAX_ENDPOINT_BYTES)?
                        .to_vec(),
                }),
                _ => {
                    return Err(Error::InvalidValue {
                        field: "directory picker presence",
                    });
                }
            };
            let snapshot = Snapshot {
                active_tab,
                tabs,
                directory_picker,
            };
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
    super::v2::encode_lifecycle_response_version(response, VERSION, false)
}

pub fn decode_lifecycle_response(bytes: &[u8]) -> Result<LifecycleResponse> {
    super::v2::decode_lifecycle_response_version(bytes, VERSION, false)
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    v2::validate_workspace(&snapshot.active_tab, &snapshot.tabs)?;
    let Some(picker) = &snapshot.directory_picker else {
        return Ok(());
    };
    v2::identity("directory picker tab", &picker.tab)?;
    if picker.tab != snapshot.active_tab {
        return Err(Error::InvalidSnapshot {
            field: "directory picker tab",
        });
    }
    v2::validate_endpoint(&picker.endpoint, "directory picker endpoint")?;
    if snapshot
        .tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .any(|pane| pane.endpoint == picker.endpoint)
    {
        return Err(Error::InvalidSnapshot {
            field: "directory picker endpoint",
        });
    }
    Ok(())
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
            directory_picker: Some(DirectoryPicker {
                tab: "t1".into(),
                endpoint: b"/run/eon/picker.sock".to_vec(),
            }),
        }
    }

    #[test]
    fn round_trips_v3_picker_without_widening_v2() {
        let actions = [
            Action::Inspect,
            Action::InspectRuntime,
            Action::InspectPresentation,
            Action::Present { workspace: true },
            Action::CreateTab,
            Action::CreatePane,
            Action::FocusId("p1".into()),
            Action::Focus(Direction::Left),
            Action::Focus(Direction::Right),
            Action::Focus(Direction::Up),
            Action::Focus(Direction::Down),
            Action::Stop {
                generation: "g1-test".into(),
            },
            Action::SetTabDirectory {
                tab: "t1".into(),
                directory: b"/tmp/eon-\xff".to_vec(),
            },
            Action::PickTabDirectory,
        ];
        for action in actions {
            let request = Request {
                id: "venus-1".into(),
                action,
            };
            assert_eq!(
                decode_request(&encode_request(&request).unwrap()).unwrap(),
                request
            );
        }

        let encoded_request = encode_request(&Request {
            id: "venus-1".into(),
            action: Action::PickTabDirectory,
        })
        .unwrap();
        assert_eq!(
            super::super::v2::decode_request(&encoded_request),
            Err(Error::UnsupportedVersion { version: VERSION })
        );

        let v2_request = super::super::v2::encode_request(&super::super::v2::Request {
            id: "venus-1".into(),
            action: super::super::v2::Action::Inspect,
        })
        .unwrap();
        assert_eq!(
            decode_request(&v2_request),
            Err(Error::UnsupportedVersion {
                version: super::super::v2::VERSION,
            })
        );

        for response in [
            Response::Snapshot(Snapshot {
                directory_picker: None,
                ..snapshot()
            }),
            Response::Snapshot(snapshot()),
            Response::Failure(Failure {
                code: "picker-unavailable".into(),
                detail: "directory picker is unavailable".into(),
            }),
        ] {
            assert_eq!(
                decode_response(&encode_response(&response).unwrap()).unwrap(),
                response
            );
        }

        let runtime = LifecycleResponse::Runtime(Runtime {
            generation: "g1-test".into(),
            eon_version: "0.1.0".into(),
            workspace_protocol: VERSION,
            component_report: "eon-alpha x86_64-linux".into(),
            sessions: vec!["session-1".into()],
            attach: Availability {
                available: true,
                reason: "compatible EONW v3 supervisor".into(),
            },
            stop: Availability {
                available: true,
                reason: "generation-aware supervisor".into(),
            },
        });
        let encoded_runtime = encode_lifecycle_response(&runtime).unwrap();
        assert_eq!(
            decode_lifecycle_response(&encoded_runtime).unwrap(),
            runtime
        );
        assert_eq!(
            super::super::v2::decode_lifecycle_response(&encoded_runtime),
            Err(Error::UnsupportedVersion { version: VERSION })
        );

        for (tab, endpoint, error) in [
            (
                "t2",
                b"/run/eon/picker.sock".to_vec(),
                Error::InvalidSnapshot {
                    field: "directory picker tab",
                },
            ),
            (
                "t1",
                b"/run/eon/orbit.sock".to_vec(),
                Error::InvalidSnapshot {
                    field: "directory picker endpoint",
                },
            ),
            (
                "t1",
                Vec::new(),
                Error::InvalidSnapshot {
                    field: "directory picker endpoint",
                },
            ),
            (
                "t1",
                vec![b'x'; v2::MAX_ENDPOINT_BYTES + 1],
                Error::FieldTooLong {
                    field: "directory picker endpoint",
                    length: v2::MAX_ENDPOINT_BYTES + 1,
                },
            ),
        ] {
            let mut invalid = snapshot();
            invalid.directory_picker = Some(DirectoryPicker {
                tab: tab.into(),
                endpoint,
            });
            assert_eq!(encode_response(&Response::Snapshot(invalid)), Err(error));
        }

        let identity = |prefix: &str, number: usize| {
            let suffix = number.to_string();
            format!(
                "{prefix}{}{suffix}",
                "x".repeat(super::super::v2::MAX_ID_BYTES - prefix.len() - suffix.len())
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
                                endpoint: vec![
                                    pane_number as u8;
                                    super::super::v2::MAX_ENDPOINT_BYTES
                                ],
                                live: true,
                            })
                            .collect(),
                    }
                })
                .collect(),
            directory_picker: Some(DirectoryPicker {
                tab: identity("t", 1),
                endpoint: (0..super::super::v2::MAX_ENDPOINT_BYTES)
                    .map(|index| (index % 251) as u8)
                    .collect(),
            }),
        });
        let encoded_maximal = encode_response(&maximal).unwrap();
        assert_eq!(
            declared_message_len(&encoded_maximal).unwrap(),
            encoded_maximal.len()
        );
        assert_eq!(decode_response(&encoded_maximal).unwrap(), maximal);
    }
}
