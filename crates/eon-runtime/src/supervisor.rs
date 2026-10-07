use super::{
    cli::Inputs,
    codex_quota,
    control::{
        ControlListener, ControlResponse, EndpointFailure, EndpointFailureKind, SocketIdentity,
        connect_control, failure, probe_launch_mode, probe_presentable_runtime, send_action_on,
        socket_identity,
    },
    generation::{current_generation, generation_directory},
    managed_environment::{self, nonempty_environment_path},
    sessions::{
        RunningSession, acquire_generation_sessions, recover_sessions, session_finished,
        start_orbit, stop_managed_sessions,
    },
    workspace::{self, LaunchCommand, SessionOperation, Workspace},
};
use eon_workspace_protocol::v7::{
    Action, Availability, Failure, LifecycleResponse, Request, Response, Runtime, Snapshot,
    Stopped, VERSION, WorkspaceAction,
};
use std::{
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::fd::OwnedFd,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(super) const SESSION_START_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

pub(super) fn request_id() -> String {
    let sequence = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}-{sequence}", std::process::id())
}

#[cfg(test)]
static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn temporary_directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "eon-test-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

pub(super) fn probe_supervisor(
    inputs: &Inputs,
    socket: &Path,
    generation: &str,
) -> Result<(LaunchMode, SocketIdentity), EndpointFailure> {
    let identity = socket_identity(socket)
        .map_err(|detail| EndpointFailure::new(EndpointFailureKind::Corrupt, detail))?
        .ok_or_else(|| {
            EndpointFailure::new(
                EndpointFailureKind::Dead,
                format!("endpoint {} is missing", socket.display()),
            )
        })?;
    let result = (|| {
        let info = probe_presentable_runtime(socket)?;
        if !info.attach.available {
            return Err(EndpointFailure::new(
                EndpointFailureKind::Incompatible,
                info.attach.reason,
            ));
        }
        validate_runtime(inputs, &info, generation)
            .map_err(|detail| EndpointFailure::new(EndpointFailureKind::Corrupt, detail))?;
        probe_launch_mode(socket)
    })();
    if socket_identity(socket)
        .map_err(|detail| EndpointFailure::new(EndpointFailureKind::Corrupt, detail))?
        != Some(identity)
    {
        return Err(EndpointFailure::new(
            EndpointFailureKind::Dead,
            "supervisor endpoint changed while it was being validated",
        ));
    }
    result.map(|mode| (mode, identity))
}

