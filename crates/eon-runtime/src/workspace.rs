use super::managed_environment::{PopupCatalog, PopupCommand, PopupDefinition};
use eon_workspace_protocol::v7::{
    Action, Direction, Failure, InvokeIntent, MAX_DIRECTORY_BYTES, MAX_SESSIONS, MAX_TABS,
    Pane as SnapshotPane, Popup as SnapshotPopup, PopupEntry as SnapshotEntry, PopupTarget,
    Snapshot, Tab as SnapshotTab, WorkspaceAction,
};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

const MAX_RECENT_REQUESTS: usize = 256;

pub(crate) fn is_directory_picker_session(id: &str) -> bool {
    id.strip_prefix("directory-picker-")
        .and_then(canonical_number)
        .is_some()
}

pub(crate) fn is_directory_picker_endpoint(name: &str) -> bool {
    name == "pick.sock"
        || name
            .strip_prefix('k')
            .and_then(|name| name.strip_suffix(".sock"))
            .and_then(canonical_number)
            .is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) endpoint: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchCommand {
    Pane,
    Project { tab: String, instance: String },
    Tool(Vec<OsString>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionOperation {
    Start {
        session: Session,
        directory: PathBuf,
        command: LaunchCommand,
    },
    Stop(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pane {
    id: String,
    session: Session,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Popup {
    id: String,
    entry: String,
    session: Session,
    launch_directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tab {
    id: String,
    directory: PathBuf,
    pending: bool,
    previous_tab: Option<String>,
    panes: Vec<Pane>,
    selected_pane: Option<usize>,
    popups: Vec<Popup>,
    selected_popup: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Workspace {
    runtime: PathBuf,
    catalog: PopupCatalog,
    tabs: Vec<Tab>,
    active: usize,
    next_tab: usize,
    next_pane: usize,
    next_session: usize,
    next_popup: usize,
    recent_requests: VecDeque<String>,
}

impl Workspace {
    pub(crate) fn pending(
        runtime: PathBuf,
        directory: PathBuf,
        catalog: PopupCatalog,
        mut prepare: impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        mut operate: impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut workspace = Self {
            runtime,
            catalog,
            tabs: vec![Tab {
                id: "t1".into(),
                directory,
                pending: true,
                previous_tab: None,
                panes: Vec::new(),
                selected_pane: None,
                popups: Vec::new(),
                selected_popup: None,
            }],
            active: 0,
            next_tab: 2,
            next_pane: 1,
            next_session: 1,
            next_popup: 1,
            recent_requests: VecDeque::new(),
        };
        if workspace
            .open_popup(0, "project", &mut prepare, &mut operate, true)
            .is_err()
        {
            workspace.tabs[0].pending = false;
            workspace.start_pane(0, &mut operate)?;
        }
        Ok(workspace)
    }

    pub(crate) fn with_recovered_sessions(
        runtime: PathBuf,
        directory: PathBuf,
        sessions: Vec<(usize, Session)>,
        catalog: PopupCatalog,
    ) -> Result<Self, String> {
        let next_session = sessions
            .last()
            .ok_or("cannot recover an empty workspace")?
            .0
            .checked_add(1)
            .ok_or("recovered Session identity leaves no next Session identity")?;
        let panes = sessions
            .into_iter()
            .map(|(number, session)| Pane {
                id: format!("p{number}"),
                session,
            })
            .collect::<Vec<_>>();
        Ok(Self {
            runtime,
            catalog,
            tabs: vec![Tab {
                id: "t1".into(),
                directory,
                pending: false,
                previous_tab: None,
                selected_pane: Some(0),
                panes,
                popups: Vec::new(),
                selected_popup: None,
            }],
            active: 0,
            next_tab: 2,
            next_pane: next_session,
            next_session,
            next_popup: 1,
            recent_requests: VecDeque::new(),
        })
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            active_tab: self.tabs[self.active].id.clone(),
            geometry: self.catalog.geometry,
            entries: self
                .catalog
                .entries
                .iter()
                .map(|entry| SnapshotEntry {
                    id: entry.id.clone(),
                    label: entry.label.clone(),
                    shortcut: entry.shortcut.clone(),
                })
                .collect(),
            tabs: self
                .tabs
                .iter()
                .map(|tab| SnapshotTab {
                    id: tab.id.clone(),
                    directory: tab.directory.as_os_str().as_bytes().to_vec(),
                    pending: tab.pending,
                    selected_pane: tab
                        .selected_pane
                        .map(|selected| tab.panes[selected].id.clone()),
                    selected_popup: tab.selected_popup.clone(),
                    panes: tab
                        .panes
                        .iter()
                        .map(|pane| SnapshotPane {
                            id: pane.id.clone(),
                            session: pane.session.id.clone(),
                            endpoint: pane.session.endpoint.as_os_str().as_bytes().to_vec(),
                            live: true,
                        })
                        .collect(),
                    popups: tab
                        .popups
                        .iter()
                        .map(|popup| SnapshotPopup {
                            id: popup.id.clone(),
                            entry: popup.entry.clone(),
                            session: popup.session.id.clone(),
                            endpoint: popup.session.endpoint.as_os_str().as_bytes().to_vec(),
                        })
                        .collect(),
                })
                .collect(),
            codex_quota: None,
        }
    }

    pub(crate) fn dispatch(
        &mut self,
        request_id: &str,
        action: Action,
        mut prepare: impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        mut operate: impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        self.reject_duplicate(request_id)?;
        self.snapshot()
            .check_popup_action(&action)
            .map_err(|error| action_error("stale-popup", error.to_string()))?;
        match action {
            Action::Workspace(action) => {
                self.dispatch_workspace(action, &mut prepare, &mut operate)?
            }
            Action::InvokePopup {
                tab, entry, intent, ..
            } => self.invoke_popup(&tab, &entry, intent, &mut prepare, &mut operate)?,
            Action::DismissPopup(target) => self.dismiss_popup(&target, &mut operate)?,
            Action::CommitDirectory { target, directory } => {
                self.commit_directory(&target, directory, &mut operate)?
            }
        }
        self.remember(request_id);
        Ok(())
    }

    fn dispatch_workspace(
        &mut self,
        action: WorkspaceAction,
        prepare: &mut impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        match action {
            WorkspaceAction::Inspect => {}
            WorkspaceAction::InspectRuntime
            | WorkspaceAction::InspectPresentation
            | WorkspaceAction::Present { .. }
            | WorkspaceAction::CloseTab { .. }
            | WorkspaceAction::Stop { .. } => {
                return Err(action_error(
                    "unavailable",
                    "supervisor lifecycle actions do not mutate workspace topology",
                ));
            }
            WorkspaceAction::CreateTab => self.create_tab(prepare, operate)?,
            WorkspaceAction::CreatePane => {
                self.require_established(self.active)?;
                self.dismiss_for_pane_action(self.active, operate)?;
                self.start_pane(self.active, operate)
                    .map_err(|detail| action_error("session-start", detail))?;
            }
            WorkspaceAction::FocusId(id) => self.focus_id(&id, operate)?,
            WorkspaceAction::Focus(direction) => self.focus_direction(direction, operate)?,
            WorkspaceAction::Move(direction) => self.move_direction(direction, operate)?,
            WorkspaceAction::SetTabDirectory { tab, directory } => {
                self.set_tab_directory(&tab, directory)?;
            }
            WorkspaceAction::PickTabDirectory => {
                return Err(action_error(
                    "unavailable",
                    "the EONW v7 Project entry replaces the retired picker action",
                ));
            }
        }
        Ok(())
    }

    fn create_tab(
        &mut self,
        prepare: &mut impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        if self.tabs.len() >= MAX_TABS || self.next_tab == usize::MAX {
            return Err(action_error(
                "capacity",
                format!("workspace is limited to {MAX_TABS} tabs"),
            ));
        }
        let previous_tab = self.tabs[self.active].id.clone();
        let id = format!("t{}", self.next_tab);
        let directory = self.tabs[self.active].directory.clone();
        self.tabs.push(Tab {
            id,
            directory,
            pending: true,
            previous_tab: Some(previous_tab),
            panes: Vec::new(),
            selected_pane: None,
            popups: Vec::new(),
            selected_popup: None,
        });
        let index = self.tabs.len() - 1;
        if let Err(error) = self.open_popup(index, "project", prepare, operate, true) {
            self.tabs.pop();
            return Err(error);
        }
        self.next_tab += 1;
        self.active = index;
        Ok(())
    }

    fn invoke_popup(
        &mut self,
        tab: &str,
        entry: &str,
        intent: InvokeIntent,
        prepare: &mut impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        let tab_index = self
            .tabs
            .iter()
            .position(|item| item.id == tab)
            .ok_or_else(|| action_error("unknown-tab", format!("tab {tab} is not live")))?;
        if tab_index != self.active {
            return Err(action_error(
                "not-active",
                "popup target is not the active tab",
            ));
        }
        if self.tabs[tab_index].pending && entry != "project" {
            return Err(action_error(
                "choose-directory-first",
                "choose the pending tab directory before opening a tool",
            ));
        }
        let definition = self.definition(entry)?.clone();
        if let Some(index) = self.tabs[tab_index]
            .popups
            .iter()
            .position(|popup| popup.entry == entry)
        {
            let popup = self.tabs[tab_index].popups[index].clone();
            if popup.launch_directory != self.tabs[tab_index].directory {
                let candidate = self.popup_candidate(tab_index, &definition, true)?;
                let launch = self.prepare_launch(tab_index, &candidate, &definition, prepare)?;
                if let Some(index) = self.displaced_transient(tab_index, Some(&popup.id))? {
                    self.stop_popup(tab_index, index, operate)?;
                }
                let index = self.tabs[tab_index]
                    .popups
                    .iter()
                    .position(|item| item.id == popup.id)
                    .expect("the popup being replaced remains live");
                self.stop_popup(tab_index, index, operate)?;
                if let Err(detail) = operate(launch) {
                    self.remove_empty_tab(tab_index);
                    return Err(action_error("popup-start", detail));
                }
                self.commit_popup(tab_index, candidate);
                return Ok(());
            }
            let selected = self.tabs[tab_index].selected_popup.as_deref() == Some(&popup.id);
            if intent == InvokeIntent::Toggle && selected {
                return self.dismiss_popup(
                    &PopupTarget {
                        tab: tab.into(),
                        instance: popup.id.clone(),
                    },
                    operate,
                );
            }
            if let Some(index) = self.displaced_transient(tab_index, Some(&popup.id))? {
                self.stop_popup(tab_index, index, operate)?;
            }
            self.tabs[tab_index].selected_popup = Some(popup.id.clone());
            return Ok(());
        }
        self.open_popup(tab_index, entry, prepare, operate, false)
    }

    fn open_popup(
        &mut self,
        tab_index: usize,
        entry: &str,
        prepare: &mut impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
        pending: bool,
    ) -> Result<(), Failure> {
        let definition = self.definition(entry)?.clone();
        let displaced = self.displaced_transient(tab_index, None)?;
        let stops_selected = displaced.is_some();
        let candidate = self.popup_candidate(tab_index, &definition, stops_selected)?;
        let launch = self.prepare_launch(tab_index, &candidate, &definition, prepare)?;
        if let Some(index) = displaced {
            self.stop_popup(tab_index, index, operate)?;
        }
        if let Err(detail) = operate(launch) {
            if stops_selected {
                self.remove_empty_tab(tab_index);
            }
            return Err(action_error("popup-start", detail));
        }
        self.commit_popup(tab_index, candidate);
        self.tabs[tab_index].pending = pending;
        Ok(())
    }

    fn displaced_transient(
        &self,
        tab_index: usize,
        retained: Option<&str>,
    ) -> Result<Option<usize>, Failure> {
        let tab = &self.tabs[tab_index];
        let Some(selected) = tab
            .selected_popup
            .as_deref()
            .filter(|id| retained != Some(*id))
        else {
            return Ok(None);
        };
        let index = tab
            .popups
            .iter()
            .position(|popup| popup.id == selected)
            .expect("selected popup is live");
        let popup = &tab.popups[index];
        Ok((!self.definition(&popup.entry)?.keep_alive).then_some(index))
    }

    fn popup_candidate(
        &self,
        tab_index: usize,
        definition: &PopupDefinition,
        replacing: bool,
    ) -> Result<Popup, Failure> {
        if !replacing && self.session_count() >= MAX_SESSIONS {
            return Err(action_error(
                "capacity",
                format!("workspace is limited to {MAX_SESSIONS} Sessions"),
            ));
        }
        if self.next_popup == usize::MAX || self.next_session == usize::MAX {
            return Err(action_error(
                "capacity",
                "workspace exhausted Session identities",
            ));
        }
        let project = matches!(definition.command, PopupCommand::Project);
        let number = if project {
            self.next_popup
        } else {
            self.next_session
        };
        let session_id = if project {
            format!("directory-picker-{number}")
        } else {
            format!("session-{number}")
        };
        Ok(Popup {
            id: format!("u{}", self.next_popup),
            entry: definition.id.clone(),
            session: Session {
                id: session_id.clone(),
                endpoint: self.runtime.join(if project {
                    format!("k{number}.sock")
                } else {
                    format!("{session_id}.sock")
                }),
            },
            launch_directory: self.tabs[tab_index].directory.clone(),
        })
    }

    fn prepare_launch(
        &self,
        tab_index: usize,
        popup: &Popup,
        definition: &PopupDefinition,
        prepare: &mut impl FnMut(&PopupCommand, &Path) -> Result<Vec<OsString>, String>,
    ) -> Result<SessionOperation, Failure> {
        let command = match &definition.command {
            PopupCommand::Project => LaunchCommand::Project {
                tab: self.tabs[tab_index].id.clone(),
                instance: popup.id.clone(),
            },
            command => LaunchCommand::Tool(
                prepare(command, &popup.launch_directory)
                    .map_err(|detail| action_error("popup-command", detail))?,
            ),
        };
        Ok(SessionOperation::Start {
            session: popup.session.clone(),
            directory: popup.launch_directory.clone(),
            command,
        })
    }

    fn commit_popup(&mut self, tab_index: usize, popup: Popup) {
        let project = is_directory_picker_session(&popup.session.id);
        self.tabs[tab_index].selected_popup = Some(popup.id.clone());
        self.tabs[tab_index].popups.push(popup);
        self.next_popup += 1;
        if !project {
            self.next_session += 1;
        }
    }

    fn dismiss_popup(
        &mut self,
        target: &PopupTarget,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.id == target.tab)
            .ok_or_else(|| action_error("stale-popup", "popup tab is no longer live"))?;
        let popup_index = self.tabs[tab_index]
            .popups
            .iter()
            .position(|popup| popup.id == target.instance)
            .ok_or_else(|| action_error("stale-popup", "popup instance is no longer live"))?;
        let popup = &self.tabs[tab_index].popups[popup_index];
        if self.definition(&popup.entry)?.keep_alive {
            if self.tabs[tab_index].selected_popup.as_deref() == Some(&popup.id) {
                self.tabs[tab_index].selected_popup = None;
            }
            return Ok(());
        }
        let project = popup.entry == "project";
        self.stop_popup(tab_index, popup_index, operate)?;
        if project && self.tabs[tab_index].pending {
            self.cancel_pending_tab(tab_index, operate)?;
        } else {
            self.remove_empty_tab(tab_index);
        }
        Ok(())
    }

    fn commit_directory(
        &mut self,
        target: &PopupTarget,
        directory: Vec<u8>,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.id == target.tab)
            .ok_or_else(|| action_error("stale-popup", "chooser tab is no longer live"))?;
        let directory = validated_directory(directory)?;
        if self.tabs[tab_index].pending {
            self.start_pane_at(tab_index, &directory, operate)
                .map_err(|detail| action_error("session-start", detail))?;
            self.tabs[tab_index].directory = directory;
            self.tabs[tab_index].pending = false;
            self.tabs[tab_index].previous_tab = None;
        } else {
            self.tabs[tab_index].directory = directory;
        }
        Ok(())
    }

    pub(crate) fn session_exited(
        &mut self,
        session_id: &str,
        mut operate: impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        if let Some((tab_index, popup_index)) =
            self.tabs.iter().enumerate().find_map(|(tab, item)| {
                item.popups
                    .iter()
                    .position(|popup| popup.session.id == session_id)
                    .map(|popup| (tab, popup))
            })
        {
            let project = self.tabs[tab_index].popups[popup_index].entry == "project";
            self.remove_popup(tab_index, popup_index);
            if project && self.tabs[tab_index].pending {
                self.cancel_pending_tab(tab_index, &mut operate)?;
            } else {
                self.remove_empty_tab(tab_index);
            }
            return Ok(());
        }
        let (tab_index, pane_index) = self
            .tabs
            .iter()
            .enumerate()
            .find_map(|(tab, item)| {
                item.panes
                    .iter()
                    .position(|pane| pane.session.id == session_id)
                    .map(|pane| (tab, pane))
            })
            .ok_or_else(|| {
                action_error(
                    "unknown-session",
                    format!("session {session_id} is not in the workspace"),
                )
            })?;
        let tab = &mut self.tabs[tab_index];
        tab.panes.remove(pane_index);
        tab.selected_pane = if tab.panes.is_empty() {
            None
        } else {
            Some(index_after_removal(
                tab.selected_pane.expect("nonempty tab has a selected pane"),
                pane_index,
                tab.panes.len(),
            ))
        };
        self.remove_empty_tab(tab_index);
        Ok(())
    }

    pub(crate) fn transient_sessions(&self) -> Vec<String> {
        self.tabs
            .iter()
            .flat_map(|tab| &tab.popups)
            .filter(|popup| popup.entry == "project")
            .map(|popup| popup.session.id.clone())
            .collect()
    }

    pub(crate) fn prepare_close_tab(
        &mut self,
        request_id: &str,
        id: &str,
    ) -> Result<Vec<String>, Failure> {
        self.reject_duplicate(request_id)?;
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or_else(|| action_error("unknown-tab", format!("tab {id} is not live")))?;
        if index != self.active {
            return Err(action_error(
                "not-active",
                format!("tab {id} is not the active tab"),
            ));
        }
        if self.tabs.len() == 1 {
            return Err(action_error(
                "unavailable",
                "the final tab cannot be closed",
            ));
        }
        let sessions = self.tabs[index]
            .panes
            .iter()
            .map(|pane| pane.session.id.clone())
            .chain(
                self.tabs[index]
                    .popups
                    .iter()
                    .map(|popup| popup.session.id.clone()),
            )
            .collect();
        self.remember(request_id);
        Ok(sessions)
    }

    fn start_pane(
        &mut self,
        tab_index: usize,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), String> {
        let directory = self.tabs[tab_index].directory.clone();
        self.start_pane_at(tab_index, &directory, operate)
    }

    fn start_pane_at(
        &mut self,
        tab_index: usize,
        directory: &Path,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_session_capacity()?;
        if self.next_pane == usize::MAX || self.next_session == usize::MAX {
            return Err("workspace exhausted pane or Session identities".into());
        }
        let pane = Pane {
            id: format!("p{}", self.next_pane),
            session: Session {
                id: format!("session-{}", self.next_session),
                endpoint: self.runtime.join(if self.next_session == 1 {
                    "orbit.sock".into()
                } else {
                    format!("session-{}.sock", self.next_session)
                }),
            },
        };
        operate(SessionOperation::Start {
            session: pane.session.clone(),
            directory: directory.to_path_buf(),
            command: LaunchCommand::Pane,
        })?;
        self.next_pane += 1;
        self.next_session += 1;
        self.tabs[tab_index].panes.push(pane);
        self.tabs[tab_index].selected_pane = Some(self.tabs[tab_index].panes.len() - 1);
        Ok(())
    }

    fn cancel_pending_tab(
        &mut self,
        tab_index: usize,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        if self.tabs.len() > 1 {
            let was_active = self.active == tab_index;
            let previous = self.tabs[tab_index].previous_tab.clone();
            self.tabs.remove(tab_index);
            let selected = index_after_removal(self.active, tab_index, self.tabs.len());
            self.active = if was_active {
                previous
                    .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
                    .unwrap_or(selected)
            } else {
                selected
            };
        } else {
            self.tabs[tab_index].pending = false;
            self.start_pane(tab_index, operate)
                .map_err(|detail| action_error("session-start", detail))?;
        }
        Ok(())
    }

    fn dismiss_for_pane_action(
        &mut self,
        tab_index: usize,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        let Some(instance) = self.tabs[tab_index].selected_popup.clone() else {
            return Ok(());
        };
        self.dismiss_popup(
            &PopupTarget {
                tab: self.tabs[tab_index].id.clone(),
                instance,
            },
            operate,
        )
    }

    fn focus_id(
        &mut self,
        id: &str,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        if let Some(tab_index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = tab_index;
            return Ok(());
        }
        for tab_index in 0..self.tabs.len() {
            if let Some(pane_index) = self.tabs[tab_index]
                .panes
                .iter()
                .position(|pane| pane.id == id)
            {
                self.dismiss_for_pane_action(tab_index, operate)?;
                self.active = tab_index;
                self.tabs[tab_index].selected_pane = Some(pane_index);
                return Ok(());
            }
        }
        Err(action_error(
            "unknown-id",
            format!("{id} is not a live tab or pane identity"),
        ))
    }

    fn focus_direction(
        &mut self,
        direction: Direction,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        match direction {
            Direction::Left => {
                self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1)
            }
            Direction::Right => self.active = (self.active + 1) % self.tabs.len(),
            Direction::Up | Direction::Down => {
                self.require_established(self.active)?;
                self.dismiss_for_pane_action(self.active, operate)?;
                let tab = &mut self.tabs[self.active];
                let selected = tab
                    .selected_pane
                    .ok_or_else(|| action_error("unavailable", "active tab has no pane"))?;
                tab.selected_pane = Some(if direction == Direction::Up {
                    selected.checked_sub(1).unwrap_or(tab.panes.len() - 1)
                } else {
                    (selected + 1) % tab.panes.len()
                });
            }
        }
        Ok(())
    }

    fn move_direction(
        &mut self,
        direction: Direction,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        match direction {
            Direction::Left if self.active > 0 => {
                self.tabs.swap(self.active, self.active - 1);
                self.active -= 1;
            }
            Direction::Right if self.active + 1 < self.tabs.len() => {
                self.tabs.swap(self.active, self.active + 1);
                self.active += 1;
            }
            Direction::Up | Direction::Down => {
                self.require_established(self.active)?;
                self.dismiss_for_pane_action(self.active, operate)?;
                let tab = &mut self.tabs[self.active];
                let selected = tab
                    .selected_pane
                    .ok_or_else(|| action_error("unavailable", "active tab has no pane"))?;
                let target = if direction == Direction::Up {
                    let Some(target) = selected.checked_sub(1) else {
                        return Ok(());
                    };
                    target
                } else {
                    if selected + 1 == tab.panes.len() {
                        return Ok(());
                    }
                    selected + 1
                };
                tab.panes.swap(selected, target);
                tab.selected_pane = Some(target);
            }
            Direction::Left | Direction::Right => {}
        }
        Ok(())
    }

    fn set_tab_directory(&mut self, id: &str, directory: Vec<u8>) -> Result<(), Failure> {
        if tab_number(id).is_none() {
            return Err(action_error(
                "invalid-tab",
                "tab directory targets require a canonical tN identity",
            ));
        }
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .ok_or_else(|| action_error("unknown-tab", format!("tab {id} is not live")))?;
        if tab.pending {
            return Err(action_error(
                "choose-directory-first",
                "the pending tab requires its exact Project chooser result",
            ));
        }
        tab.directory = validated_directory(directory)?;
        Ok(())
    }

    fn definition(&self, id: &str) -> Result<&PopupDefinition, Failure> {
        self.catalog
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| action_error("unknown-popup", format!("popup entry {id} is disabled")))
    }

    fn remove_popup(&mut self, tab_index: usize, popup_index: usize) {
        let removed = self.tabs[tab_index].popups.remove(popup_index);
        if self.tabs[tab_index].selected_popup.as_deref() == Some(&removed.id) {
            self.tabs[tab_index].selected_popup = None;
        }
    }

    fn stop_popup(
        &mut self,
        tab_index: usize,
        popup_index: usize,
        operate: &mut impl FnMut(SessionOperation) -> Result<(), String>,
    ) -> Result<(), Failure> {
        let session = self.tabs[tab_index].popups[popup_index].session.id.clone();
        operate(SessionOperation::Stop(session))
            .map_err(|detail| action_error("popup-stop", detail))?;
        self.remove_popup(tab_index, popup_index);
        Ok(())
    }

    fn remove_empty_tab(&mut self, tab_index: usize) {
        if self.tabs[tab_index].panes.is_empty()
            && self.tabs[tab_index].popups.is_empty()
            && self.tabs.len() > 1
        {
            self.tabs.remove(tab_index);
            self.active = index_after_removal(self.active, tab_index, self.tabs.len());
        }
    }

    fn require_established(&self, tab_index: usize) -> Result<(), Failure> {
        if self.tabs[tab_index].pending {
            Err(action_error(
                "choose-directory-first",
                "choose the pending tab directory before changing panes",
            ))
        } else {
            Ok(())
        }
    }

    fn check_session_capacity(&self) -> Result<(), String> {
        if self.session_count() >= MAX_SESSIONS {
            Err(format!("workspace is limited to {MAX_SESSIONS} Sessions"))
        } else {
            Ok(())
        }
    }

    fn session_count(&self) -> usize {
        self.tabs
            .iter()
            .map(|tab| tab.panes.len() + tab.popups.len())
            .sum()
    }

    fn reject_duplicate(&self, request_id: &str) -> Result<(), Failure> {
        if self.recent_requests.iter().any(|seen| seen == request_id) {
            Err(action_error(
                "duplicate-request",
                format!("request {request_id} was already accepted"),
            ))
        } else {
            Ok(())
        }
    }

    fn remember(&mut self, request_id: &str) {
        if self.recent_requests.len() == MAX_RECENT_REQUESTS {
            self.recent_requests.pop_front();
        }
        self.recent_requests.push_back(request_id.into());
    }
}

pub(crate) fn human(snapshot: &Snapshot) -> String {
    let mut output = format!("active {}\n", snapshot.active_tab);
    for tab in &snapshot.tabs {
        output.push_str(&format!(
            "tab {} directory={} selected={} popup={} pending={} active={}\n",
            tab.id,
            String::from_utf8_lossy(&tab.directory).escape_debug(),
            tab.selected_pane.as_deref().unwrap_or("none"),
            tab.selected_popup.as_deref().unwrap_or("none"),
            tab.pending,
            tab.id == snapshot.active_tab,
        ));
        for pane in &tab.panes {
            output.push_str(&format!(
                "  pane {} session={} live={} endpoint={}\n",
                pane.id,
                pane.session,
                pane.live,
                String::from_utf8_lossy(&pane.endpoint).escape_debug()
            ));
        }
        for popup in &tab.popups {
            let entry = snapshot
                .entries
                .iter()
                .find(|entry| entry.id == popup.entry);
            output.push_str(&format!(
                "  popup {} entry={} label={} key={} session={} chosen={} endpoint={}\n",
                popup.id,
                popup.entry,
                entry.map_or("?", |entry| entry.label.as_str()),
                entry.map_or_else(|| "?".into(), |entry| shortcut_text(&entry.shortcut)),
                popup.session,
                tab.selected_popup.as_deref() == Some(&popup.id),
                String::from_utf8_lossy(&popup.endpoint).escape_debug(),
            ));
        }
    }
    output
}

pub(crate) fn json(snapshot: &Snapshot) -> String {
    let mut output = format!(
        "{{\"active_tab\":\"{}\",\"geometry\":{{\"side_margin\":{},\"vertical_margin\":{}}},\"entries\":[",
        json_escape(&snapshot.active_tab),
        snapshot.geometry.side_margin,
        snapshot.geometry.vertical_margin
    );
    for (index, entry) in snapshot.entries.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"id\":\"{}\",\"label\":\"{}\",\"keybinding\":\"{}\"}}",
            json_escape(&entry.id),
            json_escape(&entry.label),
            json_escape(&shortcut_text(&entry.shortcut)),
        ));
    }
    output.push_str("],\"tabs\":[");
    for (tab_index, tab) in snapshot.tabs.iter().enumerate() {
        if tab_index != 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"id\":\"{}\",\"directory\":[{}],\"pending\":{},\"selected_pane\":",
            json_escape(&tab.id),
            json_bytes(&tab.directory),
            tab.pending
        ));
        json_option(&mut output, tab.selected_pane.as_deref());
        output.push_str(",\"selected_popup\":");
        json_option(&mut output, tab.selected_popup.as_deref());
        output.push_str(",\"panes\":[");
        for (index, pane) in tab.panes.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(&format!(
                "{{\"id\":\"{}\",\"session\":\"{}\",\"endpoint\":[{}],\"live\":{}}}",
                json_escape(&pane.id),
                json_escape(&pane.session),
                json_bytes(&pane.endpoint),
                pane.live
            ));
        }
        output.push_str("],\"popups\":[");
        for (index, popup) in tab.popups.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str(&format!(
                "{{\"id\":\"{}\",\"entry\":\"{}\",\"session\":\"{}\",\"endpoint\":[{}],\"chosen\":{}}}",
                json_escape(&popup.id), json_escape(&popup.entry), json_escape(&popup.session),
                json_bytes(&popup.endpoint), tab.selected_popup.as_deref() == Some(&popup.id)
            ));
        }
        output.push_str("]}");
    }
    output.push_str("]}\n");
    output
}

