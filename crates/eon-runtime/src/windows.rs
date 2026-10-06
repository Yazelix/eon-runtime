use super::{
    cli::Inputs,
    control::write_stdout,
    generation::{current_generation, generation_directory, private_directory_exists},
    supervisor::{
        LaunchMode, SESSION_START_TIMEOUT, base_runtime_directory, prepare_runtime,
        probe_supervisor, request_id, runtime_directory, validate_private_directory,
    },
    workspace::json_escape,
};
use std::{
    collections::hash_map::RandomState,
    env,
    ffi::OsStr,
    fs::{self, DirBuilder, OpenOptions},
    hash::BuildHasher,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const WINDOW_RUNTIME: &str = "EON_WINDOW_RUNTIME_DIR";

fn valid_window_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn windows_root() -> PathBuf {
    base_runtime_directory("eon").join("w")
}

fn existing_windows() -> Result<Vec<(String, PathBuf)>, String> {
    let base = base_runtime_directory("eon");
    if !private_directory_exists(&base, "runtime directory")? {
        return Ok(Vec::new());
    }
    let parent = windows_root();
    if !private_directory_exists(&parent, "window directory")? {
        return Ok(Vec::new());
    }
    let mut windows = Vec::new();
    for entry in fs::read_dir(&parent)
        .map_err(|error| format!("cannot list {}: {error}", parent.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot inspect Eon window: {error}"))?;
        let id = entry.file_name();
        let id = id.to_str().ok_or("Eon window ID is not UTF-8")?;
        if !valid_window_id(id) {
            return Err(format!(
                "invalid Eon window ID {id:?} in {}",
                parent.display()
            ));
        }
        validate_private_directory(&entry.path())?;
        windows.push((id.to_owned(), entry.path()));
    }
    windows.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(windows)
}

fn selected_window(id: &OsStr) -> Result<PathBuf, String> {
    let id = id.to_str().ok_or("Eon window ID must be UTF-8")?;
    if !valid_window_id(id) {
        return Err(format!("invalid Eon window ID {id:?}"));
    }
    let parent = windows_root();
    validate_private_directory(&base_runtime_directory("eon"))?;
    validate_private_directory(&parent)?;
    let root = parent.join(id);
    validate_private_directory(&root)?;
    Ok(root)
}

fn window_command(root: &Path) -> Result<Command, String> {
    let binary = env::current_exe()
        .map_err(|error| format!("cannot find the running Eon executable: {error}"))?;
    let mut command = Command::new(binary);
    command.env(WINDOW_RUNTIME, root);
    Ok(command)
}

pub(super) fn new_window(inputs: &Inputs) -> Result<i32, String> {
    let generation = current_generation(inputs)?;
    let base = base_runtime_directory("eon");
    prepare_runtime(&base)?;
    let parent = windows_root();
    prepare_runtime(&parent)?;
    let (id, root) = loop {
        let id = format!("{:08x}", RandomState::new().hash_one(request_id()) as u32);
        let root = parent.join(&id);
        if generation_directory(&root, &generation)
            .join("session-9999.sock.management")
            .as_os_str()
            .as_bytes()
            .len()
            >= 108
        {
            return Err(format!(
                "Eon runtime root {} is too long for independent Session sockets",
                base.display()
            ));
        }
        match DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => break (id, root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot reserve Eon window {id}: {error}")),
        }
    };
    let log = root.join("launch.log");
    let stderr = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log)
        .map_err(|error| format!("cannot create {}: {error}", log.display()))?;
    let mut command = window_command(&root)?;
    command
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe and has no parent-process side effects.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start Eon window {id}: {error}"))?;
    let socket = generation_directory(&root, &generation).join("eon.sock");
    let deadline = Instant::now() + SESSION_START_TIMEOUT.saturating_mul(4);
    loop {
        if let Ok((LaunchMode::Workspace, _)) = probe_supervisor(inputs, &socket, &generation) {
            write_stdout(format!("{id}\n"))?;
            return Ok(0);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot observe Eon window {id}: {error}"))?
        {
            let detail = fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "Eon window {id} exited {status}: {}",
                detail.trim().chars().take(1000).collect::<String>()
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Eon window {id} did not become ready; its process and log remain at {} for inspection",
                root.display()
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub(super) fn window_action(id: &OsStr, stop: bool, json: bool) -> Result<i32, String> {
    let root = selected_window(id)?;
    let mut command = window_command(&root)?;
    command.args(if stop {
        &["stop", "all"][..]
    } else {
        &["attach"][..]
    });
    if json {
        command.arg("--json");
    }
    let status = command
        .status()
        .map_err(|error| format!("cannot run Eon window action: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

pub(super) fn stop_all_windows() -> Result<i32, String> {
    let current = runtime_directory("eon");
    let mut windows = existing_windows()?;
    windows.sort_by_key(|(_, root)| root == &current);
    let mut status = 0;
    for (id, _) in windows {
        eprintln!("Stop Eon window {id}:");
        status = status.max(window_action(OsStr::new(&id), true, false)?);
    }
    Ok(status)
}

pub(super) fn list_windows(json: bool) -> Result<i32, String> {
    let mut output = if json {
        String::from("{\"windows\":[")
    } else {
        String::new()
    };
    for (index, (id, root)) in existing_windows()?.iter().enumerate() {
        let listing = window_command(root)?
            .args(["generations", "--json"])
            .output()
            .map_err(|error| format!("cannot inspect Eon window {id}: {error}"))?;
        if !listing.status.success() {
            return Err(format!(
                "cannot inspect Eon window {id}: {}",
                String::from_utf8_lossy(&listing.stderr)
            ));
        }
        let generations = String::from_utf8(listing.stdout)
            .map_err(|_| format!("Eon window {id} returned non-UTF-8 generations"))?;
        if json {
            if index > 0 {
                output.push(',');
            }
            output.push_str(&format!(
                "{{\"id\":\"{}\",\"inventory\":{}}}",
                json_escape(id),
                generations.trim()
            ));
        } else {
            output.push_str(&format!("window {id}\n{generations}"));
        }
    }
    if json {
        output.push_str("]}\n");
    } else if output.is_empty() {
        output.push_str("no Eon windows\n");
    }
    write_stdout(output)?;
    Ok(0)
}
