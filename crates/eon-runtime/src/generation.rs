use super::cli::Inputs;
use super::control::{
    ControlResponse, EndpointFailure, EndpointFailureKind, connect_control, failure,
    probe_generation_runtime, remove_socket_if_identity, report_failure, send_generation_action_on,
    send_legacy_inspect, socket_identity, socket_identity_from, write_stdout,
};
use super::supervisor::{
    effective_uid, path_exists, present_at, probe_supervisor, runtime_directory,
    supervisor_lock_path, try_lock_supervisor_lifecycle, validate_private_directory,
};
use super::workspace::json_escape;
use eon_workspace_protocol::v7::{
    Availability, LifecycleResponse, Stopped, VERSION, WorkspaceAction,
};
use std::{
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
};

pub(super) fn current_generation(inputs: &Inputs) -> Result<String, String> {
    inputs.components()?;
    let mut sources: Vec<&[u8]> = vec![
        include_bytes!("lib.rs"),
        include_bytes!("../Cargo.toml"),
        include_bytes!("cli.rs"),
        include_bytes!("codex_quota.rs"),
        include_bytes!("control.rs"),
        include_bytes!("generation.rs"),
        include_bytes!("managed_environment.rs"),
        include_bytes!("sessions.rs"),
        include_bytes!("supervisor.rs"),
        include_bytes!("workspace.rs"),
        include_bytes!("windows.rs"),
        include_bytes!("../../eon-workspace-protocol/src/lib.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v2.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v3.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v4.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v5.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v6.rs"),
        include_bytes!("../../eon-workspace-protocol/src/v7.rs"),
        include_bytes!("../../eon-workspace-protocol/Cargo.toml"),
    ];
    sources.extend_from_slice(inputs.assembly);
    Ok(generation_id(&sources))
}