fn validate_owned_private_directory(path: &Path, kind: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {kind} {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(format!(
            "{kind} {} must be an owned private directory",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn validate_private_directory(path: &Path) -> Result<(), String> {
    validate_owned_private_directory(path, "directory")
}

pub(super) fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchMode {
    Workspace,
    Terminal,
}

impl LaunchMode {
    fn name(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Terminal => "EonTerm",
        }
    }
}

pub(super) fn launch_current(
    inputs: &Inputs,
    mode: LaunchMode,
    child: &[OsString],
    attach_existing: bool,
    decorations: bool,
    application_id: &str,
) -> Result<i32, String> {
    let generation = current_generation(inputs)?;
    let root = runtime_directory(if mode == LaunchMode::Terminal {
        "eonterm"
    } else {
        "eon"
    });
    prepare_runtime(&root)?;
    let lifecycle_lock_path = supervisor_lock_path(&root, &generation);
    let runtime = generation_directory(&root, &generation);
    let socket = runtime.join("eon.sock");
    let mut missing_deadline = None;
    let lifecycle_lock = loop {
        let endpoint_failure = match probe_supervisor(inputs, &socket, &generation) {
            Ok((active_mode, supervisor)) => {
                missing_deadline = None;
                if active_mode != mode {
                    return Err(format!(
                        "generation {generation} already has a live {} mode; requested {} mode",
                        active_mode.name(),
                        mode.name()
                    ));
                }
                if !attach_existing {
                    return Err(format!(
                        "generation {generation} already has a live Eon supervisor"
                    ));
                }
                let presentation_error =
                    match present_at(inputs, &runtime, &generation, mode, supervisor) {
                        Ok(code) => return Ok(code),
                        Err(error) => error,
                    };
                match probe_supervisor(inputs, &socket, &generation) {
                    Ok((_, current)) if current == supervisor => return Err(presentation_error),
                    Ok(_) => continue,
                    Err(error)
                        if matches!(
                            error.kind,
                            EndpointFailureKind::Dead | EndpointFailureKind::Unreachable
                        ) =>
                    {
                        error
                    }
                    Err(_) => return Err(presentation_error),
                }
            }
            Err(error)
                if matches!(
                    error.kind,
                    EndpointFailureKind::Dead | EndpointFailureKind::Unreachable
                ) =>
            {
                error
            }
            Err(error) => return Err(error.detail),
        };
        let lifecycle_lock = if endpoint_failure.kind == EndpointFailureKind::Dead {
            let Some(lock) = try_lock_supervisor_lifecycle(&lifecycle_lock_path)? else {
                let deadline = missing_deadline.get_or_insert_with(|| {
                    Instant::now() + SESSION_START_TIMEOUT.saturating_add(Duration::from_secs(1))
                });
                if Instant::now() >= *deadline {
                    return Err(format!(
                        "timed out waiting for Eon supervisor lifecycle {}",
                        lifecycle_lock_path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(25));
                continue;
            };
            lock
        } else {
            lock_supervisor_lifecycle(&lifecycle_lock_path)?
        };
        match probe_supervisor(inputs, &socket, &generation) {
            Err(error) if error.kind == EndpointFailureKind::Dead => break lifecycle_lock,
            Ok(_) => {
                drop(lifecycle_lock);
                continue;
            }
            Err(error) => return Err(error.detail),
        }
    };
    let config = configuration_directory()?;
    let terminal = managed_environment::terminal_presentation(&config, &inputs.defaults)?;
    let popups = (mode == LaunchMode::Workspace)
        .then(|| managed_environment::popup_catalog(&config, &inputs.defaults))
        .transpose()?;
    let startup_animation = (mode == LaunchMode::Workspace)
        .then(|| managed_environment::startup_animation(&config, &inputs.defaults))
        .transpose()?
        .flatten();
    prepare_generation_runtime(&root, &generation)?;
    prepare_configuration(&config)?;
    supervise(
        inputs,
        &config,
        terminal,
        popups,
        startup_animation,
        lifecycle_lock,
        &runtime,
        child,
        &generation,
        mode,
        application_id,
        decorations,
    )
    .or_else(|error| {
        if attach_existing && path_exists(&socket) {
            attach_competing_supervisor(inputs, &socket, &runtime, &generation, mode, &error)
        } else {
            Err(error)
        }
    })
}

fn attach_competing_supervisor(
    inputs: &Inputs,
    socket: &Path,
    runtime: &Path,
    generation: &str,
    mode: LaunchMode,
    launch_error: &str,
) -> Result<i32, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match probe_supervisor(inputs, socket, generation) {
            Ok((active_mode, supervisor)) => {
                if active_mode != mode {
                    return Err(format!(
                        "{launch_error}; competing supervisor started in {} mode, requested {} mode",
                        active_mode.name(),
                        mode.name()
                    ));
                }
                return present_at(inputs, runtime, generation, mode, supervisor);
            }
            Err(error)
                if matches!(
                    error.kind,
                    EndpointFailureKind::Dead | EndpointFailureKind::Unreachable
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(format!(
                    "{launch_error}; competing supervisor did not become attachable: {}",
                    error.detail
                ));
            }
        }
    }
}

fn validate_runtime(inputs: &Inputs, info: &Runtime, generation: &str) -> Result<(), String> {
    if info.generation != generation {
        return Err(format!(
            "runtime directory contains generation {}, expected {generation}",
            info.generation
        ));
    }
    if info.workspace_protocol != VERSION {
        return Err(format!(
            "supervisor uses EONW {}, expected EONW {VERSION}",
            info.workspace_protocol
        ));
    }
    let components = &inputs.components()?.report;
    if &info.component_report != components {
        return Err("supervisor reports a different component graph".into());
    }
    Ok(())
}

pub(super) fn present_at(
    inputs: &Inputs,
    runtime: &Path,
    generation: &str,
    mode: LaunchMode,
    supervisor: SocketIdentity,
) -> Result<i32, String> {
    let control = runtime.join("eon.sock");
    let stream = connect_control(&control).map_err(|error| error.detail)?;
    if socket_identity(&control)? != Some(supervisor) {
        return Err("supervisor changed before Eon Desktop could present".into());
    }
    match send_action_on(
        stream,
        Action::Workspace(WorkspaceAction::Present {
            workspace: mode == LaunchMode::Workspace,
        }),
    )
    .map_err(|error| error.detail)?
    {
        ControlResponse::Lifecycle(LifecycleResponse::Runtime(info)) => {
            validate_runtime(inputs, &info, generation)?;
            Ok(0)
        }
        ControlResponse::Lifecycle(LifecycleResponse::Failure(failure)) => {
            Err(format!("cannot present Eon Desktop: {}", failure.detail))
        }
        _ => Err("supervisor returned the wrong EONW result for presentation".into()),
    }
}

fn venus_command(
    inputs: &Inputs,
    config: &Path,
    socket: &Path,
    mode: LaunchMode,
    terminal: &managed_environment::TerminalConfig,
    application_id: &str,
    decorations: bool,
) -> Command {
    let mut command = Command::new(&inputs.venus);
    if !decorations {
        command.arg("--no-decorations");
    }
    command
        .arg("--application-id")
        .arg(application_id)
        .arg("--background-opacity")
        .arg(terminal.background_opacity.to_string());
    if terminal.background_blur {
        command.arg("--background-blur");
    }
    if let Some(color) = &terminal.cursor_trail_color {
        command.arg("--cursor-trail-color").arg(color);
    }
    if let Some(family) = &terminal.font_family {
        command.arg("--font-family").arg(family);
    }
    for family in &terminal.font_fallbacks {
        command.arg("--font-fallback").arg(family);
    }
    for (flag, value) in [
        (
            "--cursor-trail-duration-v1",
            terminal
                .cursor_trail_duration
                .map(|value| value.to_string()),
        ),
        (
            "--font-size",
            terminal.font_size.map(|value| value.to_string()),
        ),
        (
            "--line-height",
            terminal.line_height.map(|value| value.to_string()),
        ),
        ("--columns", terminal.columns.map(|value| value.to_string())),
        ("--rows", terminal.rows.map(|value| value.to_string())),
    ] {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    }
    if mode == LaunchMode::Workspace {
        if let Ok(executable) = env::current_exe() {
            command.env("EON_NEW_WINDOW_EXECUTABLE", executable);
        }
        command
            .arg("--pane-frames")
            .arg(terminal.pane_frames.to_string())
            .arg("--workspace");
    }
    command.arg(socket);
    command.env("EON_CONFIG_HOME", config);
    command
}

struct PresentationProcess {
    child: Child,
    control: UnixStream,
}

impl PresentationProcess {
    fn start(
        mut command: Command,
        admission: bool,
        snapshot: Option<Snapshot>,
        deadline: Instant,
    ) -> Result<Self, String> {
        let (control, input) = UnixStream::pair()
            .map_err(|error| format!("cannot create Eon Desktop presentation control: {error}"))?;
        if admission {
            command.stdout(Stdio::from(OwnedFd::from(input.try_clone().map_err(
                |error| format!("cannot create Eon Desktop readiness channel: {error}"),
            )?)));
        }
        command
            .env(
                "EON_VENUS_PRESENTATION_CONTROL",
                if admission { "stdin-ready-v1" } else { "stdin" },
            )
            .stdin(Stdio::from(OwnedFd::from(input)));
        let child = command
            .spawn()
            .map_err(|error| format!("cannot launch Eon Desktop: {error}"))?;
        // Command retains its Stdio handles; release them so child exit yields EOF.
        drop(command);
        let mut process = Self { child, control };
        if admission {
            let result = (|| {
                let timeout = || {
                    deadline
                        .checked_duration_since(Instant::now())
                        .filter(|duration| !duration.is_zero())
                        .ok_or("timed out waiting for native startup admission")
                };
                if let Some(snapshot) = snapshot {
                    let bytes =
                        eon_workspace_protocol::v7::encode_response(&Response::Snapshot(snapshot))
                            .map_err(|error| format!("cannot encode startup workspace: {error}"))?;
                    let mut remaining = bytes.as_slice();
                    while !remaining.is_empty() {
                        process
                            .control
                            .set_write_timeout(Some(timeout()?))
                            .map_err(|error| error.to_string())?;
                        match process.control.write(remaining) {
                            Ok(0) => {
                                return Err("Venus closed while receiving startup workspace".into());
                            }
                            Ok(count) => remaining = &remaining[count..],
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(error) => return Err(error.to_string()),
                        }
                    }
                }
                let mut ready = [0; 8];
                let mut remaining = &mut ready[..];
                while !remaining.is_empty() {
                    process
                        .control
                        .set_read_timeout(Some(timeout()?))
                        .map_err(|error| error.to_string())?;
                    match process.control.read(remaining) {
                        Ok(0) => return Err("Venus closed before native startup admission; see its diagnostic in the supervisor output".into()),
                        Ok(count) => remaining = &mut remaining[count..],
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.to_string()),
                    }
                }
                if ready != *b"ready-v1" {
                    return Err("invalid Venus startup readiness response".into());
                }
                Ok(())
            })();
            if let Err(error) = result {
                stop(&mut process.child);
                return Err(format!("cannot admit Eon Desktop presentation: {error}"));
            }
        }
        process
            .control
            .set_write_timeout(Some(Duration::from_millis(250)))
            .map_err(|error| format!("cannot bound Eon Desktop presentation control: {error}"))?;
        Ok(process)
    }

    fn present(&self) -> Result<(), String> {
        (&self.control)
            .write_all(b"present\n")
            .map_err(|error| format!("cannot signal Eon Desktop presentation: {error}"))
    }
}

impl Drop for PresentationProcess {
    fn drop(&mut self) {
        let _ = self.control.shutdown(Shutdown::Both);
        let deadline = Instant::now() + SESSION_START_TIMEOUT;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        stop(&mut self.child);
    }
}

pub(super) fn configuration_directory() -> Result<PathBuf, String> {
    let path = nonempty_environment_path("EON_CONFIG_HOME")
        .or_else(|| {
            xdg_path(nonempty_environment_path("XDG_CONFIG_HOME")).map(|path| path.join("eon"))
        })
        .or_else(|| nonempty_environment_path("HOME").map(|path| path.join(".config/eon")))
        .ok_or_else(|| "HOME, XDG_CONFIG_HOME, and EON_CONFIG_HOME are unset".to_string())?;
    std::path::absolute(path)
        .map_err(|error| format!("cannot resolve Eon configuration root: {error}"))
}

pub(super) fn prepare_configuration(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

pub(super) fn runtime_directory(product: &str) -> PathBuf {
    if product == "eon"
        && let Some(root) = nonempty_environment_path("EON_WINDOW_RUNTIME_DIR")
    {
        return root;
    }
    base_runtime_directory(product)
}

pub(super) fn base_runtime_directory(product: &str) -> PathBuf {
    nonempty_environment_path("EON_RUNTIME_DIR")
        .or_else(|| {
            xdg_path(nonempty_environment_path("XDG_RUNTIME_DIR")).map(|path| path.join(product))
        })
        .unwrap_or_else(|| {
            eprintln!(
                "{product}: warning: XDG_RUNTIME_DIR is unset; using a private temporary runtime root"
            );
            env::temp_dir().join(format!("{product}-{}", effective_uid()))
        })
}

pub(super) fn prepare_generation_runtime(root: &Path, generation: &str) -> Result<PathBuf, String> {
    prepare_runtime(root)?;
    prepare_runtime(&root.join("generations"))?;
    let runtime = generation_directory(root, generation);
    prepare_runtime(&runtime)?;
    Ok(runtime)
}

fn xdg_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| path.is_absolute())
}

pub(super) fn prepare_runtime(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| {
            format!(
                "cannot create runtime directory {}: {error}",
                path.display()
            )
        })?;
    validate_owned_private_directory(path, "runtime directory")
}

