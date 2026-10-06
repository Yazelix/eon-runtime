//! Eon-owned values and bounded EONW v4 codec for local workspace clients.

use super::{v2, v3};
use std::collections::HashSet;

pub use super::v2::{
    Availability, Direction, Error, Failure, HEADER_BYTES, LifecycleResponse, MAX_DETAIL_BYTES,
    MAX_DIRECTORY_BYTES, MAX_PANES, MAX_TABS, Pane, Runtime, Stopped, declared_message_len,
};
pub use super::v3::{Action, DirectoryPicker, Request};

pub const VERSION: u16 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: String,
    pub directory: Vec<u8>,
    pub selected_pane: Option<String>,
    pub panes: Vec<Pane>,
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
    v3::encode_request_version(request, VERSION)
}

pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    v3::decode_request_version(bytes, VERSION)
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

fn encode_workspace(encoder: &mut v2::Encoder, snapshot: &Snapshot) {
    encoder.string(&snapshot.active_tab);
    encoder.count(snapshot.tabs.len());
    for tab in &snapshot.tabs {
        encoder.string(&tab.id);
        encoder.raw(&tab.directory);
        match &tab.selected_pane {
            Some(selected) => {
                encoder.byte(1);
                encoder.string(selected);
            }
            None => encoder.byte(0),
        }
        encoder.count(tab.panes.len());
        for pane in &tab.panes {
            encoder.string(&pane.id);
            encoder.string(&pane.session);
            encoder.raw(&pane.endpoint);
            encoder.byte(u8::from(pane.live));
        }
    }
    match &snapshot.directory_picker {
        Some(picker) => {
            encoder.byte(1);
            encoder.string(&picker.tab);
            encoder.raw(&picker.endpoint);
        }
        None => encoder.byte(0),
    }
}