pub(crate) fn failure_human(failure: &Failure) -> String {
    format!(
        "error {}: {}\n",
        failure.code,
        failure.detail.escape_debug()
    )
}

pub(crate) fn failure_json(failure: &Failure) -> String {
    format!(
        "{{\"error\":{{\"code\":\"{}\",\"detail\":\"{}\"}}}}\n",
        json_escape(&failure.code),
        json_escape(&failure.detail)
    )
}

pub(crate) fn validate_initial_directory(directory: &Path) -> Result<(), String> {
    let bytes = directory.as_os_str().as_bytes();
    if !directory.is_absolute() || bytes.len() > MAX_DIRECTORY_BYTES || bytes.contains(&0) {
        return Err("Eon launch directory must be one bounded absolute Unix path".into());
    }
    if !fs::metadata(directory)
        .map_err(|error| format!("cannot inspect Eon launch directory: {error}"))?
        .is_dir()
    {
        return Err("Eon launch directory is not a directory".into());
    }
    Ok(())
}

fn validated_directory(directory: Vec<u8>) -> Result<PathBuf, Failure> {
    if directory.is_empty()
        || directory.len() > MAX_DIRECTORY_BYTES
        || directory.contains(&0)
        || directory[0] != b'/'
    {
        return Err(action_error(
            "invalid-directory",
            "tab launch directory must be one bounded absolute Unix path",
        ));
    }
    let directory = PathBuf::from(OsString::from_vec(directory));
    if !fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(action_error(
            "invalid-directory",
            "tab launch directory is unavailable or not a directory",
        ));
    }
    Ok(directory)
}