fn generation_id(inputs: &[&[u8]]) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for input in inputs {
        for byte in (input.len() as u64).to_le_bytes().iter().chain(*input) {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    format!("g1-{hash:032x}")
}

pub(super) fn valid_generation(value: &str) -> bool {
    value.len() == 35
        && value.starts_with("g1-")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn generation_directory(root: &Path, generation: &str) -> PathBuf {
    root.join("generations").join(generation)
}

const MAX_GENERATIONS: usize = 256;
#[derive(Debug)]
struct GenerationRecord {
    id: String,
    kind: &'static str,
    state: &'static str,
    runtime: PathBuf,
    eon_version: Option<String>,
    workspace_protocol: Option<u16>,
    component_report: Option<String>,
    sessions: Vec<String>,
    attach: Availability,
    stop: Availability,
    detail: String,
}

fn discover_generations(
    inputs: &Inputs,
    root: &Path,
    current: &str,
) -> Result<Vec<GenerationRecord>, String> {
    let component_report = &inputs.components()?.report;
    if !private_directory_exists(root, "runtime directory")? {
        return Ok(vec![unstarted_current_generation(root, current)]);
    }

    let parent = root.join("generations");
    let mut candidates = Vec::new();
    if private_directory_exists(&parent, "generation directory")? {
        for entry in fs::read_dir(&parent).map_err(|error| {
            format!(
                "cannot list generation directory {}: {error}",
                parent.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!(
                    "cannot read generation entry in {}: {error}",
                    parent.display()
                )
            })?;
            candidates.push((entry.file_name(), entry.path()));
            if candidates.len() > MAX_GENERATIONS {
                return Err(format!(
                    "generation directory {} exceeds the {MAX_GENERATIONS}-entry inspection limit",
                    parent.display()
                ));
            }
        }
    }
    candidates.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

    let current_path = generation_directory(root, current);
    let mut records = vec![inspect_generation(
        current,
        "current",
        current_path,
        &supervisor_lock_path(root, current),
        component_report,
    )];
    for (id, path) in candidates {
        let id = id.to_string_lossy();
        if id != current {
            records.push(inspect_generation(
                &id,
                "previous",
                path,
                &supervisor_lock_path(root, &id),
                component_report,
            ));
        }
    }
    if path_exists(&root.join("eon.sock")) || path_exists(&root.join("orbit.sock")) {
        records.push(inspect_legacy(root));
    }
    Ok(records)
}

fn inspect_selected_generation(
    inputs: &Inputs,
    root: &Path,
    current: &str,
    target: &str,
) -> Result<Option<GenerationRecord>, String> {
    let missing_current =
        || (target == current).then(|| unstarted_current_generation(root, current));
    if !private_directory_exists(root, "runtime directory")? {
        return Ok(missing_current());
    }
    if target == "legacy" {
        return Ok(
            (path_exists(&root.join("eon.sock")) || path_exists(&root.join("orbit.sock")))
                .then(|| inspect_legacy(root)),
        );
    }

    let parent = root.join("generations");
    if !private_directory_exists(&parent, "generation directory")? {
        return Ok(missing_current());
    }
    let runtime = generation_directory(root, target);
    if target != current
        && matches!(
            fs::symlink_metadata(&runtime),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    {
        return Ok(None);
    }
    let component_report = &inputs.components()?.report;
    Ok(Some(inspect_generation(
        target,
        if target == current {
            "current"
        } else {
            "previous"
        },
        runtime,
        &supervisor_lock_path(root, target),
        component_report,
    )))
}

fn inspect_generation(
    id: &str,
    kind: &'static str,
    runtime: PathBuf,
    lifecycle_lock: &Path,
    current_components: &str,
) -> GenerationRecord {
    if !valid_generation(id) {
        return failed_generation(
            id,
            kind,
            runtime,
            EndpointFailure::new(
                EndpointFailureKind::Corrupt,
                "generation directory name is not a valid Eon identity",
            ),
        );
    }
    if let Err(detail) = validate_private_directory(&runtime) {
        return if matches!(
            fs::symlink_metadata(&runtime),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            dead_generation(id, kind, runtime, "generation has not started")
        } else {
            failed_generation(
                id,
                kind,
                runtime,
                EndpointFailure::new(EndpointFailureKind::Corrupt, detail),
            )
        };
    }

    let socket = runtime.join("eon.sock");
    match probe_generation_runtime(&socket) {
        Ok(info) if info.generation != id => failed_generation(
            id,
            kind,
            runtime,
            EndpointFailure::new(
                EndpointFailureKind::Corrupt,
                format!(
                    "supervisor reports generation {} from directory {id}",
                    info.generation
                ),
            ),
        ),
        Ok(mut info) => {
            if info.component_report != current_components {
                info.attach = Availability {
                    available: false,
                    reason: "component graph differs from the current Eon build".into(),
                };
            }
            if info.workspace_protocol != VERSION {
                info.attach = Availability {
                    available: false,
                    reason: format!(
                        "supervisor uses EONW {}, current Eon requires EONW {VERSION}",
                        info.workspace_protocol
                    ),
                };
            }
            GenerationRecord {
                id: id.into(),
                kind,
                state: "live",
                runtime,
                eon_version: Some(info.eon_version),
                workspace_protocol: Some(info.workspace_protocol),
                component_report: Some(info.component_report),
                sessions: info.sessions,
                attach: info.attach,
                stop: info.stop,
                detail: "live supervisor validated its generation identity".into(),
            }
        }
        Err(error) => {
            let dead = error.kind == EndpointFailureKind::Dead;
            let record = failed_generation(id, kind, runtime.clone(), error);
            if dead && let Ok(Some(_lifecycle_lock)) = try_lock_supervisor_lifecycle(lifecycle_lock)
            {
                remove_dead_socket(&socket);
                let _ = fs::remove_dir(&runtime);
            }
            record
        }
    }
}

fn inspect_legacy(root: &Path) -> GenerationRecord {
    let runtime = root.to_path_buf();
    let socket = root.join("eon.sock");
    match send_legacy_inspect(&socket) {
        Ok(eon_workspace_protocol::v4::Response::Snapshot(snapshot)) => GenerationRecord {
            id: "legacy".into(),
            kind: "legacy",
            state: "live",
            runtime,
            eon_version: None,
            workspace_protocol: Some(eon_workspace_protocol::v4::VERSION),
            component_report: None,
            sessions: snapshot
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter().map(|pane| pane.session.clone()))
                .collect(),
            attach: Availability {
                available: false,
                reason: format!(
                    "legacy supervisor uses EONW v4; current Eon requires EONW v{VERSION}"
                ),
            },
            stop: Availability {
                available: false,
                reason: "legacy supervisor has no authoritative stop action".into(),
            },
            detail: "live fixed-namespace supervisor; component identity unavailable".into(),
        },
        Ok(eon_workspace_protocol::v4::Response::Failure(failure)) => failed_generation(
            "legacy",
            "legacy",
            runtime,
            EndpointFailure::new(
                if failure.code == "unsupported-version" {
                    EndpointFailureKind::Incompatible
                } else {
                    EndpointFailureKind::Corrupt
                },
                format!(
                    "legacy supervisor rejected EONW inspection: {}",
                    failure.detail
                ),
            ),
        ),
        Err(error) => failed_generation("legacy", "legacy", runtime, error),
    }
}

fn dead_generation(
    id: &str,
    kind: &'static str,
    runtime: PathBuf,
    detail: impl Into<String>,
) -> GenerationRecord {
    failed_generation(
        id,
        kind,
        runtime,
        EndpointFailure::new(EndpointFailureKind::Dead, detail),
    )
}

fn unstarted_current_generation(root: &Path, current: &str) -> GenerationRecord {
    dead_generation(
        current,
        "current",
        generation_directory(root, current),
        "current generation has not started",
    )
}

fn failed_generation(
    id: &str,
    kind: &'static str,
    runtime: PathBuf,
    error: EndpointFailure,
) -> GenerationRecord {
    let state = match error.kind {
        EndpointFailureKind::Dead => "dead",
        EndpointFailureKind::Incompatible | EndpointFailureKind::UnsupportedVersion(_) => {
            "incompatible"
        }
        EndpointFailureKind::InvalidAction | EndpointFailureKind::Corrupt => "corrupt",
        EndpointFailureKind::Unreachable => "unreachable",
    };
    GenerationRecord {
        id: id.into(),
        kind,
        state,
        runtime,
        eon_version: None,
        workspace_protocol: None,
        component_report: None,
        sessions: Vec::new(),
        attach: Availability {
            available: false,
            reason: error.detail.clone(),
        },
        stop: Availability {
            available: false,
            reason: error.detail.clone(),
        },
        detail: error.detail,
    }
}

pub(super) fn private_directory_exists(path: &Path, description: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path).map(|()| true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "cannot inspect {description} {}: {error}",
            path.display()
        )),
    }
}