fn decode_workspace(decoder: &mut v2::Decoder<'_>) -> Result<Snapshot> {
    let active_tab = decoder.string("active tab", v2::MAX_ID_BYTES)?;
    let tab_count = decoder.count("tabs", MAX_TABS)?;
    let mut tabs = Vec::with_capacity(tab_count);
    let mut pane_count = 0;
    for _ in 0..tab_count {
        let id = decoder.string("tab id", v2::MAX_ID_BYTES)?;
        let directory = decoder.raw("directory", MAX_DIRECTORY_BYTES)?.to_vec();
        let selected_pane = match decoder.byte()? {
            0 => None,
            1 => Some(decoder.string("selected pane", v2::MAX_ID_BYTES)?),
            _ => {
                return Err(Error::InvalidValue {
                    field: "selected pane presence",
                });
            }
        };
        let count = decoder.count("panes", MAX_PANES)?;
        pane_count += count;
        if pane_count > MAX_PANES {
            return Err(Error::InvalidSnapshot { field: "panes" });
        }
        let mut panes = Vec::with_capacity(count);
        for _ in 0..count {
            panes.push(Pane {
                id: decoder.string("pane id", v2::MAX_ID_BYTES)?,
                session: decoder.string("session id", v2::MAX_ID_BYTES)?,
                endpoint: decoder.raw("endpoint", v2::MAX_ENDPOINT_BYTES)?.to_vec(),
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
    Ok(Snapshot {
        active_tab,
        tabs,
        directory_picker,
    })
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    if snapshot.tabs.is_empty() || snapshot.tabs.len() > MAX_TABS {
        return Err(Error::InvalidSnapshot { field: "tabs" });
    }
    v2::identity("active tab", &snapshot.active_tab)?;
    if let Some(picker) = &snapshot.directory_picker {
        v2::identity("directory picker tab", &picker.tab)?;
        v2::validate_endpoint(&picker.endpoint, "directory picker endpoint")?;
    }

    let mut focus_ids = HashSet::new();
    let mut sessions = HashSet::new();
    let mut endpoints = HashSet::new();
    let mut pane_count = 0;
    let mut active_tab_present = false;
    for tab in &snapshot.tabs {
        v2::identity("tab id", &tab.id)?;
        v2::validate_directory(&tab.directory)?;
        if !focus_ids.insert(tab.id.as_str()) {
            return Err(Error::InvalidSnapshot { field: "tab id" });
        }
        active_tab_present |= tab.id == snapshot.active_tab;
        if tab.panes.is_empty() {
            if tab.selected_pane.is_some() {
                return Err(Error::InvalidSnapshot {
                    field: "selected_pane",
                });
            }
            if snapshot.directory_picker.as_ref().map(|picker| &picker.tab) != Some(&tab.id) {
                return Err(Error::InvalidSnapshot {
                    field: "pending tab",
                });
            }
            continue;
        }

        let selected = tab.selected_pane.as_ref().ok_or(Error::InvalidSnapshot {
            field: "selected_pane",
        })?;
        v2::identity("selected pane", selected)?;
        pane_count += tab.panes.len();
        if pane_count > MAX_PANES {
            return Err(Error::InvalidSnapshot { field: "panes" });
        }
        let mut selected_present = false;
        for pane in &tab.panes {
            v2::identity("pane id", &pane.id)?;
            v2::identity("session id", &pane.session)?;
            if !focus_ids.insert(pane.id.as_str()) {
                return Err(Error::InvalidSnapshot { field: "pane id" });
            }
            if !sessions.insert(pane.session.as_str()) {
                return Err(Error::InvalidSnapshot {
                    field: "session id",
                });
            }
            v2::validate_endpoint(&pane.endpoint, "endpoint")?;
            if !endpoints.insert(pane.endpoint.as_slice()) {
                return Err(Error::InvalidSnapshot { field: "endpoint" });
            }
            selected_present |= pane.id == *selected;
        }
        if !selected_present {
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
    if snapshot
        .directory_picker
        .as_ref()
        .is_some_and(|picker| !snapshot.tabs.iter().any(|tab| tab.id == picker.tab))
    {
        return Err(Error::InvalidSnapshot {
            field: "directory picker tab",
        });
    }
    if snapshot
        .directory_picker
        .as_ref()
        .is_some_and(|picker| endpoints.contains(picker.endpoint.as_slice()))
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

    fn pane(number: usize) -> Pane {
        Pane {
            id: format!("p{number}"),
            session: format!("session-{number}"),
            endpoint: format!("/run/eon/session-{number}.sock").into_bytes(),
            live: true,
        }
    }

    fn durable_tab(number: usize) -> Tab {
        Tab {
            id: format!("t{number}"),
            directory: format!("/tmp/tab-{number}").into_bytes(),
            selected_pane: Some(format!("p{number}")),
            panes: vec![pane(number)],
        }
    }

    fn pending_snapshot() -> Snapshot {
        Snapshot {
            active_tab: "t2".into(),
            tabs: vec![
                durable_tab(1),
                Tab {
                    id: "t2".into(),
                    directory: b"/tmp/tab-1".to_vec(),
                    selected_pane: None,
                    panes: Vec::new(),
                },
            ],
            directory_picker: Some(DirectoryPicker {
                tab: "t2".into(),
                endpoint: b"/run/eon/pick.sock".to_vec(),
            }),
        }
    }

    #[test]
    fn round_trips_picker_bound_to_inactive_tab() {
        let inactive_durable = Snapshot {
            active_tab: "t2".into(),
            tabs: vec![durable_tab(1), durable_tab(2)],
            directory_picker: Some(DirectoryPicker {
                tab: "t1".into(),
                endpoint: b"/run/eon/pick.sock".to_vec(),
            }),
        };
        let mut inactive_pending = pending_snapshot();
        inactive_pending.active_tab = "t1".into();

        for snapshot in [&inactive_durable, &inactive_pending] {
            let encoded = encode_response(&Response::Snapshot(snapshot.clone())).unwrap();
            assert_eq!(
                decode_response(&encoded).unwrap(),
                Response::Snapshot(snapshot.clone())
            );
        }

        let mut missing_picker_tab = inactive_durable.clone();
        missing_picker_tab.directory_picker.as_mut().unwrap().tab = "t3".into();
        assert_eq!(
            encode_response(&Response::Snapshot(missing_picker_tab)),
            Err(Error::InvalidSnapshot {
                field: "directory picker tab",
            })
        );
    }

    #[test]
    fn round_trips_picker_first_v4_without_widening_v3() {
        let request = Request {
            id: "venus-1".into(),
            action: Action::CreateTab,
        };
        assert_eq!(
            decode_request(&encode_request(&request).unwrap()).unwrap(),
            request
        );

        let pending_first = Response::Snapshot(Snapshot {
            active_tab: "t1".into(),
            tabs: vec![Tab {
                id: "t1".into(),
                directory: b"/tmp".to_vec(),
                selected_pane: None,
                panes: Vec::new(),
            }],
            directory_picker: Some(DirectoryPicker {
                tab: "t1".into(),
                endpoint: b"/run/eon/pick.sock".to_vec(),
            }),
        });
        let durable_picker = Response::Snapshot(Snapshot {
            active_tab: "t1".into(),
            tabs: vec![durable_tab(1)],
            directory_picker: Some(DirectoryPicker {
                tab: "t1".into(),
                endpoint: b"/run/eon/pick.sock".to_vec(),
            }),
        });
        for response in [
            pending_first,
            Response::Snapshot(pending_snapshot()),
            durable_picker,
            Response::Snapshot(Snapshot {
                active_tab: "t1".into(),
                tabs: vec![durable_tab(1)],
                directory_picker: None,
            }),
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

        let v4_request = encode_request(&Request {
            id: "venus-1".into(),
            action: Action::Inspect,
        })
        .unwrap();
        assert_eq!(
            v3::decode_request(&v4_request),
            Err(Error::UnsupportedVersion { version: VERSION })
        );
        let v3_request = v3::encode_request(&Request {
            id: "venus-1".into(),
            action: Action::Inspect,
        })
        .unwrap();
        assert_eq!(
            decode_request(&v3_request),
            Err(Error::UnsupportedVersion {
                version: v3::VERSION,
            })
        );

        let runtime = LifecycleResponse::Runtime(Runtime {
            generation: "g1-test".into(),
            eon_version: "0.1.0".into(),
            workspace_protocol: VERSION,
            component_report: "eon-alpha x86_64-linux".into(),
            sessions: Vec::new(),
            attach: Availability {
                available: true,
                reason: "compatible EONW v4 supervisor".into(),
            },
            stop: Availability {
                available: true,
                reason: "generation-aware supervisor".into(),
            },
        });
        assert_eq!(
            v3::encode_lifecycle_response(&runtime),
            Err(Error::InvalidValue { field: "sessions" })
        );
        let encoded_runtime = encode_lifecycle_response(&runtime).unwrap();
        assert_eq!(
            decode_lifecycle_response(&encoded_runtime).unwrap(),
            runtime
        );
        assert_eq!(
            v3::decode_lifecycle_response(&encoded_runtime),
            Err(Error::UnsupportedVersion { version: VERSION })
        );
        let stopped = LifecycleResponse::Stopped(Stopped {
            generation: "g1-test".into(),
            sessions: Vec::new(),
        });
        assert_eq!(
            decode_lifecycle_response(&encode_lifecycle_response(&stopped).unwrap()).unwrap(),
            stopped
        );

        let mut no_picker = pending_snapshot();
        no_picker.directory_picker = None;
        assert_eq!(
            encode_response(&Response::Snapshot(no_picker)),
            Err(Error::InvalidSnapshot {
                field: "pending tab",
            })
        );
        let mut selected_pending = pending_snapshot();
        selected_pending.tabs[1].selected_pane = Some("p2".into());
        assert_eq!(
            encode_response(&Response::Snapshot(selected_pending)),
            Err(Error::InvalidSnapshot {
                field: "selected_pane",
            })
        );
        let mut unselected_durable = pending_snapshot();
        unselected_durable.tabs[0].selected_pane = None;
        assert_eq!(
            encode_response(&Response::Snapshot(unselected_durable)),
            Err(Error::InvalidSnapshot {
                field: "selected_pane",
            })
        );
        let mut unbound_pending = pending_snapshot();
        unbound_pending.active_tab = "t1".into();
        unbound_pending.directory_picker = Some(DirectoryPicker {
            tab: "t1".into(),
            endpoint: b"/run/eon/pick.sock".to_vec(),
        });
        assert_eq!(
            encode_response(&Response::Snapshot(unbound_pending)),
            Err(Error::InvalidSnapshot {
                field: "pending tab",
            })
        );
        let mut aliased = pending_snapshot();
        aliased.directory_picker.as_mut().unwrap().endpoint =
            aliased.tabs[0].panes[0].endpoint.clone();
        assert_eq!(
            encode_response(&Response::Snapshot(aliased)),
            Err(Error::InvalidSnapshot {
                field: "directory picker endpoint",
            })
        );

        let encoded_pending = encode_response(&Response::Snapshot(pending_snapshot())).unwrap();
        assert_eq!(
            v3::decode_response(&encoded_pending),
            Err(Error::UnsupportedVersion { version: VERSION })
        );
        let encoded_v3 = v3::encode_response(&v3::Response::Snapshot(v3::Snapshot {
            active_tab: "t1".into(),
            tabs: vec![v2::Tab {
                id: "t1".into(),
                directory: b"/tmp".to_vec(),
                selected_pane: "p1".into(),
                panes: vec![pane(1)],
            }],
            directory_picker: None,
        }))
        .unwrap();
        assert_eq!(
            decode_response(&encoded_v3),
            Err(Error::UnsupportedVersion {
                version: v3::VERSION,
            })
        );

        let identity = |prefix: &str, number: usize| {
            let suffix = number.to_string();
            format!(
                "{prefix}{}{suffix}",
                "x".repeat(v2::MAX_ID_BYTES - prefix.len() - suffix.len())
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
                        selected_pane: Some(identity("p", first_pane)),
                        panes: (first_pane..first_pane + panes_per_tab)
                            .map(|pane_number| Pane {
                                id: identity("p", pane_number),
                                session: identity("session-", pane_number),
                                endpoint: vec![pane_number as u8; v2::MAX_ENDPOINT_BYTES],
                                live: true,
                            })
                            .collect(),
                    }
                })
                .collect(),
            directory_picker: Some(DirectoryPicker {
                tab: identity("t", 1),
                endpoint: (0..v2::MAX_ENDPOINT_BYTES)
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

    #[test]
    fn round_trips_v4_movement_without_widening_v3() {
        for (direction, tag) in [
            (Direction::Left, 14),
            (Direction::Right, 15),
            (Direction::Up, 16),
            (Direction::Down, 17),
        ] {
            let request = Request {
                id: "move-1".into(),
                action: Action::Move(direction),
            };
            let mut payload = v2::Encoder::default();
            payload.string(&request.id);
            payload.byte(tag);
            let encoded = v2::frame_version(VERSION, v2::REQUEST, payload.bytes.clone()).unwrap();
            assert_eq!(encode_request(&request).unwrap(), encoded);
            assert_eq!(decode_request(&encoded).unwrap(), request);
            assert_eq!(
                v3::encode_request(&request),
                Err(Error::InvalidValue { field: "action" })
            );
            let encoded = v2::frame_version(v3::VERSION, v2::REQUEST, payload.bytes).unwrap();
            assert_eq!(
                v3::decode_request(&encoded),
                Err(Error::InvalidTag {
                    field: "action",
                    value: tag,
                })
            );
        }
    }

    #[test]
    fn round_trips_v4_tab_close_without_widening_v3() {
        let request = Request {
            id: "close-1".into(),
            action: Action::CloseTab { tab: "t2".into() },
        };
        let mut payload = v2::Encoder::default();
        payload.string(&request.id);
        payload.byte(18);
        payload.string("t2");
        let encoded = v2::frame_version(VERSION, v2::REQUEST, payload.bytes.clone()).unwrap();
        assert_eq!(encode_request(&request).unwrap(), encoded);
        assert_eq!(decode_request(&encoded).unwrap(), request);
        assert_eq!(
            v3::encode_request(&request),
            Err(Error::InvalidValue { field: "action" })
        );
        let encoded = v2::frame_version(v3::VERSION, v2::REQUEST, payload.bytes).unwrap();
        assert_eq!(
            v3::decode_request(&encoded),
            Err(Error::InvalidTag {
                field: "action",
                value: 18,
            })
        );
    }
}