pub(super) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and no failure state.
    unsafe { libc::geteuid() }
}

fn recovered_workspace_sessions(
    sessions: &[RunningSession],
) -> Result<Vec<(usize, workspace::Session)>, String> {
    sessions
        .iter()
        .map(|session| {
            Ok((
                session
                    .number
                    .ok_or("recovered a transient terminal as a durable pane")?,
                workspace::Session {
                    id: session.id.clone(),
                    endpoint: session.endpoint.clone(),
                },
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    inputs: &Inputs,
    config: &Path,
    terminal: managed_environment::TerminalConfig,
    popups: Option<managed_environment::PopupCatalog>,
    startup_animation: Option<managed_environment::StartupAnimation>,
    lifecycle_lock: fs::File,
    runtime: &Path,
    child: &[OsString],
    generation: &str,
    mode: LaunchMode,
    application_id: &str,
    decorations: bool,
) -> Result<i32, String> {
    let _lifecycle_lock = lifecycle_lock;
    let launch_directory = env::current_dir()
        .map_err(|error| format!("cannot resolve Eon launch directory: {error}"))?;
    if mode == LaunchMode::Workspace {
        workspace::validate_initial_directory(&launch_directory)?;
    }
    let component_generation = inputs.components()?.orbit_revision.clone();
    let deadline = Instant::now() + SESSION_START_TIMEOUT;
    let popup_catalog = popups.unwrap_or_else(|| managed_environment::PopupCatalog {
        geometry: inputs.defaults.popups.geometry,
        entries: Vec::new(),
    });
    let (mut sessions, recovered_picker) =
        recover_sessions(runtime, mode, &component_generation, deadline)?;
    let control_listener = ControlListener::bind(&runtime.join("eon.sock"))?;
    let initial_endpoint = runtime.join("orbit.sock");
    let initial_session_id = "session-1";
    let presentation_socket = if mode == LaunchMode::Workspace {
        runtime.join("eon.sock")
    } else {
        sessions.first().map_or_else(
            || initial_endpoint.clone(),
            |session| session.endpoint.clone(),
        )
    };
    let fresh_animation = startup_animation.is_some() && sessions.is_empty() && !recovered_picker;
    let admitted = if terminal.requires_startup_admission() || fresh_animation {
        let snapshot = if mode == LaunchMode::Workspace {
            Some(
                if sessions.is_empty() && !recovered_picker {
                    Workspace::pending(
                        runtime.to_path_buf(),
                        launch_directory.clone(),
                        popup_catalog.clone(),
                        |_, _| Ok(Vec::new()),
                        |_| Ok(()),
                    )?
                } else {
                    let recovered = if sessions.is_empty() {
                        vec![(
                            1,
                            workspace::Session {
                                id: initial_session_id.into(),
                                endpoint: initial_endpoint.clone(),
                            },
                        )]
                    } else {
                        recovered_workspace_sessions(&sessions)?
                    };
                    Workspace::with_recovered_sessions(
                        runtime.to_path_buf(),
                        launch_directory.clone(),
                        recovered,
                        popup_catalog.clone(),
                    )?
                }
                .snapshot(),
            )
        } else {
            None
        };
        let command = venus_command(
            inputs,
            config,
            &presentation_socket,
            mode,
            &terminal,
            application_id,
            decorations,
        );
        Some(PresentationProcess::start(
            command, true, snapshot, deadline,
        )?)
    } else {
        None
    };
    let mut initial_child = if sessions.is_empty() {
        child.to_vec()
    } else {
        Vec::new()
    };
    if sessions.is_empty() && (mode == LaunchMode::Terminal || recovered_picker) {
        sessions.push(start_orbit(
            inputs,
            config,
            &initial_endpoint,
            initial_session_id,
            &component_generation,
            &launch_directory,
            &initial_child,
            deadline,
        )?);
        initial_child.clear();
    }
    let workspace = if mode == LaunchMode::Workspace {
        Some(if sessions.is_empty() {
            Workspace::pending(
                runtime.to_path_buf(),
                launch_directory.clone(),
                popup_catalog.clone(),
                |command, directory| {
                    managed_environment::prepare_popup_command(
                        command,
                        inputs.managed.shell_bin.as_deref(),
                        directory,
                        &inputs.defaults,
                    )
                },
                |operation| {
                    apply_session_operation(
                        inputs,
                        config,
                        &component_generation,
                        &mut sessions,
                        &mut initial_child,
                        operation,
                        startup_animation.as_ref(),
                    )
                },
            )?
        } else {
            Workspace::with_recovered_sessions(
                runtime.to_path_buf(),
                launch_directory,
                recovered_workspace_sessions(&sessions)?,
                popup_catalog,
            )?
        })
    } else {
        None
    };
    let mut state = SupervisorState {
        component_generation,
        application_id: application_id.into(),
        decorations,
        workspace,
        sessions,
        venus: None,
        initial_child,
        initial_status: None,
        stopping: false,
        codex_quota: (mode == LaunchMode::Workspace).then(codex_quota::Provider::start),
    };
    let presentation = match admitted {
        Some(venus) => Ok(venus),
        None => PresentationProcess::start(
            venus_command(
                inputs,
                config,
                &presentation_socket,
                mode,
                &terminal,
                application_id,
                decorations,
            ),
            false,
            None,
            deadline,
        ),
    };
    state.venus = match presentation {
        Ok(venus) => Some(venus),
        Err(error) => {
            let session_cleanup = stop_managed_sessions(&mut state.sessions, SESSION_START_TIMEOUT);
            drop(control_listener);
            if session_cleanup.is_ok() {
                let _ = fs::remove_dir(runtime);
            }
            let mut errors = vec![error];
            if let Err(cleanup) = session_cleanup {
                errors.push(format!("cannot roll back terminals: {cleanup}"));
            }
            return Err(errors.join("; "));
        }
    };

    let status = (|| {
        loop {
            if !state.stopping {
                reap_finished_sessions(&mut state, inputs, config)?;
            }

            if state.sessions.is_empty() {
                return Ok(state.initial_status.unwrap_or(0));
            }

            if !state.stopping && reap_presentation(&mut state, inputs, config)? {
                match mode {
                    LaunchMode::Workspace => eprintln!(
                        "Eon Desktop exited; terminals remain active. Run `eon attach {generation}` to reconnect."
                    ),
                    LaunchMode::Terminal => eprintln!(
                        "Eon Desktop exited; terminal remains active. Run `eonterm attach {generation}` to reconnect."
                    ),
                }
            }

            if control_listener.accept(|request| {
                dispatch_control_request(request, &mut state, inputs, config, runtime, generation)
            })? {
                return Ok(state.initial_status.unwrap_or(0));
            }
            thread::sleep(Duration::from_millis(25));
        }
    })();
    drop(control_listener);
    if let Some(process) = state.venus.take() {
        drop(process);
    }
    let popup_cleanup = stop_transient_popups(&mut state);
    let _ = fs::remove_dir(runtime);
    match (status, popup_cleanup) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}; cannot clean up transient popups: {cleanup}"
        )),
    }
}