fn remove_dead_socket(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
        return;
    }
    let identity = socket_identity_from(&metadata);
    if matches!(
        UnixStream::connect(path),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    ) {
        remove_socket_if_identity(path, identity);
    }
}

pub(super) fn generations_command(
    inputs: &Inputs,
    json: bool,
    product: &str,
) -> Result<i32, String> {
    let records = discover_generations(
        inputs,
        &runtime_directory(product),
        &current_generation(inputs)?,
    )?;
    write_stdout(if json {
        generations_json(&records)
    } else {
        generations_human(&records)
    })?;
    Ok(0)
}

fn generations_human(records: &[GenerationRecord]) -> String {
    let mut output = String::new();
    for record in records {
        output.push_str(&format!(
            "{} {} {} terminals={}\n",
            record.kind,
            record.id.escape_debug(),
            record.state,
            record.sessions.len()
        ));
        output.push_str(&format!(
            "  attach={} {}\n  stop={} {}\n  {}\n",
            record.attach.available,
            record.attach.reason.escape_debug(),
            record.stop.available,
            record.stop.reason.escape_debug(),
            record.detail.escape_debug()
        ));
        if let (Some(version), Some(protocol)) = (&record.eon_version, record.workspace_protocol) {
            output.push_str(&format!(
                "  eon={} eonw={protocol}\n",
                version.escape_debug()
            ));
        }
        for session in &record.sessions {
            output.push_str(&format!("  terminal {session}\n"));
        }
        if let Some(report) = &record.component_report {
            for line in report.lines() {
                output.push_str(&format!("  component {}\n", line.escape_debug()));
            }
        }
    }
    output
}

fn generations_json(records: &[GenerationRecord]) -> String {
    let mut output = String::from("{\"generations\":[");
    for (index, record) in records.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!(
            "{{\"id\":\"{}\",\"kind\":\"{}\",\"state\":\"{}\",\"eon_version\":{},\"workspace_protocol\":{},\"component_report\":{},\"sessions\":[",
            json_escape(&record.id),
            record.kind,
            record.state,
            json_option(record.eon_version.as_deref()),
            record.workspace_protocol.map_or_else(|| "null".into(), |value| value.to_string()),
            json_option(record.component_report.as_deref()),
        ));
        for (session_index, session) in record.sessions.iter().enumerate() {
            if session_index != 0 {
                output.push(',');
            }
            output.push_str(&format!("\"{}\"", json_escape(session)));
        }
        output.push_str(&format!(
            "],\"attach\":{{\"available\":{},\"reason\":\"{}\"}},\"stop\":{{\"available\":{},\"reason\":\"{}\"}},\"detail\":\"{}\"}}",
            record.attach.available,
            json_escape(&record.attach.reason),
            record.stop.available,
            json_escape(&record.stop.reason),
            json_escape(&record.detail),
        ));
    }
    output.push_str("]}\n");
    output
}