fn tab_number(value: &str) -> Option<usize> {
    value.strip_prefix('t').and_then(canonical_number)
}

fn canonical_number(value: &str) -> Option<usize> {
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<usize>()
        .ok()
        .filter(|number| *number > 0 && number.to_string() == value)
}

fn index_after_removal(selected: usize, removed: usize, remaining: usize) -> usize {
    if selected > removed {
        selected - 1
    } else {
        selected.min(remaining.saturating_sub(1))
    }
}

fn action_error(code: &'static str, detail: impl Into<String>) -> Failure {
    Failure {
        code: code.into(),
        detail: detail.into(),
    }
}

fn shortcut_text(shortcut: &eon_workspace_protocol::v7::Shortcut) -> String {
    let mut parts = Vec::new();
    for (bit, name) in [
        (eon_workspace_protocol::v7::CTRL, "Ctrl"),
        (eon_workspace_protocol::v7::ALT, "Alt"),
        (eon_workspace_protocol::v7::SHIFT, "Shift"),
        (eon_workspace_protocol::v7::SUPER, "Super"),
    ] {
        if shortcut.modifiers & bit != 0 {
            parts.push(name.to_string());
        }
    }
    parts.push(
        shortcut
            .key
            .strip_prefix("Key")
            .or_else(|| shortcut.key.strip_prefix("Digit"))
            .unwrap_or(&shortcut.key)
            .to_string(),
    );
    parts.join("+")
}