struct SupervisorState {
    component_generation: String,
    application_id: String,
    decorations: bool,
    workspace: Option<Workspace>,
    sessions: Vec<RunningSession>,
    venus: Option<PresentationProcess>,
    initial_child: Vec<OsString>,
    initial_status: Option<i32>,
    stopping: bool,
    codex_quota: Option<codex_quota::Provider>,
}

impl SupervisorState {
    fn workspace_snapshot(&self) -> Option<Snapshot> {
        self.workspace.as_ref().map(|workspace| {
            let mut snapshot = workspace.snapshot();
            snapshot.codex_quota = self
                .codex_quota
                .as_ref()
                .and_then(codex_quota::Provider::snapshot);
            snapshot
        })
    }
}

fn reap_finished_sessions(
    state: &mut SupervisorState,
    inputs: &Inputs,
    config: &Path,
) -> Result<(), String> {
    let mut index = 0;
    while index < state.sessions.len() {
        if let Some(code) = session_finished(&mut state.sessions[index])? {
            let session = state.sessions.remove(index);
            if let Some(workspace) = &mut state.workspace {
                let component_generation = &state.component_generation;
                let sessions = &mut state.sessions;
                let initial_child = &mut state.initial_child;
                workspace
                    .session_exited(&session.id, |operation| {
                        apply_session_operation(
                            inputs,
                            config,
                            component_generation,
                            sessions,
                            initial_child,
                            operation,
                            None,
                        )
                    })
                    .map_err(|error| error.detail)?;
            }
            if session.id == "session-1" {
                state.initial_status = Some(code);
            }
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn stop_workspace_session(session: &mut RunningSession) -> Result<Option<i32>, String> {
    if let Some(status) = session_finished(session)? {
        return Ok(Some(status));
    }
    match stop_managed_sessions(std::slice::from_mut(session), SESSION_START_TIMEOUT) {
        Ok(_) => Ok(None),
        Err(stop_error) => match session_finished(session) {
            Ok(Some(status)) => Ok(Some(status)),
            Ok(None) => Err(stop_error),
            Err(reconcile_error) => Err(format!(
                "{stop_error}; cannot reconcile terminal after failed stop: {reconcile_error}"
            )),
        },
    }
}

fn close_workspace_tab(
    request_id: &str,
    tab: &str,
    state: &mut SupervisorState,
    inputs: &Inputs,
    config: &Path,
) -> Result<Snapshot, Failure> {
    reap_finished_sessions(state, inputs, config)
        .map_err(|detail| failure("tab-close-failed", detail))?;
    let sessions = state
        .workspace
        .as_mut()
        .ok_or_else(|| failure("workspace-unavailable", "EonTerm has no Eon workspace"))?
        .prepare_close_tab(request_id, tab)?;

    for id in sessions {
        let natural_status = stop_owned_session(&mut state.sessions, &id)
            .map_err(|detail| failure("tab-close-failed", format!("{id}: {detail}")))?;
        state
            .workspace
            .as_mut()
            .expect("workspace close retains its owner")
            .session_exited(&id, |_| {
                Err("closing a tab cannot start a replacement terminal".into())
            })?;
        if id == "session-1" && natural_status.is_some() {
            state.initial_status = natural_status;
        }
    }

    Ok(state
        .workspace_snapshot()
        .expect("tab close retains a non-final workspace"))
}

fn directory_picker_command(
    inputs: &Inputs,
    runtime: &Path,
    tab: &str,
    instance: &str,
    startup_animation: Option<&managed_environment::StartupAnimation>,
) -> Result<Vec<OsString>, String> {
    let session_bin = inputs
        .managed
        .shell_bin
        .as_ref()
        .ok_or("the installed Eon package has no directory picker")?;
    let mut command = vec![session_bin.join("eon-directory-picker").into_os_string()];
    if let Some(animation) = startup_animation {
        command.extend([
            OsString::from("__startup-anima"),
            animation.style.clone().into(),
            animation.duration_seconds.to_string().into(),
        ]);
    } else {
        command.push("__directory-picker".into());
    }
    command.extend([
        runtime.join("eon.sock").into_os_string(),
        tab.into(),
        instance.into(),
    ]);
    Ok(command)
}

#[allow(clippy::too_many_arguments)]
fn apply_session_operation(
    inputs: &Inputs,
    config: &Path,
    component_generation: &str,
    sessions: &mut Vec<RunningSession>,
    initial_child: &mut Vec<OsString>,
    operation: SessionOperation,
    startup_animation: Option<&managed_environment::StartupAnimation>,
) -> Result<(), String> {
    let (session, directory, command) = match operation {
        SessionOperation::Start {
            session,
            directory,
            command,
        } => (session, directory, command),
        SessionOperation::Stop(id) => return stop_owned_session(sessions, &id).map(|_| ()),
    };
    let child = match command {
        LaunchCommand::Project { tab, instance } => directory_picker_command(
            inputs,
            session
                .endpoint
                .parent()
                .ok_or("directory picker endpoint has no runtime directory")?,
            &tab,
            &instance,
            startup_animation,
        )?,
        LaunchCommand::Tool(argv) => argv,
        LaunchCommand::Pane if session.id == "session-1" => initial_child.clone(),
        LaunchCommand::Pane => Vec::new(),
    };
    let running = start_orbit(
        inputs,
        config,
        &session.endpoint,
        &session.id,
        component_generation,
        &directory,
        &child,
        Instant::now() + SESSION_START_TIMEOUT,
    )?;
    sessions.push(running);
    if session.id == "session-1" {
        initial_child.clear();
    }
    Ok(())
}

fn stop_owned_session(sessions: &mut Vec<RunningSession>, id: &str) -> Result<Option<i32>, String> {
    let index = sessions
        .iter()
        .position(|session| session.id == id)
        .ok_or_else(|| format!("terminal {id} is missing from the supervisor"))?;
    let status = stop_workspace_session(&mut sessions[index])?;
    sessions.remove(index);
    Ok(status)
}

fn cancel_transient_popups(
    state: &mut SupervisorState,
    inputs: &Inputs,
    config: &Path,
) -> Result<(), String> {
    let ids = state
        .workspace
        .as_ref()
        .map(Workspace::transient_sessions)
        .unwrap_or_default();
    for id in ids {
        stop_owned_session(&mut state.sessions, &id)?;
        let SupervisorState {
            component_generation,
            workspace,
            sessions,
            initial_child,
            ..
        } = state;
        workspace
            .as_mut()
            .ok_or("transient popup has no workspace owner")?
            .session_exited(&id, |operation| {
                apply_session_operation(
                    inputs,
                    config,
                    component_generation,
                    sessions,
                    initial_child,
                    operation,
                    None,
                )
            })
            .map_err(|error| error.detail)?;
    }
    Ok(())
}

fn stop_transient_popups(state: &mut SupervisorState) -> Result<(), String> {
    let ids = state
        .workspace
        .as_ref()
        .map(Workspace::transient_sessions)
        .unwrap_or_default();
    for id in ids {
        if state.sessions.iter().any(|session| session.id == id) {
            stop_owned_session(&mut state.sessions, &id)?;
        }
    }
    Ok(())
}

fn reap_presentation(
    state: &mut SupervisorState,
    inputs: &Inputs,
    config: &Path,
) -> Result<bool, String> {
    let exited = match state.venus.as_mut() {
        Some(process) => process
            .child
            .try_wait()
            .map_err(|error| format!("cannot observe Eon Desktop: {error}"))?
            .is_some(),
        None => false,
    };
    if exited {
        state.venus = None;
        cancel_transient_popups(state, inputs, config)?;
    }
    Ok(exited)
}

pub(super) fn supervisor_lock_path(root: &Path, generation: &str) -> PathBuf {
    root.join(format!("supervisor-{generation}.lock"))
}

fn open_supervisor_lifecycle_lock(path: &Path) -> Result<fs::File, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("cannot open Eon lifecycle lock {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "cannot inspect Eon lifecycle lock {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() || metadata.uid() != effective_uid() {
        return Err(format!(
            "Eon lifecycle lock {} must be an owned regular file",
            path.display()
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            format!(
                "cannot protect Eon lifecycle lock {}: {error}",
                path.display()
            )
        })?;
    Ok(file)
}

pub(super) fn lock_supervisor_lifecycle(path: &Path) -> Result<fs::File, String> {
    let file = open_supervisor_lifecycle_lock(path)?;
    let deadline = Instant::now() + SESSION_START_TIMEOUT.saturating_add(Duration::from_secs(1));
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "timed out waiting for Eon supervisor lifecycle {}",
                    path.display()
                ));
            }
            Err(fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "cannot lock Eon supervisor lifecycle {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

pub(super) fn try_lock_supervisor_lifecycle(path: &Path) -> Result<Option<fs::File>, String> {
    let file = open_supervisor_lifecycle_lock(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "cannot lock Eon supervisor lifecycle {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn dispatch_control_request(
    request: Request,
    state: &mut SupervisorState,
    inputs: &Inputs,
    config: &Path,
    runtime: &Path,
    generation: &str,
) -> (ControlResponse, bool) {
    let mode = if state.workspace.is_some() {
        LaunchMode::Workspace
    } else {
        LaunchMode::Terminal
    };
    if state.stopping
        && !matches!(
            request.action,
            Action::Workspace(
                WorkspaceAction::InspectRuntime
                    | WorkspaceAction::InspectPresentation
                    | WorkspaceAction::Stop { .. }
            )
        )
    {
        let error = failure(
            "stop-incomplete",
            "generation cleanup is incomplete; retry Stop",
        );
        return (
            match request.action {
                Action::Workspace(WorkspaceAction::Present { .. }) => {
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(error))
                }
                _ => ControlResponse::Workspace(Response::Failure(error)),
            },
            false,
        );
    }
    match request {
        Request {
            action:
                Action::Workspace(
                    WorkspaceAction::InspectRuntime | WorkspaceAction::InspectPresentation,
                ),
            ..
        } => match runtime_status(inputs, generation, &state.sessions, mode) {
            Ok(mut runtime) => {
                if state.stopping {
                    runtime.attach = Availability {
                        available: false,
                        reason: "generation cleanup is incomplete; retry Stop".into(),
                    };
                }
                (
                    ControlResponse::Lifecycle(LifecycleResponse::Runtime(runtime)),
                    false,
                )
            }
            Err(detail) => (
                ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                    "unrepresentable-state",
                    detail,
                ))),
                false,
            ),
        },
        Request {
            action:
                Action::Workspace(WorkspaceAction::Present {
                    workspace: expected,
                }),
            ..
        } => {
            if expected != (mode == LaunchMode::Workspace) {
                return (
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                        "launch-mode-mismatch",
                        format!("supervisor owns {} mode", mode.name()),
                    ))),
                    false,
                );
            }
            if let Err(detail) = reap_finished_sessions(state, inputs, config) {
                return (
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                        "presentation-unavailable",
                        detail,
                    ))),
                    false,
                );
            }
            if state.sessions.is_empty() {
                return (
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                        "generation-ending",
                        "the last terminal exited while Eon Desktop was reopening",
                    ))),
                    true,
                );
            }
            let result = reap_presentation(state, inputs, config).and_then(|_| {
                if let Some(venus) = &state.venus {
                    return venus.present();
                }
                let socket = if mode == LaunchMode::Workspace {
                    runtime.join("eon.sock")
                } else {
                    state
                        .sessions
                        .first()
                        .ok_or("EonTerm has no live terminal")?
                        .endpoint
                        .clone()
                };
                let terminal =
                    managed_environment::terminal_presentation(config, &inputs.defaults)?;
                let command = venus_command(
                    inputs,
                    config,
                    &socket,
                    mode,
                    &terminal,
                    &state.application_id,
                    state.decorations,
                );
                state.venus = Some(PresentationProcess::start(
                    command,
                    terminal.requires_startup_admission(),
                    state.workspace_snapshot(),
                    Instant::now() + SESSION_START_TIMEOUT,
                )?);
                Ok(())
            });
            match result.and_then(|()| runtime_status(inputs, generation, &state.sessions, mode)) {
                Ok(runtime) => (
                    ControlResponse::Lifecycle(LifecycleResponse::Runtime(runtime)),
                    false,
                ),
                Err(detail) => (
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                        "presentation-unavailable",
                        detail,
                    ))),
                    false,
                ),
            }
        }
        Request {
            action: Action::Workspace(WorkspaceAction::Stop { generation: target }),
            ..
        } => {
            if target != generation {
                return (
                    ControlResponse::Lifecycle(LifecycleResponse::Failure(failure(
                        "generation-mismatch",
                        format!("supervisor owns generation {generation}, not {target}"),
                    ))),
                    false,
                );
            }
            state.stopping = true;
            let deadline = Instant::now() + SESSION_START_TIMEOUT;
            let result = acquire_generation_sessions(
                runtime,
                mode,
                &state.component_generation,
                &state.sessions,
                deadline,
            )
            .and_then(|untracked| {
                state.sessions.extend(untracked);
                stop_managed_sessions(
                    &mut state.sessions,
                    deadline.saturating_duration_since(Instant::now()),
                )
            });
            let shutdown = result.is_ok();
            let response = match result {
                Ok(mut sessions) => {
                    sessions.retain(|session| !workspace::is_directory_picker_session(session));
                    state.sessions.clear();
                    LifecycleResponse::Stopped(Stopped {
                        generation: generation.into(),
                        sessions,
                    })
                }
                Err(detail) => LifecycleResponse::Failure(failure("stop-failed", detail)),
            };
            (ControlResponse::Lifecycle(response), shutdown)
        }
        Request {
            id,
            action: Action::Workspace(WorkspaceAction::CloseTab { tab }),
        } => match close_workspace_tab(&id, &tab, state, inputs, config) {
            Ok(snapshot) => (
                ControlResponse::Workspace(Response::Snapshot(snapshot)),
                false,
            ),
            Err(error) => (ControlResponse::Workspace(Response::Failure(error)), false),
        },
        request => {
            let Some(workspace) = &mut state.workspace else {
                return (
                    ControlResponse::Workspace(Response::Failure(failure(
                        "workspace-unavailable",
                        "EonTerm has no Eon workspace",
                    ))),
                    false,
                );
            };
            let component_generation = &state.component_generation;
            let sessions = &mut state.sessions;
            let initial_child = &mut state.initial_child;
            match workspace.dispatch(
                &request.id,
                request.action,
                |command, directory| {
                    managed_environment::prepare_popup_command(
                        command,
                        inputs.managed.shell_bin.as_deref(),
                        directory,
                        &inputs.defaults,
                    )
                },
                |operation| {
                    apply_session_operation(
                        inputs,
                        config,
                        component_generation,
                        sessions,
                        initial_child,
                        operation,
                        None,
                    )
                },
            ) {
                Ok(()) => (
                    ControlResponse::Workspace(Response::Snapshot(
                        state
                            .workspace_snapshot()
                            .expect("workspace dispatch retains its owner"),
                    )),
                    false,
                ),
                Err(error) => (
                    ControlResponse::Workspace(Response::Failure(failure(
                        error.code,
                        error.detail,
                    ))),
                    false,
                ),
            }
        }
    }
}