fn json_option(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".into(),
        |value| format!("\"{}\"", json_escape(value)),
    )
}

pub(super) fn attach_generation(
    inputs: &Inputs,
    target: Option<&str>,
    product: &str,
) -> Result<i32, String> {
    let root = runtime_directory(product);
    let current = current_generation(inputs)?;
    let target = target.unwrap_or(&current);
    let record = inspect_selected_generation(inputs, &root, &current, target)?
        .ok_or_else(|| format!("generation {target} was not found"))?;
    if !record.attach.available {
        return Err(format!(
            "generation {target} is not attachable: {}",
            record.attach.reason
        ));
    }
    let (mode, supervisor) = probe_supervisor(inputs, &record.runtime.join("eon.sock"), target)
        .map_err(|error| error.detail)?;
    present_at(inputs, &record.runtime, target, mode, supervisor)
}

pub(super) fn stop_generation(
    inputs: &Inputs,
    target: &str,
    json: bool,
    product: &str,
) -> Result<i32, String> {
    let root = runtime_directory(product);
    let current = current_generation(inputs)?;
    let control = generation_directory(&root, target).join("eon.sock");
    let observed_supervisor = socket_identity(&control);
    let Some(record) = inspect_selected_generation(inputs, &root, &current, target)? else {
        return report_failure(
            &failure(
                "unknown-generation",
                format!("generation {target} was not found"),
            ),
            json,
        );
    };
    if !record.stop.available {
        return report_failure(
            &failure(
                "stop-unavailable",
                format!("generation {target}: {}", record.stop.reason),
            ),
            json,
        );
    }
    let supervisor = match observed_supervisor {
        Ok(Some(supervisor))
            if socket_identity(&control).is_ok_and(|current| current == Some(supervisor)) =>
        {
            supervisor
        }
        _ => {
            return report_failure(
                &failure(
                    "stop-failed",
                    "supervisor endpoint changed while stop was being validated",
                ),
                json,
            );
        }
    };
    if !json {
        eprint!(
            "Stop generation {target} and {} live terminal{} [{}]? [y/N] ",
            record.sessions.len(),
            if record.sessions.len() == 1 { "" } else { "s" },
            record.sessions.join(", ")
        );
        let answer = read_stop_confirmation()?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            write_stdout(format!("cancelled; no Stop sent for generation {target}\n"))?;
            return Ok(0);
        }
    }

    let stream = match connect_control(&control) {
        Ok(stream) => stream,
        Err(error) => return report_failure(&failure("stop-failed", error.detail), json),
    };
    if !socket_identity(&control).is_ok_and(|current| current == Some(supervisor)) {
        return report_failure(
            &failure(
                "stop-failed",
                "supervisor endpoint changed before stop could be sent",
            ),
            json,
        );
    }
    let Some(version) = record.workspace_protocol else {
        return report_failure(
            &failure("stop-failed", "supervisor did not report its EONW version"),
            json,
        );
    };
    let response = match send_generation_action_on(
        stream,
        version,
        WorkspaceAction::Stop {
            generation: target.into(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            return report_failure(&failure("stop-failed", error.detail), json);
        }
    };
    match response {
        ControlResponse::Lifecycle(LifecycleResponse::Stopped(stopped))
            if stopped.generation == target =>
        {
            if json {
                write_stdout(stopped_json(&stopped))?;
            } else {
                write_stdout(format!(
                    "stopped generation {}: {}\n",
                    stopped.generation,
                    stopped.sessions.join(", ")
                ))?;
            }
            Ok(0)
        }
        ControlResponse::Lifecycle(LifecycleResponse::Failure(failure)) => {
            report_failure(&failure, json)
        }
        _ => report_failure(
            &failure(
                "stop-failed",
                "supervisor returned the wrong EONW result for stop",
            ),
            json,
        ),
    }
}

fn read_stop_confirmation() -> Result<String, String> {
    let mut answer = Vec::new();
    loop {
        let mut byte = 0u8;
        // SAFETY: read writes at most one byte into the valid local buffer.
        let result = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
        match result {
            0 => break,
            1 if byte == b'\n' => break,
            1 => answer.push(byte),
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(format!("cannot read stop confirmation: {error}"));
                }
            }
        }
    }
    String::from_utf8(answer)
        .map_err(|_| "cannot read stop confirmation: stream did not contain valid UTF-8".into())
}