fn json_option(output: &mut String, value: Option<&str>) {
    match value {
        Some(value) => output.push_str(&format!("\"{}\"", json_escape(value))),
        None => output.push_str("null"),
    }
}

fn json_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn json_escape(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use eon_workspace_protocol::v7::{ALT, PopupGeometry, Shortcut};

    fn catalog() -> PopupCatalog {
        PopupCatalog {
            geometry: PopupGeometry {
                side_margin: 8.0,
                vertical_margin: 4.0,
            },
            entries: vec![
                PopupDefinition {
                    id: "project".into(),
                    label: "Project".into(),
                    shortcut: Shortcut {
                        modifiers: ALT,
                        key: "KeyZ".into(),
                    },
                    command: PopupCommand::Project,
                    keep_alive: false,
                },
                PopupDefinition {
                    id: "agent".into(),
                    label: "Agent".into(),
                    shortcut: Shortcut {
                        modifiers: ALT,
                        key: "KeyA".into(),
                    },
                    command: PopupCommand::Argv(vec!["agent".into()]),
                    keep_alive: true,
                },
            ],
        }
    }

    fn prepared(command: &PopupCommand, _: &Path) -> Result<Vec<OsString>, String> {
        match command {
            PopupCommand::Argv(argv) => Ok(argv.clone()),
            PopupCommand::Project => Ok(Vec::new()),
            PopupCommand::AgentAuto => unreachable!(),
        }
    }

    #[test]
    fn popup_identity_reuses_hides_and_replaces_only_after_retarget() {
        let root = std::env::temp_dir().join(format!("eon-workspace-{}", std::process::id()));
        let old = root.join("old");
        let new = root.join("new");
        let newest = root.join("newest");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::create_dir_all(&newest).unwrap();
        let mut operations = Vec::new();
        let mut workspace = Workspace::with_recovered_sessions(
            root.clone(),
            old,
            vec![(
                1,
                Session {
                    id: "session-1".into(),
                    endpoint: root.join("orbit.sock"),
                },
            )],
            catalog(),
        )
        .unwrap();
        let mut operate = |operation| {
            operations.push(operation);
            Ok(())
        };
        workspace
            .dispatch(
                "open",
                Action::InvokePopup {
                    tab: "t1".into(),
                    entry: "agent".into(),
                    expected_instance: None,
                    intent: InvokeIntent::Toggle,
                },
                prepared,
                &mut operate,
            )
            .unwrap();
        let first = workspace.snapshot().tabs[0].popups[0].clone();
        workspace
            .dispatch(
                "hide",
                Action::InvokePopup {
                    tab: "t1".into(),
                    entry: "agent".into(),
                    expected_instance: Some(first.id.clone()),
                    intent: InvokeIntent::Toggle,
                },
                prepared,
                &mut operate,
            )
            .unwrap();
        assert_eq!(workspace.snapshot().tabs[0].popups[0], first);
        workspace
            .dispatch(
                "retarget",
                Action::Workspace(WorkspaceAction::SetTabDirectory {
                    tab: "t1".into(),
                    directory: new.as_os_str().as_bytes().to_vec(),
                }),
                prepared,
                &mut operate,
            )
            .unwrap();
        assert_eq!(workspace.snapshot().tabs[0].popups[0], first);
        workspace
            .dispatch(
                "replace",
                Action::InvokePopup {
                    tab: "t1".into(),
                    entry: "agent".into(),
                    expected_instance: Some(first.id.clone()),
                    intent: InvokeIntent::Toggle,
                },
                prepared,
                &mut operate,
            )
            .unwrap();
        let replacement = &workspace.snapshot().tabs[0].popups[0];
        assert_ne!(replacement.id, first.id);
        assert_eq!(
            operations
                .iter()
                .filter_map(|operation| match operation {
                    SessionOperation::Stop(id) => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [first.session]
        );
        assert_eq!(
            operations
                .iter()
                .filter_map(|operation| match operation {
                    SessionOperation::Start { directory, .. } => Some(directory),
                    SessionOperation::Stop(_) => None,
                })
                .collect::<Vec<_>>(),
            [&root.join("old"), &new]
        );

        let live = replacement.clone();
        workspace
            .dispatch(
                "retarget-again",
                Action::Workspace(WorkspaceAction::SetTabDirectory {
                    tab: "t1".into(),
                    directory: newest.as_os_str().as_bytes().to_vec(),
                }),
                prepared,
                |_| Ok(()),
            )
            .unwrap();
        let replace = || Action::InvokePopup {
            tab: "t1".into(),
            entry: "agent".into(),
            expected_instance: Some(live.id.clone()),
            intent: InvokeIntent::Toggle,
        };
        assert_eq!(
            workspace
                .dispatch(
                    "stop-fails",
                    replace(),
                    prepared,
                    |operation| match operation {
                        SessionOperation::Stop(_) => Err("stop failed".into()),
                        SessionOperation::Start { .. } => Ok(()),
                    }
                )
                .unwrap_err()
                .code,
            "popup-stop"
        );
        assert_eq!(
            workspace.snapshot().tabs[0].popups.as_slice(),
            std::slice::from_ref(&live)
        );
        assert_eq!(
            workspace
                .dispatch(
                    "start-fails",
                    replace(),
                    prepared,
                    |operation| match operation {
                        SessionOperation::Stop(_) => Ok(()),
                        SessionOperation::Start { .. } => Err("start failed".into()),
                    }
                )
                .unwrap_err()
                .code,
            "popup-start"
        );
        assert!(workspace.snapshot().tabs[0].popups.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn popup_switching_stops_transients_without_leaving_empty_tabs() {
        let mut workspace = Workspace::with_recovered_sessions(
            "/runtime".into(),
            "/".into(),
            vec![(
                1,
                Session {
                    id: "session-1".into(),
                    endpoint: "/runtime/orbit.sock".into(),
                },
            )],
            catalog(),
        )
        .unwrap();
        let mut operations = Vec::new();
        let mut operate = |operation| {
            operations.push(operation);
            Ok(())
        };
        let invoke = |entry: &str, expected_instance: Option<String>| Action::InvokePopup {
            tab: "t1".into(),
            entry: entry.into(),
            expected_instance,
            intent: InvokeIntent::Toggle,
        };

        workspace
            .dispatch("agent-open", invoke("agent", None), prepared, &mut operate)
            .unwrap();
        let agent = workspace.snapshot().tabs[0].popups[0].clone();
        workspace
            .dispatch(
                "agent-hide",
                invoke("agent", Some(agent.id.clone())),
                prepared,
                &mut operate,
            )
            .unwrap();
        workspace
            .dispatch(
                "project-open",
                invoke("project", None),
                prepared,
                &mut operate,
            )
            .unwrap();
        workspace
            .dispatch(
                "agent-return",
                invoke("agent", Some(agent.id.clone())),
                prepared,
                &mut operate,
            )
            .unwrap();

        let snapshot = workspace.snapshot();
        assert_eq!(
            snapshot.tabs[0].popups.as_slice(),
            std::slice::from_ref(&agent)
        );
        assert_eq!(snapshot.tabs[0].selected_popup.as_ref(), Some(&agent.id));
        assert!(matches!(
            operations.last(),
            Some(SessionOperation::Stop(id)) if id == "directory-picker-2"
        ));

        workspace.tabs.push(Tab {
            id: "t2".into(),
            directory: "/".into(),
            pending: false,
            previous_tab: None,
            panes: vec![Pane {
                id: "p99".into(),
                session: Session {
                    id: "session-99".into(),
                    endpoint: "/runtime/session-99.sock".into(),
                },
            }],
            selected_pane: Some(0),
            popups: Vec::new(),
            selected_popup: None,
        });
        workspace
            .dispatch("project-again", invoke("project", None), prepared, |_| {
                Ok(())
            })
            .unwrap();
        workspace
            .session_exited(&agent.session, |_| Ok(()))
            .unwrap();
        workspace.session_exited("session-1", |_| Ok(())).unwrap();
        assert_eq!(workspace.snapshot().tabs[0].popups[0].entry, "project");

        let failure = workspace
            .dispatch(
                "replacement-fails",
                invoke("agent", None),
                prepared,
                |operation| match operation {
                    SessionOperation::Stop(_) => Ok(()),
                    SessionOperation::Start { .. } => Err("start failed".into()),
                },
            )
            .unwrap_err();
        assert_eq!(failure.code, "popup-start");
        assert_eq!(workspace.snapshot().tabs[0].id, "t2");
    }

    #[test]
    fn chooser_result_requires_its_exact_live_invocation() {
        let root = std::env::temp_dir().join(format!("eon-chooser-{}", std::process::id()));
        let selected = root.join("selected");
        fs::create_dir_all(&selected).unwrap();
        let mut workspace =
            Workspace::pending(root.clone(), root.clone(), catalog(), prepared, |_| Ok(()))
                .unwrap();
        let project = workspace.snapshot().tabs[0].popups[0].clone();
        let commit = || Action::CommitDirectory {
            target: PopupTarget {
                tab: "t1".into(),
                instance: project.id.clone(),
            },
            directory: selected.as_os_str().as_bytes().to_vec(),
        };
        assert_eq!(
            workspace
                .dispatch("commit-fails", commit(), prepared, |_| {
                    Err("start failed".into())
                })
                .unwrap_err()
                .code,
            "session-start"
        );
        let unchanged = workspace.snapshot();
        assert!(unchanged.tabs[0].pending);
        assert_eq!(unchanged.tabs[0].directory, root.as_os_str().as_bytes());
        assert!(unchanged.tabs[0].panes.is_empty());
        workspace
            .dispatch("commit", commit(), prepared, |_| Ok(()))
            .unwrap();
        assert!(!workspace.snapshot().tabs[0].pending);
        assert!(
            workspace
                .dispatch(
                    "stale",
                    Action::DismissPopup(PopupTarget {
                        tab: "t1".into(),
                        instance: "u999".into(),
                    }),
                    prepared,
                    |_| Ok(()),
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_tabs_keep_independent_pickers_and_close_only_the_selected_tab() {
        let mut workspace =
            Workspace::pending("/runtime".into(), "/".into(), catalog(), prepared, |_| {
                Ok(())
            })
            .unwrap();
        workspace
            .dispatch(
                "new-t2",
                Action::Workspace(WorkspaceAction::CreateTab),
                prepared,
                |_| Ok(()),
            )
            .unwrap();
        let state = workspace.snapshot();
        assert_eq!(state.active_tab, "t2");
        assert_eq!(state.tabs.len(), 2);
        assert!(state.tabs.iter().all(|tab| tab.pending));
        assert_ne!(state.tabs[0].popups[0].id, state.tabs[1].popups[0].id);
        assert_ne!(
            state.tabs[0].popups[0].endpoint,
            state.tabs[1].popups[0].endpoint
        );
        let first = state.tabs[0].popups[0].clone();
        let second = state.tabs[1].popups[0].clone();

        workspace
            .dispatch(
                "focus-t1",
                Action::Workspace(WorkspaceAction::FocusId("t1".into())),
                prepared,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            workspace.prepare_close_tab("close-t1", "t1").unwrap(),
            std::slice::from_ref(&first.session)
        );
        workspace
            .session_exited(&first.session, |_| Ok(()))
            .unwrap();
        assert_eq!(workspace.snapshot().active_tab, "t2");
        assert_eq!(workspace.snapshot().tabs[0].popups[0], second);
        assert!(
            workspace
                .dispatch(
                    "stale-t1",
                    Action::CommitDirectory {
                        target: PopupTarget {
                            tab: "t1".into(),
                            instance: first.id,
                        },
                        directory: b"/".to_vec(),
                    },
                    prepared,
                    |_| Ok(()),
                )
                .is_err()
        );

        let before_failed_launch = workspace.snapshot();
        assert_eq!(
            workspace
                .dispatch(
                    "new-fails",
                    Action::Workspace(WorkspaceAction::CreateTab),
                    prepared,
                    |_| Err("launch failed".into()),
                )
                .unwrap_err()
                .code,
            "popup-start"
        );
        assert_eq!(workspace.snapshot(), before_failed_launch);

        workspace
            .dispatch(
                "new-t3",
                Action::Workspace(WorkspaceAction::CreateTab),
                prepared,
                |_| Ok(()),
            )
            .unwrap();
        let third = workspace.snapshot().tabs[1].popups[0].clone();
        assert_eq!(
            workspace.prepare_close_tab("close-t3", "t3").unwrap(),
            std::slice::from_ref(&third.session)
        );
        workspace
            .session_exited(&third.session, |_| Ok(()))
            .unwrap();
        assert_eq!(workspace.snapshot(), before_failed_launch);

        workspace
            .dispatch(
                "accept-t2",
                Action::CommitDirectory {
                    target: PopupTarget {
                        tab: "t2".into(),
                        instance: second.id,
                    },
                    directory: b"/".to_vec(),
                },
                prepared,
                |_| Ok(()),
            )
            .unwrap();
        assert!(!workspace.snapshot().tabs[0].pending);
        assert_eq!(workspace.snapshot().tabs[0].panes.len(), 1);
    }

    #[test]
    fn background_pending_picker_exit_keeps_the_selected_tab() {
        let mut workspace =
            Workspace::pending("/runtime".into(), "/".into(), catalog(), prepared, |_| {
                Ok(())
            })
            .unwrap();
        for number in 2..=3 {
            workspace
                .dispatch(
                    &format!("new-t{number}"),
                    Action::Workspace(WorkspaceAction::CreateTab),
                    prepared,
                    |_| Ok(()),
                )
                .unwrap();
        }
        let second_picker = workspace.snapshot().tabs[1].popups[0].session.clone();
        workspace
            .session_exited(&second_picker, |_| Ok(()))
            .unwrap();
        let snapshot = workspace.snapshot();
        assert_eq!(snapshot.active_tab, "t3");
        assert_eq!(
            snapshot
                .tabs
                .iter()
                .map(|tab| tab.id.as_str())
                .collect::<Vec<_>>(),
            ["t1", "t3"]
        );
    }
}