fn runtime_status(
    inputs: &Inputs,
    generation: &str,
    sessions: &[RunningSession],
    mode: LaunchMode,
) -> Result<Runtime, String> {
    if mode == LaunchMode::Terminal && sessions.is_empty() {
        return Err("supervisor has no live terminals".into());
    }
    Ok(Runtime {
        generation: generation.into(),
        eon_version: inputs.version.into(),
        workspace_protocol: VERSION,
        component_report: inputs.components()?.report.clone(),
        sessions: sessions
            .iter()
            .filter(|session| {
                !session.is_finished() && !workspace::is_directory_picker_session(&session.id)
            })
            .map(|session| session.id.clone())
            .collect(),
        attach: Availability {
            available: true,
            reason: match mode {
                LaunchMode::Workspace => "supervisor accepts EONW v7 presentation requests",
                LaunchMode::Terminal => "supervisor owns one EonTerm terminal",
            }
            .into(),
        },
        stop: Availability {
            available: true,
            reason: "generation-aware supervisor owns these terminals".into(),
        },
    })
}

pub(super) fn stop(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub(super) fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchMode, effective_uid, prepare_configuration, prepare_runtime, temporary_directory,
        venus_command, xdg_path,
    };
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };

    #[test]
    fn startup_snapshot_transfer_uses_the_admission_deadline() {
        use eon_workspace_protocol::v7::{
            Pane, PopupGeometry, Response, Snapshot, Tab, encode_response,
        };
        use std::{
            process::Command,
            time::{Duration, Instant},
        };

        let root = temporary_directory();
        let snapshot = Snapshot {
            active_tab: "t1".into(),
            geometry: PopupGeometry {
                side_margin: 8.0,
                vertical_margin: 4.0,
            },
            entries: Vec::new(),
            tabs: (1..=64)
                .map(|number| Tab {
                    id: format!("t{number}"),
                    directory: vec![b'/'; 4096],
                    pending: false,
                    selected_pane: Some(format!("p{number}")),
                    selected_popup: None,
                    panes: vec![Pane {
                        id: format!("p{number}"),
                        session: format!("session-{number}"),
                        endpoint: format!("/runtime/orbit-{number}.sock").into_bytes(),
                        live: true,
                    }],
                    popups: Vec::new(),
                })
                .collect(),
            codex_quota: None,
        };
        let expected = encode_response(&Response::Snapshot(snapshot.clone())).unwrap();
        let received = root.join("snapshot");
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "sleep 0.6; dd bs=\"$1\" count=1 iflag=fullblock of=\"$2\" status=none || exit; printf ready-v1; exec cat >/dev/null",
            "venus",
        ]).arg(expected.len().to_string()).arg(&received);
        let process = super::PresentationProcess::start(
            command,
            true,
            Some(snapshot.clone()),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(fs::read(received).unwrap(), expected);
        drop(process);

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 10"]);
        let started = Instant::now();
        assert!(
            super::PresentationProcess::start(
                command,
                true,
                Some(snapshot),
                started + Duration::from_millis(300),
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        fs::remove_dir_all(root).unwrap();
    }

    fn terminal_presentation(
        background_opacity: f32,
        background_blur: bool,
    ) -> crate::managed_environment::TerminalConfig {
        crate::managed_environment::TerminalConfig {
            background_opacity,
            background_blur,
            ..crate::fixtures::inputs().defaults.terminal
        }
    }

    #[test]
    fn configured_typography_reaches_both_product_launches() {
        let inputs = crate::fixtures::inputs();
        let root = temporary_directory();
        fs::write(
            root.join("config.toml"),
            r#"
[terminal]
font_family = "DejaVu Sans Mono"
font_fallbacks = ["DejaVu Sans", "Symbols Nerd Font Mono"]
font_size = 20.5
line_height = 1.5
columns = 100
rows = 30
"#,
        )
        .unwrap();
        for mode in [LaunchMode::Workspace, LaunchMode::Terminal] {
            let terminal =
                crate::managed_environment::terminal_presentation(&root, &inputs.defaults).unwrap();
            let command = venus_command(
                &inputs,
                &root,
                Path::new("/orbit.sock"),
                mode,
                &terminal,
                "eon",
                true,
            );
            let args = command
                .get_args()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>();
            assert!(args.windows(14).any(|args| args
                == [
                    "--font-family",
                    "DejaVu Sans Mono",
                    "--font-fallback",
                    "DejaVu Sans",
                    "--font-fallback",
                    "Symbols Nerd Font Mono",
                    "--font-size",
                    "20.5",
                    "--line-height",
                    "1.5",
                    "--columns",
                    "100",
                    "--rows",
                    "30"
                ]));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_cursor_tail_reaches_both_product_launches() {
        let inputs = crate::fixtures::inputs();
        let root = temporary_directory();
        for value in ["random", "preset:ice", "custom:#12ABCF"] {
            fs::write(
                root.join("config.toml"),
                format!(
                    "[terminal]\ncursor_trail_color = '{value}'\ncursor_trail_duration = 2.5\n"
                ),
            )
            .unwrap();
            let terminal =
                crate::managed_environment::terminal_presentation(&root, &inputs.defaults).unwrap();
            assert!(terminal.requires_startup_admission());
            for mode in [LaunchMode::Workspace, LaunchMode::Terminal] {
                let command = venus_command(
                    &inputs,
                    &root,
                    Path::new("/runtime.sock"),
                    mode,
                    &terminal,
                    "eon",
                    true,
                );
                let args = command.get_args().collect::<Vec<_>>();
                assert!(
                    args.windows(2)
                        .any(|pair| pair == ["--cursor-trail-color", value]),
                    "cursor color did not reach Venus"
                );
                assert!(
                    args.windows(2)
                        .any(|pair| pair == ["--cursor-trail-duration-v1", "2.5"])
                );
            }
        }

        for source in ["", "[terminal]\ncursor_trail_duration = 3.0\n"] {
            fs::write(root.join("config.toml"), source).unwrap();
            let mut defaults = crate::fixtures::inputs().defaults;
            defaults.terminal.cursor_trail_duration = Some(2.0);
            let terminal =
                crate::managed_environment::terminal_presentation(&root, &defaults).unwrap();
            for mode in [LaunchMode::Workspace, LaunchMode::Terminal] {
                let command = venus_command(
                    &inputs,
                    &root,
                    Path::new("/runtime.sock"),
                    mode,
                    &terminal,
                    "eon",
                    true,
                );
                let args = command.get_args().collect::<Vec<_>>();
                let expected = if source.is_empty() { "2" } else { "3" };
                assert!(
                    args.windows(2)
                        .any(|pair| pair == ["--cursor-trail-duration-v1", expected])
                );
                assert!(!terminal.requires_startup_admission());
            }
        }

        fs::write(root.join("config.toml"), "").unwrap();
        let terminal =
            crate::managed_environment::terminal_presentation(&root, &inputs.defaults).unwrap();
        assert!(!terminal.requires_startup_admission());
        assert!(
            venus_command(
                &inputs,
                &root,
                Path::new("/runtime.sock"),
                LaunchMode::Workspace,
                &terminal,
                "eon",
                true,
            )
            .get_args()
            .all(|argument| argument != "--cursor-trail-color")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_pane_frames_reach_only_workspace_launches() {
        let inputs = crate::fixtures::inputs();
        let root = temporary_directory();
        for (source, expected) in [
            ("", "true"),
            ("[terminal]\npane_frames = true\n", "true"),
            ("[terminal]\npane_frames = false\n", "false"),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            let terminal =
                crate::managed_environment::terminal_presentation(&root, &inputs.defaults).unwrap();
            for mode in [LaunchMode::Workspace, LaunchMode::Terminal] {
                let command = venus_command(
                    &inputs,
                    &root,
                    Path::new("/runtime.sock"),
                    mode,
                    &terminal,
                    "eon",
                    true,
                );
                let args = command.get_args().collect::<Vec<_>>();
                let values = args
                    .windows(2)
                    .filter(|pair| pair[0] == "--pane-frames")
                    .map(|pair| pair[1])
                    .collect::<Vec<_>>();
                if mode == LaunchMode::Workspace {
                    assert_eq!(values, [expected], "{source}");
                } else {
                    assert!(values.is_empty());
                }
            }
        }
        for value in ["\"false\"", "0", "true\npane_frames = false"] {
            fs::write(
                root.join("config.toml"),
                format!("[terminal]\npane_frames = {value}\n"),
            )
            .unwrap();
            assert!(
                crate::managed_environment::terminal_presentation(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains("pane_frames")
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn venus_receives_workspace_endpoint_only_in_workspace_mode() {
        let mut inputs = crate::fixtures::inputs();
        inputs.orbit = "/managed/orbit".into();
        inputs.venus = "/managed/venus".into();
        inputs.managed.shell_bin = None;
        let config = Path::new("/config/eon");
        let socket = Path::new("/runtime/eon.sock");

        let workspace = venus_command(
            &inputs,
            config,
            socket,
            LaunchMode::Workspace,
            &terminal_presentation(0.88, true),
            "eon",
            true,
        );
        assert_eq!(
            workspace.get_args().map(OsString::from).collect::<Vec<_>>(),
            [
                "--application-id",
                "eon",
                "--background-opacity",
                "0.88",
                "--background-blur",
                "--pane-frames",
                "true",
                "--workspace",
                "/runtime/eon.sock",
            ]
            .map(OsString::from)
        );

        let terminal = venus_command(
            &inputs,
            config,
            Path::new("/runtime/orbit.sock"),
            LaunchMode::Terminal,
            &terminal_presentation(1.0, false),
            "eonterm",
            true,
        );
        for command in [&workspace, &terminal] {
            let environment = command.get_envs().collect::<Vec<_>>();
            assert!(environment.iter().all(|(key, _)| *key != "XDG_CONFIG_HOME"));
            assert!(environment.iter().any(|(key, value)| {
                *key == "EON_CONFIG_HOME" && *value == Some(config.as_os_str())
            }));
        }
        assert_eq!(
            terminal.get_args().map(OsString::from).collect::<Vec<_>>(),
            [
                "--application-id",
                "eonterm",
                "--background-opacity",
                "1",
                "/runtime/orbit.sock",
            ]
            .map(OsString::from)
        );

        let undecorated = venus_command(
            &inputs,
            config,
            Path::new("/runtime/orbit.sock"),
            LaunchMode::Terminal,
            &terminal_presentation(0.0, true),
            "eonova",
            false,
        );
        assert_eq!(
            undecorated
                .get_args()
                .map(OsString::from)
                .collect::<Vec<_>>(),
            [
                "--no-decorations",
                "--application-id",
                "eonova",
                "--background-opacity",
                "0",
                "--background-blur",
                "/runtime/orbit.sock",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn runtime_directory_is_private() {
        let root = temporary_directory();
        let runtime = root.join("runtime");

        prepare_runtime(&runtime).unwrap();

        let metadata = fs::metadata(&runtime).unwrap();
        assert_eq!(metadata.uid(), effective_uid());
        assert_eq!(metadata.mode() & 0o777, 0o700);

        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            prepare_runtime(&runtime).unwrap_err(),
            format!(
                "runtime directory {} must be an owned private directory",
                runtime.display()
            )
        );
        assert_eq!(fs::metadata(&runtime).unwrap().mode() & 0o777, 0o777);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configuration_directory_is_private_only_when_created() {
        let root = temporary_directory();
        let config = root.join("config");

        prepare_configuration(&config).unwrap();
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o700);

        fs::set_permissions(&config, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_configuration(&config).unwrap();
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o755);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn xdg_paths_must_be_absolute() {
        assert_eq!(xdg_path(Some("relative".into())), None);

        let absolute = PathBuf::from("/absolute");
        assert_eq!(xdg_path(Some(absolute.clone())), Some(absolute));
    }
}