pub(super) fn stop_generations(
    inputs: &Inputs,
    include_current: bool,
    json: bool,
    product: &str,
) -> Result<i32, String> {
    let mut records = discover_generations(
        inputs,
        &runtime_directory(product),
        &current_generation(inputs)?,
    )?;
    // The CLI may itself be running inside current; stop that generation last.
    records.sort_by_key(|record| record.kind == "current");
    if json {
        write_stdout("[")?;
    }
    let mut separator = "";
    let mut status = 0;
    for record in records {
        if record.state == "dead" || (!include_current && record.kind == "current") {
            continue;
        }
        if json {
            write_stdout(separator)?;
            separator = ",";
        }
        let result = match stop_generation(inputs, &record.id, json, product) {
            Ok(code) => code,
            Err(error) => report_failure(
                &failure("stop-failed", format!("generation {}: {error}", record.id)),
                json,
            )?,
        };
        status = status.max(result);
    }
    if json {
        write_stdout("]\n")?;
    }
    Ok(status)
}

fn stopped_json(stopped: &Stopped) -> String {
    let mut output = format!(
        "{{\"stopped\":{{\"generation\":\"{}\",\"sessions\":[",
        json_escape(&stopped.generation)
    );
    for (index, session) in stopped.sessions.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!("\"{}\"", json_escape(session)));
    }
    output.push_str("]}}\n");
    output
}

#[cfg(test)]
mod tests {
    use super::{
        current_generation, discover_generations, generation_directory, generation_id,
        valid_generation,
    };
    use crate::supervisor::{lock_supervisor_lifecycle, supervisor_lock_path, temporary_directory};
    use std::{
        fs,
        os::unix::{
            fs::{PermissionsExt, symlink},
            net::UnixListener,
        },
        path::Path,
        thread,
        time::Duration,
    };

    #[test]
    fn generation_identity_is_stable_bounded_and_namespaced() {
        let first = generation_id(&[b"ab".as_slice(), b"c".as_slice()]);
        assert_eq!(first, generation_id(&[b"ab".as_slice(), b"c".as_slice()]));
        assert_ne!(first, generation_id(&[b"a".as_slice(), b"bc".as_slice()]));
        assert!(valid_generation(&first));
        assert!(!valid_generation("legacy"));
        assert!(!valid_generation("g1-0123456789ABCDEF0123456789ABCDEF"));
        assert_eq!(
            generation_directory(Path::new("/runtime/eon"), &first),
            Path::new("/runtime/eon/generations").join(first)
        );
    }

    #[test]
    fn current_generation_consumes_supplied_assembly() {
        let mut inputs = crate::fixtures::inputs();
        let first = current_generation(&inputs).unwrap();
        assert_eq!(first, current_generation(&inputs).unwrap());
        inputs.assembly = &[b"changed-assembly"];
        assert_ne!(first, current_generation(&inputs).unwrap());
    }

    #[test]
    fn discovery_bounds_dead_cleanup_and_rejects_unsafe_metadata() {
        let root = temporary_directory();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let generations = root.join("generations");
        fs::create_dir(&generations).unwrap();
        fs::set_permissions(&generations, fs::Permissions::from_mode(0o700)).unwrap();

        let outside = temporary_directory();
        let linked_id = "g1-00000000000000000000000000000000";
        symlink(&outside, generations.join(linked_id)).unwrap();

        let dead_id = "g1-11111111111111111111111111111111";
        let dead = generations.join(dead_id);
        fs::create_dir(&dead).unwrap();
        fs::set_permissions(&dead, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(dead.join("eon.sock")).unwrap();
        fs::set_permissions(dead.join("eon.sock"), fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);

        let held = lock_supervisor_lifecycle(&supervisor_lock_path(&root, dead_id)).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let inspection_root = root.clone();
        let inspection = thread::spawn(move || {
            sender
                .send(discover_generations(
                    &crate::fixtures::inputs(),
                    &inspection_root,
                    &current_generation(&crate::fixtures::inputs()).unwrap(),
                ))
                .unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        let preserved = dead.exists();
        drop(held);
        inspection.join().unwrap();

        let records = result
            .expect("generation inspection waited for startup")
            .unwrap();
        assert!(records.iter().any(|record| {
            record.id == linked_id && record.kind == "previous" && record.state == "corrupt"
        }));
        assert!(records.iter().any(|record| {
            record.id == dead_id && record.kind == "previous" && record.state == "dead"
        }));
        assert!(
            preserved,
            "contended inspection removed dead control metadata"
        );

        discover_generations(
            &crate::fixtures::inputs(),
            &root,
            &current_generation(&crate::fixtures::inputs()).unwrap(),
        )
        .unwrap();
        assert!(!dead.exists());
        assert!(outside.is_dir());

        fs::remove_file(generations.join(linked_id)).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
