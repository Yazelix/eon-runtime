use super::{
    cli::Inputs,
    managed_environment,
    supervisor::{
        LaunchMode, SESSION_START_TIMEOUT, effective_uid, request_id, status_code, stop,
        validate_private_directory,
    },
    workspace::{is_directory_picker_endpoint, is_directory_picker_session},
};
use eon_workspace_protocol::v7::MAX_SESSIONS;
use orbit_protocol::management::{
    self as management, ClientMessage as ManagementClientMessage, EndpointIdentity, LiveIdentity,
    ObjectIdentity, ProcessOutcome, Record as ManagementRecord,
    ServerMessage as ManagementServerMessage, TerminationReason, Tombstone,
};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn session_number(value: &str) -> Option<usize> {
    let number = value.strip_prefix("session-")?;
    if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

fn managed_session_number(value: &str) -> Result<Option<usize>, String> {
    if value == "directory-picker" || is_directory_picker_session(value) {
        Ok(None)
    } else {
        session_number(value)
            .map(Some)
            .ok_or_else(|| format!("invalid terminal identity {value:?}"))
    }
}

struct RecordSnapshot {
    object: ObjectIdentity,
    record: ManagementRecord,
}

struct ReadyClaim {
    file: fs::File,
    path: PathBuf,
    object: ObjectIdentity,
}

impl ReadyClaim {
    fn create(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("terminal record {} has no parent", path.display()))?;
        validate_private_directory(parent)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|error| {
                format!(
                    "cannot create terminal Ready claim {}: {error}",
                    path.display()
                )
            })?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "cannot inspect terminal Ready claim {}: {error}",
                path.display()
            )
        })?;
        let claim = Self {
            object: object_identity(&metadata),
            file,
            path: path.to_path_buf(),
        };
        claim
            .file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                format!(
                    "cannot protect terminal Ready claim {}: {error}",
                    path.display()
                )
            })?;
        if !claim.exact_path_is_empty()? {
            return Err(format!(
                "terminal Ready claim {} changed while it was created",
                path.display()
            ));
        }
        Ok(claim)
    }

    fn retains_path(&self) -> Result<bool, String> {
        let current = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "cannot revalidate terminal Ready claim {}: {error}",
                    self.path.display()
                ));
            }
        };
        Ok(current.file_type().is_file()
            && current.uid() == effective_uid()
            && current.mode() & 0o7777 == 0o600
            && object_identity(&current) == self.object)
    }

    fn exact_path_is_empty(&self) -> Result<bool, String> {
        let metadata = self.file.metadata().map_err(|error| {
            format!(
                "cannot inspect terminal Ready claim {}: {error}",
                self.path.display()
            )
        })?;
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o7777 != 0o600
            || object_identity(&metadata) != self.object
        {
            return Err(format!(
                "terminal Ready claim {} changed while retained",
                self.path.display()
            ));
        }
        Ok(metadata.len() == 0 && self.retains_path()?)
    }

    fn lock_for_rollback(&self, deadline: Instant) -> Result<bool, String> {
        loop {
            match self.file.try_lock() {
                Ok(()) => return self.exact_path_is_empty(),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "terminal Ready claim {} remained busy",
                        self.path.display()
                    ));
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "cannot lock terminal Ready claim {}: {error}",
                        self.path.display()
                    ));
                }
            }
        }
    }

    fn remove_empty(&self) -> Result<(), String> {
        if !self.exact_path_is_empty()? {
            return Err(format!(
                "terminal Ready claim {} changed before cleanup",
                self.path.display()
            ));
        }
        fs::remove_file(&self.path).map_err(|error| {
            format!(
                "cannot remove terminal Ready claim {}: {error}",
                self.path.display()
            )
        })
    }
}

impl Drop for ReadyClaim {
    fn drop(&mut self) {
        if self
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.len() == 0)
            && fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| object_identity(&metadata) == self.object)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct ManagedCandidate {
    number: Option<usize>,
    endpoint: PathBuf,
    record_path: PathBuf,
    record: ObjectIdentity,
    identity: LiveIdentity,
}

fn artifact_path(endpoint: &Path, suffix: &str) -> PathBuf {
    let mut path = endpoint.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn object_identity(metadata: &fs::Metadata) -> ObjectIdentity {
    ObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn read_management_record(path: &Path) -> Result<Option<RecordSnapshot>, String> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot open terminal record {}: {error}",
                path.display()
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect terminal record {}: {error}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > management::MAX_RECORD_BYTES as u64
    {
        return Err(format!(
            "terminal record {} must be an owned mode-0600 regular file no larger than {} bytes",
            path.display(),
            management::MAX_RECORD_BYTES
        ));
    }
    let object = object_identity(&metadata);
    let mut bytes = Vec::with_capacity(management::MAX_RECORD_BYTES.saturating_add(1));
    Read::by_ref(&mut file)
        .take(management::MAX_RECORD_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read terminal record {}: {error}", path.display()))?;
    if bytes.len() > management::MAX_RECORD_BYTES {
        return Err(format!(
            "terminal record {} exceeds {} bytes",
            path.display(),
            management::MAX_RECORD_BYTES
        ));
    }
    let current = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot revalidate terminal record {}: {error}",
            path.display()
        )
    })?;
    if !current.file_type().is_file()
        || current.uid() != effective_uid()
        || current.mode() & 0o7777 != 0o600
        || object_identity(&current) != object
    {
        return Err(format!(
            "terminal record {} changed while it was being read",
            path.display()
        ));
    }
    let record = management::decode_record(&bytes)
        .map_err(|error| format!("invalid terminal record {}: {error}", path.display()))?;
    Ok(Some(RecordSnapshot { object, record }))
}

fn endpoint_matches(path: &Path, expected: &EndpointIdentity) -> Result<(), String> {
    if expected.path != path.as_os_str().as_bytes() {
        return Err(format!(
            "terminal identity names endpoint {}, expected {}",
            Path::new(OsStr::from_bytes(&expected.path)).display(),
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect terminal endpoint {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != 0o600
        || object_identity(&metadata) != expected.object
    {
        return Err(format!(
            "terminal endpoint {} does not match its owned mode-0600 socket identity",
            path.display()
        ));
    }
    Ok(())
}

fn validate_management_identity(
    identity: &LiveIdentity,
    component_generation: &str,
    expected_run: Option<&str>,
    require_live_endpoints: bool,
) -> Result<(Option<usize>, PathBuf), String> {
    let number = managed_session_number(&identity.session_id)?;
    if expected_run.is_some_and(|expected| identity.run_id != expected) {
        return Err("terminal record reports a different run identity".into());
    }
    if identity.component_generation != component_generation {
        return Err(format!(
            "terminal record reports component generation {}, expected {component_generation}",
            identity.component_generation
        ));
    }
    if identity.record_generation != management::RECORD_GENERATION
        || identity.management_generation != management::VERSION
    {
        return Err("terminal record reports an unsupported management generation".into());
    }
    if identity.uid != effective_uid() {
        return Err(format!(
            "terminal record reports UID {}, expected {}",
            identity.uid,
            effective_uid()
        ));
    }
    let presentation = PathBuf::from(OsStr::from_bytes(&identity.presentation.path));
    let runtime = presentation
        .parent()
        .ok_or_else(|| "terminal presentation endpoint has no parent".to_string())?;
    let expected_presentation = match number {
        None => presentation
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(is_directory_picker_endpoint),
        Some(1) => presentation == runtime.join("orbit.sock"),
        Some(number) => presentation == runtime.join(format!("session-{number}.sock")),
    };
    if !expected_presentation {
        return Err(format!(
            "terminal identity {} uses unexpected presentation endpoint {}",
            identity.session_id,
            presentation.display()
        ));
    }
    let management_path = artifact_path(&presentation, ".management");
    if identity.management.path != management_path.as_os_str().as_bytes() {
        return Err(format!(
            "terminal identity {} uses an unexpected management endpoint",
            identity.session_id
        ));
    }
    if require_live_endpoints {
        endpoint_matches(&presentation, &identity.presentation)?;
        endpoint_matches(&management_path, &identity.management)?;
    }
    Ok((number, presentation))
}

fn operation_timeout(deadline: Instant, operation: &str) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| format!("{operation} exceeded five seconds"))
}

fn unix_connect_with_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let address = socket2::SockAddr::unix(path)?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&address, timeout)?;
    Ok(UnixStream::from(OwnedFd::from(socket)))
}

fn read_management_response(
    stream: &mut UnixStream,
    deadline: Instant,
    operation: &str,
) -> Result<ManagementServerMessage, String> {
    let mut read_exact = |mut bytes: &mut [u8], context: &str| -> Result<(), String> {
        while !bytes.is_empty() {
            let timeout = operation_timeout(deadline, operation)?;
            stream
                .set_read_timeout(Some(timeout))
                .map_err(|error| format!("cannot bound {operation}: {error}"))?;
            match stream.read(bytes) {
                Ok(0) => return Err(format!("{context}: unexpected end of file")),
                Ok(read) => bytes = &mut bytes[read..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("{context}: {error}")),
            }
        }
        Ok(())
    };
    let mut bytes = vec![0; management::HEADER_BYTES];
    read_exact(&mut bytes, "cannot read terminal management result")?;
    let length = management::server_message_len(&bytes)
        .map_err(|error| format!("invalid terminal management result: {error}"))?
        .ok_or("incomplete terminal management header")?;
    bytes.resize(length, 0);
    read_exact(
        &mut bytes[management::HEADER_BYTES..],
        "cannot read complete terminal management result",
    )?;
    management::decode_server_message(&bytes)
        .map_err(|error| format!("invalid terminal management result: {error}"))
}

fn validate_management_peer(stream: &UnixStream, identity: &LiveIdentity) -> Result<(), String> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials points to writable ucred storage of the declared length.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
        )
    } == -1
        || length as usize != std::mem::size_of::<libc::ucred>()
    {
        return Err("cannot validate terminal management peer credentials".into());
    }
    if u32::try_from(credentials.pid).ok() != Some(identity.process_id)
        || credentials.uid != identity.uid
    {
        return Err("terminal management peer differs from its Ready process identity".into());
    }
    let stat = fs::read_to_string(format!("/proc/{}/stat", identity.process_id))
        .map_err(|error| format!("cannot validate terminal process identity: {error}"))?;
    let start = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("invalid terminal process identity")?;
    if start != identity.process_start {
        return Err("terminal process start differs from its Ready identity".into());
    }
    Ok(())
}

fn acquire_management(
    candidate: ManagedCandidate,
    deadline: Instant,
) -> Result<RunningSession, String> {
    let current = read_management_record(&candidate.record_path)?.ok_or_else(|| {
        format!(
            "terminal record {} disappeared",
            candidate.record_path.display()
        )
    })?;
    if current.object != candidate.record
        || current.record != ManagementRecord::Live(candidate.identity.clone())
    {
        return Err(format!(
            "terminal record {} changed before lease acquisition",
            candidate.record_path.display()
        ));
    }
    let management_path = Path::new(OsStr::from_bytes(&candidate.identity.management.path));
    endpoint_matches(management_path, &candidate.identity.management)?;
    let timeout = operation_timeout(deadline, "terminal lease acquisition")?;
    let mut stream = unix_connect_with_timeout(management_path, timeout).map_err(|error| {
        format!(
            "cannot connect to terminal management endpoint {}: {error}",
            management_path.display()
        )
    })?;
    validate_management_peer(&stream, &candidate.identity)?;
    let timeout = operation_timeout(deadline, "terminal lease acquisition")?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("cannot bound terminal lease acquisition: {error}"))?;
    let request = management::encode_client_message(&ManagementClientMessage::Acquire {
        expected: candidate.identity.clone(),
        record: candidate.record,
    })
    .map_err(|error| format!("cannot encode terminal lease request: {error}"))?;
    stream
        .write_all(&request)
        .map_err(|error| format!("cannot send terminal lease request: {error}"))?;
    match read_management_response(&mut stream, deadline, "terminal lease acquisition")? {
        ManagementServerMessage::Lease(identity) if identity == candidate.identity => {}
        ManagementServerMessage::Busy => {
            return Err(format!(
                "terminal run {} is already managed",
                candidate.identity.run_id
            ));
        }
        ManagementServerMessage::Failure(failure) => {
            return Err(format!(
                "terminal rejected lease acquisition: {}",
                failure.detail
            ));
        }
        _ => return Err("terminal returned the wrong lease result".into()),
    }
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .and_then(|()| stream.set_nonblocking(true))
        .map_err(|error| format!("cannot configure terminal management lease: {error}"))?;
    Ok(RunningSession {
        number: candidate.number,
        id: candidate.identity.session_id.clone(),
        endpoint: candidate.endpoint,
        record: candidate.record_path,
        record_object: candidate.record,
        identity: candidate.identity,
        lease: stream,
        child: None,
        finished: None,
    })
}

fn cleanup_record(path: &Path, expected: ObjectIdentity) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect ended terminal record {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != 0o600
        || object_identity(&metadata) != expected
    {
        return Err(format!(
            "ended terminal record {} changed before cleanup",
            path.display()
        ));
    }
    fs::remove_file(path).map_err(|error| {
        format!(
            "cannot remove ended terminal record {}: {error}",
            path.display()
        )
    })
}

fn recorded_process_is_dead(identity: &LiveIdentity) -> Result<bool, String> {
    let stat = match fs::read_to_string(format!("/proc/{}/stat", identity.process_id)) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(format!(
                "cannot validate terminal process identity: {error}"
            ));
        }
    };
    let mut fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields.split_whitespace())
        .ok_or("invalid terminal process identity")?;
    let state = fields.next().ok_or("invalid terminal process identity")?;
    let start = fields
        .nth(18)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("invalid terminal process identity")?;
    if start != identity.process_start {
        return Err("terminal process start differs from its Ready identity".into());
    }
    Ok(matches!(state, "Z" | "X"))
}

fn remove_dead_endpoint(path: &Path, expected: &EndpointIdentity) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect dead terminal endpoint {}: {error}",
                path.display()
            ));
        }
    };
    if !metadata.file_type().is_socket()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != 0o600
        || object_identity(&metadata) != expected.object
    {
        return Err(format!(
            "dead terminal endpoint {} changed before cleanup",
            path.display()
        ));
    }
    fs::remove_file(path).map_err(|error| {
        format!(
            "cannot remove dead terminal endpoint {}: {error}",
            path.display()
        )
    })
}

fn retire_dead_run(
    record: &Path,
    object: ObjectIdentity,
    identity: &LiveIdentity,
    deadline: Instant,
) -> Result<(), String> {
    remove_dead_endpoint(
        Path::new(OsStr::from_bytes(&identity.presentation.path)),
        &identity.presentation,
    )?;
    remove_dead_endpoint(
        Path::new(OsStr::from_bytes(&identity.management.path)),
        &identity.management,
    )?;
    reap_untracked_child(identity.process_id, deadline)?;
    cleanup_record(record, object)
}

pub(super) fn recover_sessions(
    runtime: &Path,
    mode: LaunchMode,
    component_generation: &str,
    deadline: Instant,
) -> Result<(Vec<RunningSession>, bool), String> {
    let mut sessions =
        acquire_generation_sessions(runtime, mode, component_generation, &[], deadline)?;
    let mut recovered_picker = false;
    for picker in sessions
        .iter_mut()
        .filter(|session| session.number.is_none())
    {
        recovered_picker = true;
        stop_managed_sessions(
            std::slice::from_mut(picker),
            operation_timeout(deadline, "stale directory-picker cleanup")?,
        )?;
    }
    sessions.retain(|session| session.number.is_some());
    Ok((sessions, recovered_picker))
}

pub(super) fn acquire_generation_sessions(
    runtime: &Path,
    mode: LaunchMode,
    component_generation: &str,
    owned: &[RunningSession],
    deadline: Instant,
) -> Result<Vec<RunningSession>, String> {
    let mut paths = fs::read_dir(runtime)
        .map_err(|error| {
            format!(
                "cannot list terminal runtime {}: {error}",
                runtime.display()
            )
        })?
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_name().as_bytes().ends_with(b".record") => {
                Some(Ok(entry.path()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(format!(
                "cannot inspect terminal entry in {}: {error}",
                runtime.display()
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if paths.len() > MAX_SESSIONS + 1 {
        return Err(format!(
            "terminal runtime {} exceeds the {}-record recovery limit",
            runtime.display(),
            MAX_SESSIONS + 1
        ));
    }
    paths.sort_by(|left, right| {
        left.as_os_str()
            .as_bytes()
            .cmp(right.as_os_str().as_bytes())
    });

    let mut candidates = Vec::new();
    let mut pickers = Vec::new();
    let mut numbers = HashSet::new();
    for record_path in paths {
        if owned.iter().any(|session| session.record == record_path) {
            continue;
        }
        operation_timeout(deadline, "terminal recovery")?;
        let snapshot = read_management_record(&record_path)?
            .ok_or_else(|| format!("terminal record {} disappeared", record_path.display()))?;
        let identity = match snapshot.record {
            ManagementRecord::Live(identity) => identity,
            ManagementRecord::Tombstone(tombstone) => {
                let (_, endpoint) = validate_management_identity(
                    &tombstone.identity,
                    component_generation,
                    None,
                    false,
                )?;
                if artifact_path(&endpoint, ".record") != record_path {
                    return Err(format!(
                        "ended terminal record {} has the wrong identity path",
                        record_path.display()
                    ));
                }
                let management_path =
                    Path::new(OsStr::from_bytes(&tombstone.identity.management.path));
                loop {
                    if endpoint_removed(&endpoint, tombstone.identity.presentation.object)?
                        && endpoint_removed(management_path, tombstone.identity.management.object)?
                    {
                        break;
                    }
                    operation_timeout(deadline, "ended terminal cleanup")?;
                    thread::sleep(Duration::from_millis(25));
                }
                recorded_process_is_dead(&tombstone.identity)?;
                reap_untracked_child(tombstone.identity.process_id, deadline)?;
                cleanup_record(&record_path, snapshot.object)?;
                continue;
            }
        };
        let (number, endpoint) =
            validate_management_identity(&identity, component_generation, None, false)?;
        if artifact_path(&endpoint, ".record") != record_path {
            return Err(format!(
                "terminal record {} has the wrong presentation identity",
                record_path.display()
            ));
        }
        if recorded_process_is_dead(&identity)? {
            retire_dead_run(&record_path, snapshot.object, &identity, deadline)?;
            continue;
        }
        validate_management_identity(&identity, component_generation, None, true)?;
        let candidate = ManagedCandidate {
            number,
            endpoint,
            record_path,
            record: snapshot.object,
            identity,
        };
        if let Some(number) = number {
            if !numbers.insert(number) {
                return Err(format!("duplicate live terminal identity session-{number}"));
            }
            candidates.push(candidate);
        } else {
            pickers.push(candidate);
        }
    }
    candidates.sort_by_key(|candidate| candidate.number);
    if mode == LaunchMode::Terminal && candidates.len() > 1 {
        return Err("EonTerm cannot recover more than one live terminal".into());
    }

    candidates
        .into_iter()
        .chain(pickers)
        .map(|candidate| acquire_management(candidate, deadline))
        .collect()
}

pub(super) struct RunningSession {
    pub(super) number: Option<usize>,
    pub(super) id: String,
    pub(super) endpoint: PathBuf,
    record: PathBuf,
    record_object: ObjectIdentity,
    identity: LiveIdentity,
    lease: UnixStream,
    child: Option<Child>,
    finished: Option<SessionEnd>,
}

#[derive(Clone, Copy)]
enum SessionEnd {
    Managed(TerminationReason, ProcessOutcome),
    OwnerLost,
}

impl RunningSession {
    pub(super) fn is_finished(&self) -> bool {
        self.finished.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_orbit(
    inputs: &Inputs,
    config: &Path,
    socket: &Path,
    session_id: &str,
    component_generation: &str,
    directory: &Path,
    child: &[OsString],
    deadline: Instant,
) -> Result<RunningSession, String> {
    let run_id = request_id();
    let record_path = artifact_path(socket, ".record");
    let mut orbit_command = orbit_command(
        inputs,
        config,
        socket,
        session_id,
        &run_id,
        component_generation,
        directory,
        child,
    )?;
    let claim = ReadyClaim::create(&record_path)?;
    let mut orbit = match orbit_command.spawn() {
        Ok(orbit) => orbit,
        Err(error) => {
            return match claim.remove_empty() {
                Ok(()) => Err(format!("cannot launch terminal: {error}")),
                Err(cleanup_error) => Err(format!(
                    "cannot launch terminal: {error}; cannot roll back terminal: {cleanup_error}"
                )),
            };
        }
    };
    let result = (|| {
        loop {
            let record = if claim.retains_path()? {
                None
            } else {
                read_management_record(&record_path)?
            };
            match record {
                Some(RecordSnapshot {
                    object,
                    record: ManagementRecord::Live(identity),
                }) => {
                    if let Some(status) = orbit
                        .try_wait()
                        .map_err(|error| format!("cannot observe terminal startup: {error}"))?
                    {
                        return Err(format!(
                            "terminal exited before its management lease was acquired (status {})",
                            status_code(status)
                        ));
                    }
                    let (number, endpoint) = validate_management_identity(
                        &identity,
                        component_generation,
                        Some(&run_id),
                        true,
                    )?;
                    if identity.session_id != session_id || endpoint != socket {
                        return Err("terminal Ready identity does not match its launch".into());
                    }
                    return acquire_management(
                        ManagedCandidate {
                            number,
                            endpoint,
                            record_path,
                            record: object,
                            identity,
                        },
                        deadline,
                    );
                }
                Some(RecordSnapshot {
                    record: ManagementRecord::Tombstone(_),
                    ..
                }) => {
                    return Err("terminal ended before its management lease was acquired".into());
                }
                None => {
                    if let Some(status) = orbit
                        .try_wait()
                        .map_err(|error| format!("cannot observe terminal startup: {error}"))?
                    {
                        return if claim.exact_path_is_empty()? {
                            Err(format!(
                                "terminal exited before publishing Ready (status {})",
                                status_code(status)
                            ))
                        } else {
                            Err(format!(
                                "terminal exited before its management lease was acquired (status {})",
                                status_code(status)
                            ))
                        };
                    }
                    if Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(25));
                    } else {
                        return Err("terminal did not publish Ready within five seconds".into());
                    }
                }
            }
        }
    })();
    match result {
        Ok(mut session) => {
            session.child = Some(orbit);
            Ok(session)
        }
        Err(error) => match rollback_unleased_orbit(orbit, claim, deadline) {
            Ok(()) => Err(error),
            Err(stop_error) => Err(format!("{error}; cannot roll back terminal: {stop_error}")),
        },
    }
}

fn rollback_unleased_orbit(
    mut orbit: Child,
    claim: ReadyClaim,
    deadline: Instant,
) -> Result<(), String> {
    let claim_error = match claim.lock_for_rollback(deadline) {
        Ok(true) => {
            while Instant::now() < deadline {
                match orbit.try_wait() {
                    Ok(Some(_)) => return claim.remove_empty(),
                    Ok(None) => thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
            stop(&mut orbit);
            return claim.remove_empty();
        }
        Ok(false) => None,
        Err(error) => Some(error),
    };
    let Ok(Some(RecordSnapshot {
        object,
        record: ManagementRecord::Live(identity),
    })) = read_management_record(&claim.path)
    else {
        return claim_error.map_or(Ok(()), Err);
    };
    if identity.process_id != orbit.id() {
        return claim_error.map_or(Ok(()), Err);
    }
    let mut session = acquire_management(
        ManagedCandidate {
            number: managed_session_number(&identity.session_id)?,
            endpoint: PathBuf::from(OsStr::from_bytes(&identity.presentation.path)),
            record_path: claim.path.clone(),
            record: object,
            identity,
        },
        deadline,
    )?;
    session.child = Some(orbit);
    let timeout = operation_timeout(deadline, "terminal rollback")?;
    stop_managed_sessions(std::slice::from_mut(&mut session), timeout).map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn orbit_command(
    inputs: &Inputs,
    config: &Path,
    socket: &Path,
    session_id: &str,
    run_id: &str,
    component_generation: &str,
    directory: &Path,
    child: &[OsString],
) -> Result<Command, String> {
    let mut orbit_command = Command::new(&inputs.orbit);
    orbit_command
        .arg("serve")
        .arg(socket)
        .arg("--management-v1")
        .arg(session_id)
        .arg(run_id)
        .arg(component_generation)
        .arg("--ansi-palette-v1")
        .arg(inputs.defaults.ansi_palette)
        .arg("--")
        .env("EON_CONFIG_HOME", config)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(session_bin) = &inputs.managed.shell_bin {
        orbit_command.env("PATH", managed_environment::session_path(session_bin)?);
    }
    if child.is_empty() {
        orbit_command.args(managed_environment::shell_command(
            config,
            &inputs.defaults,
        )?);
    } else {
        orbit_command.args(child);
    }
    Ok(orbit_command)
}

fn endpoint_removed(path: &Path, expected: ObjectIdentity) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(metadata) if object_identity(&metadata) == expected => Ok(false),
        Ok(_) => Err(format!(
            "terminal endpoint {} was replaced during cleanup",
            path.display()
        )),
        Err(error) => Err(format!(
            "cannot observe terminal endpoint cleanup {}: {error}",
            path.display()
        )),
    }
}

fn reap_untracked_child(process_id: u32, deadline: Instant) -> Result<(), String> {
    let pid = i32::try_from(process_id)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or("invalid ended terminal process identity")?;
    loop {
        // A failed Ready rollback can drop our Child handle. Reap only an
        // exact child after managed cleanup; recovered runs return ECHILD.
        let mut status = 0;
        // SAFETY: status is writable; WNOHANG sends no signal and cannot wait on a non-child.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("cannot reap ended terminal: {error}"));
        }
        operation_timeout(deadline, "terminal process reaping")?;
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_and_finalize_tombstone(
    session: &mut RunningSession,
    reason: TerminationReason,
    response: Option<&Tombstone>,
    deadline: Instant,
) -> Result<SessionEnd, String> {
    let (outcome, record_object) = loop {
        let snapshot = read_management_record(&session.record)?.ok_or_else(|| {
            format!(
                "ended terminal record {} disappeared",
                session.record.display()
            )
        })?;
        match snapshot.record {
            ManagementRecord::Live(identity) if identity == session.identity => {
                if recorded_process_is_dead(&identity)? {
                    // Death can race Orbit's final atomic record publication.
                    let current = read_management_record(&session.record)?
                        .ok_or("ended terminal record disappeared")?;
                    if current.record != ManagementRecord::Live(identity.clone()) {
                        operation_timeout(deadline, "terminal tombstone reconciliation")?;
                        continue;
                    }
                    if current.object != session.record_object {
                        return Err("terminal Live record changed before cleanup".into());
                    }
                    retire_dead_run(&session.record, current.object, &identity, deadline)?;
                    session.finished = Some(SessionEnd::OwnerLost);
                    eprintln!(
                        "terminal {}: Orbit exited without publishing a terminal result",
                        session.id
                    );
                    return if reason == TerminationReason::NaturalExit {
                        Ok(SessionEnd::OwnerLost)
                    } else {
                        Err("terminal owner exited without completing Stop".into())
                    };
                }
                operation_timeout(deadline, "terminal tombstone reconciliation")?;
                thread::sleep(Duration::from_millis(25));
            }
            ManagementRecord::Tombstone(tombstone)
                if tombstone.identity == session.identity && tombstone.reason == reason =>
            {
                if response.is_some_and(|response| response != &tombstone) {
                    return Err("terminal stop result differs from its final record".into());
                }
                break (tombstone.outcome, snapshot.object);
            }
            _ => return Err("terminal final record does not match the acquired run".into()),
        }
    };
    let management_path = Path::new(OsStr::from_bytes(&session.identity.management.path));
    loop {
        if endpoint_removed(&session.endpoint, session.identity.presentation.object)?
            && endpoint_removed(management_path, session.identity.management.object)?
        {
            break;
        }
        operation_timeout(deadline, "terminal endpoint cleanup")?;
        thread::sleep(Duration::from_millis(25));
    }
    if let Some(child) = &mut session.child {
        loop {
            if child
                .try_wait()
                .map_err(|error| format!("cannot reap ended terminal: {error}"))?
                .is_some()
            {
                break;
            }
            operation_timeout(deadline, "terminal process reaping")?;
            thread::sleep(Duration::from_millis(25));
        }
    } else {
        reap_untracked_child(session.identity.process_id, deadline)?;
    }
    cleanup_record(&session.record, record_object)?;
    let end = SessionEnd::Managed(reason, outcome);
    session.finished = Some(end);
    Ok(end)
}

pub(super) fn session_finished(session: &mut RunningSession) -> Result<Option<i32>, String> {
    let end = if let Some(end) = session.finished {
        end
    } else {
        match session.lease.read(&mut [0]) {
            Ok(0) => {}
            Ok(_) => return Err("terminal sent an unsolicited management result".into()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
            Err(_) => {}
        }
        wait_and_finalize_tombstone(
            session,
            TerminationReason::NaturalExit,
            None,
            Instant::now() + SESSION_START_TIMEOUT,
        )?
    };
    Ok(Some(match end {
        SessionEnd::Managed(_, ProcessOutcome::ExitCode(code)) => code,
        SessionEnd::Managed(_, ProcessOutcome::Signal(_)) | SessionEnd::OwnerLost => 1,
    }))
}

pub(super) fn stop_managed_sessions(
    sessions: &mut [RunningSession],
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let deadline = Instant::now() + timeout;
    let request = management::encode_client_message(&ManagementClientMessage::Stop)
        .map_err(|error| format!("cannot encode terminal stop: {error}"))?;
    let mut writes = Vec::with_capacity(sessions.len());
    for session in sessions.iter_mut().filter(|session| !session.is_finished()) {
        let result = (|| {
            let remaining = operation_timeout(deadline, "terminal stop")?;
            session
                .lease
                .set_nonblocking(false)
                .and_then(|()| session.lease.set_write_timeout(Some(remaining)))
                .map_err(|error| format!("cannot bound terminal stop: {error}"))?;
            session
                .lease
                .write_all(&request)
                .map_err(|error| format!("cannot send terminal stop: {error}"))
        })();
        writes.push(result);
    }

    let mut errors = Vec::new();
    for (session, write) in sessions
        .iter_mut()
        .filter(|session| !session.is_finished())
        .zip(writes)
    {
        let response = if write.is_ok() {
            match read_management_response(&mut session.lease, deadline, "terminal stop response") {
                Ok(ManagementServerMessage::Stopped(tombstone))
                    if tombstone.identity == session.identity
                        && tombstone.reason == TerminationReason::ExplicitStop =>
                {
                    Some(Ok(tombstone))
                }
                Ok(ManagementServerMessage::Failure(failure)) => Some(Err(format!(
                    "terminal rejected stop for {}: {}",
                    session.id, failure.detail
                ))),
                Ok(_) => Some(Err(format!(
                    "terminal returned the wrong stop result for {}",
                    session.id
                ))),
                Err(_) => None,
            }
        } else {
            None
        };
        let result = match response {
            Some(Ok(tombstone)) => wait_and_finalize_tombstone(
                session,
                TerminationReason::ExplicitStop,
                Some(&tombstone),
                deadline,
            ),
            Some(Err(error)) => Err(error),
            None => wait_and_finalize_tombstone(
                session,
                TerminationReason::ExplicitStop,
                None,
                deadline,
            ),
        };
        if let Err(error) = result {
            if matches!(read_management_record(&session.record), Ok(Some(RecordSnapshot {
                record: ManagementRecord::Tombstone(ref tombstone), ..
            })) if tombstone.identity == session.identity && tombstone.reason == TerminationReason::NaturalExit)
                && let Err(cleanup) = wait_and_finalize_tombstone(
                    session,
                    TerminationReason::NaturalExit,
                    None,
                    deadline,
                )
            {
                errors.push(format!(
                    "{}: cannot reconcile natural exit: {cleanup}",
                    session.id
                ));
            }
            errors.push(format!("{}: {error}", session.id));
        }
    }
    if errors.is_empty() {
        Ok(sessions
            .iter()
            .filter(|session| {
                matches!(
                    session.finished,
                    Some(SessionEnd::Managed(TerminationReason::ExplicitStop, _))
                )
            })
            .map(|session| session.id.clone())
            .collect())
    } else {
        for session in sessions {
            if let Err(error) = session
                .lease
                .set_read_timeout(None)
                .and_then(|()| session.lease.set_write_timeout(None))
                .and_then(|()| session.lease.set_nonblocking(true))
            {
                errors.push(format!(
                    "{}: cannot restore terminal management lease after failed stop: {error}",
                    session.id
                ));
            }
        }
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EndpointIdentity, LiveIdentity, ManagementRecord, ObjectIdentity, ProcessOutcome,
        RunningSession, TerminationReason, Tombstone, is_directory_picker_endpoint,
        managed_session_number, management, orbit_command, read_management_response,
        session_number, stop_managed_sessions, unix_connect_with_timeout,
    };
    use crate::supervisor::temporary_directory;
    use orbit_protocol::management::ServerMessage as ManagementServerMessage;
    use std::{
        ffi::{OsStr, OsString},
        fs,
        io::Write,
        os::{
            fd::AsRawFd,
            unix::{
                ffi::OsStrExt,
                fs::OpenOptionsExt,
                net::{UnixListener, UnixStream},
            },
        },
        path::{Path, PathBuf},
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn lost_owner_cleanup_requires_death_and_original_owned_objects() {
        let root = temporary_directory();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let process = PathBuf::from(format!("/proc/{}", child.id()));
        let start = fs::read_to_string(process.join("stat"))
            .unwrap()
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let endpoint = root.join("orbit.sock");
        let management_path = root.join("orbit.sock.management");
        let _presentation = UnixListener::bind(&endpoint).unwrap();
        let _management = UnixListener::bind(&management_path).unwrap();
        for path in [&endpoint, &management_path] {
            fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        }
        let endpoint_identity = |path: &Path| EndpointIdentity {
            path: path.as_os_str().as_bytes().to_vec(),
            object: super::object_identity(&fs::symlink_metadata(path).unwrap()),
        };
        let identity = LiveIdentity {
            session_id: "session-1".into(),
            run_id: "lost-owner".into(),
            component_generation: "component-1".into(),
            record_generation: management::RECORD_GENERATION,
            management_generation: management::VERSION,
            process_id: child.id(),
            process_start: start,
            uid: super::effective_uid(),
            presentation: endpoint_identity(&endpoint),
            management: endpoint_identity(&management_path),
        };
        let record = root.join("orbit.sock.record");
        let bytes = management::encode_record(&ManagementRecord::Live(identity.clone())).unwrap();
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&record)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let (lease, peer) = UnixStream::pair().unwrap();
        lease.set_nonblocking(true).unwrap();
        drop(peer);
        let mut session = RunningSession {
            number: Some(1),
            id: identity.session_id.clone(),
            endpoint: endpoint.clone(),
            record: record.clone(),
            record_object: super::object_identity(&fs::symlink_metadata(&record).unwrap()),
            identity,
            lease,
            child: None,
            finished: None,
        };
        assert!(
            super::wait_and_finalize_tombstone(
                &mut session,
                TerminationReason::NaturalExit,
                None,
                Instant::now() + Duration::from_millis(10),
            )
            .is_err()
        );
        assert!(record.exists() && endpoint.exists() && management_path.exists());
        assert!(!session.is_finished());
        child.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !super::recorded_process_is_dead(&session.identity).unwrap() {
            assert!(Instant::now() < deadline, "test owner did not exit");
            thread::sleep(Duration::from_millis(1));
        }

        let held_record = root.join("held-record");
        fs::rename(&record, &held_record).unwrap();
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&record)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        assert!(super::session_finished(&mut session).is_err());
        assert_eq!(fs::read(&record).unwrap(), bytes);
        assert!(endpoint.exists() && management_path.exists());
        fs::remove_file(&record).unwrap();
        fs::rename(held_record, &record).unwrap();

        let held_endpoint = root.join("held-endpoint");
        fs::rename(&endpoint, &held_endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();
        fs::set_permissions(
            &endpoint,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        assert!(super::session_finished(&mut session).is_err());
        assert!(record.exists() && endpoint.exists());
        fs::remove_file(&endpoint).unwrap();
        drop(replacement);
        fs::rename(held_endpoint, &endpoint).unwrap();

        assert!(
            super::wait_and_finalize_tombstone(
                &mut session,
                TerminationReason::ExplicitStop,
                None,
                Instant::now() + Duration::from_secs(1),
            )
            .is_err()
        );
        assert_eq!(super::session_finished(&mut session).unwrap(), Some(1));
        assert!(session.is_finished());
        assert!(!process.exists(), "lost direct child was not reaped");
        assert!(!record.exists() && !endpoint.exists() && !management_path.exists());
        assert!(
            stop_managed_sessions(&mut [session], Duration::from_secs(1))
                .unwrap()
                .is_empty(),
            "owner loss was reported as successful Stop"
        );
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_reaps_untracked_ended_children_before_removing_records() {
        let root = temporary_directory();
        for reason in [
            Some(TerminationReason::NaturalExit),
            Some(TerminationReason::ExplicitStop),
            None,
        ] {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            let process_path = PathBuf::from(format!("/proc/{pid}"));
            let start = fs::read_to_string(process_path.join("stat"))
                .unwrap()
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .parse()
                .unwrap();
            let endpoint = root.join("k1.sock");
            let record = root.join("k1.sock.record");
            let identity = LiveIdentity {
                session_id: "directory-picker-1".into(),
                run_id: format!("run-{pid}"),
                component_generation: "component-1".into(),
                record_generation: management::RECORD_GENERATION,
                management_generation: management::VERSION,
                process_id: pid,
                process_start: start,
                uid: super::effective_uid(),
                presentation: EndpointIdentity {
                    path: endpoint.as_os_str().as_bytes().to_vec(),
                    object: ObjectIdentity {
                        device: 1,
                        inode: 1,
                    },
                },
                management: EndpointIdentity {
                    path: root
                        .join("k1.sock.management")
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                    object: ObjectIdentity {
                        device: 1,
                        inode: 2,
                    },
                },
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            while !super::recorded_process_is_dead(&identity).unwrap() {
                assert!(Instant::now() < deadline, "test child did not exit");
                thread::sleep(Duration::from_millis(5));
            }
            let value = reason.map_or_else(
                || ManagementRecord::Live(identity.clone()),
                |reason| {
                    ManagementRecord::Tombstone(Tombstone {
                        identity: identity.clone(),
                        reason,
                        outcome: ProcessOutcome::ExitCode(0),
                    })
                },
            );
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&record)
                .unwrap()
                .write_all(&management::encode_record(&value).unwrap())
                .unwrap();
            if let ManagementRecord::Tombstone(mut wrong) = value.clone() {
                wrong.identity.process_start += 1;
                fs::write(
                    &record,
                    management::encode_record(&ManagementRecord::Tombstone(wrong)).unwrap(),
                )
                .unwrap();
                let error = super::acquire_generation_sessions(
                    &root,
                    super::LaunchMode::Workspace,
                    "component-1",
                    &[],
                    deadline,
                )
                .err()
                .expect("wrong process start was accepted");
                assert!(error.contains("process start differs"), "{error}");
                assert!(
                    process_path.exists(),
                    "wrong identity authorized child reaping"
                );
                assert!(record.exists(), "wrong identity retired its record");
                fs::write(&record, management::encode_record(&value).unwrap()).unwrap();
            }
            let sessions = super::acquire_generation_sessions(
                &root,
                super::LaunchMode::Workspace,
                "component-1",
                &[],
                deadline,
            )
            .unwrap();
            let reaped = !process_path.exists();
            let _ = child.wait();
            assert!(sessions.is_empty());
            assert!(!record.exists());
            assert!(
                reaped,
                "discovery removed {reason:?} record without reaping its child"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_natural_cleanup_still_finalizes_other_runs_and_restores_leases() {
        let root = temporary_directory();
        let mut peers = Vec::new();
        let mut sessions = Vec::new();
        for (index, reason) in [
            TerminationReason::NaturalExit,
            TerminationReason::ExplicitStop,
        ]
        .into_iter()
        .enumerate()
        {
            let number = index + 1;
            let endpoint = root.join(format!("session-{number}.sock"));
            let identity = LiveIdentity {
                session_id: format!("session-{number}"),
                run_id: format!("run-{number}"),
                component_generation: "component-1".into(),
                record_generation: management::RECORD_GENERATION,
                management_generation: management::VERSION,
                process_id: std::process::id(),
                process_start: 1,
                uid: super::effective_uid(),
                presentation: EndpointIdentity {
                    path: endpoint.as_os_str().as_bytes().to_vec(),
                    object: ObjectIdentity {
                        device: 1,
                        inode: 1,
                    },
                },
                management: EndpointIdentity {
                    path: root
                        .join(format!("management-{number}"))
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                    object: ObjectIdentity {
                        device: 1,
                        inode: 2,
                    },
                },
            };
            let tombstone = Tombstone {
                identity: identity.clone(),
                reason,
                outcome: ProcessOutcome::ExitCode(0),
            };
            let record = root.join(format!("session-{number}.record"));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&record)
                .unwrap()
                .write_all(
                    &management::encode_record(&ManagementRecord::Tombstone(tombstone.clone()))
                        .unwrap(),
                )
                .unwrap();
            let (lease, mut peer) = UnixStream::pair().unwrap();
            peer.write_all(
                &management::encode_server_message(&ManagementServerMessage::Stopped(tombstone))
                    .unwrap(),
            )
            .unwrap();
            peers.push(peer);
            sessions.push(RunningSession {
                number: Some(number),
                id: identity.session_id.clone(),
                endpoint,
                record_object: super::object_identity(&fs::symlink_metadata(&record).unwrap()),
                record,
                identity,
                lease,
                child: None,
                finished: None,
            });
        }
        fs::write(&sessions[0].endpoint, "replacement").unwrap();
        let error = stop_managed_sessions(&mut sessions, Duration::from_secs(1)).unwrap_err();
        assert!(error.contains("replaced during cleanup"), "{error}");
        assert!(
            sessions[1].is_finished(),
            "one failure skipped a completed run"
        );
        assert!(!sessions[1].record.exists());
        assert_eq!(
            fs::read_to_string(&sessions[0].endpoint).unwrap(),
            "replacement"
        );
        for session in &sessions {
            assert_eq!(session.lease.read_timeout().unwrap(), None);
            assert_eq!(session.lease.write_timeout().unwrap(), None);
            // SAFETY: F_GETFL only reads flags from this test-owned descriptor.
            let flags = unsafe { libc::fcntl(session.lease.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags & libc::O_NONBLOCK, 0);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_identity_is_positive_canonical_decimal() {
        assert_eq!(session_number("session-1"), Some(1));
        assert_eq!(session_number("session-256"), Some(256));
        assert_eq!(managed_session_number("directory-picker"), Ok(None));
        for invalid in [
            "session-0",
            "session-01",
            "session-",
            "session-a",
            "Session-1",
            "pane-1",
            "session-184467440737095516160",
        ] {
            assert_eq!(session_number(invalid), None, "accepted {invalid}");
        }
        for valid in ["pick.sock", "k1.sock", "k256.sock"] {
            assert!(is_directory_picker_endpoint(valid));
        }
        for invalid in [
            "k0.sock",
            "k01.sock",
            "k.sock",
            "k-1.sock",
            "k1.sock.record",
            "../k1.sock",
            "k184467440737095516160.sock",
        ] {
            assert!(!is_directory_picker_endpoint(invalid), "accepted {invalid}");
        }
    }

    #[test]
    fn unix_connection_attempt_obeys_timeout() {
        let root = temporary_directory();
        let path = root.join("backlog.sock");
        let socket =
            socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        socket
            .bind(&socket2::SockAddr::unix(&path).unwrap())
            .unwrap();
        socket.listen(0).unwrap();
        let listener = UnixListener::from(std::os::fd::OwnedFd::from(socket));
        let queued = UnixStream::connect(&path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let attempt = thread::spawn(move || {
            sender
                .send(unix_connect_with_timeout(&path, Duration::from_millis(50)))
                .unwrap();
        });

        let result = receiver.recv_timeout(Duration::from_secs(1));
        drop(queued);
        drop(listener);
        attempt.join().unwrap();

        assert!(
            result
                .expect("Unix connection exceeded its timeout")
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn management_response_obeys_one_deadline_across_partial_reads() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let response = management::encode_server_message(&ManagementServerMessage::Busy).unwrap();
        let sender = thread::spawn(move || {
            for byte in response {
                if writer.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let result = read_management_response(
            &mut reader,
            started + Duration::from_millis(80),
            "terminal management response",
        );
        let elapsed = started.elapsed();
        drop(reader);
        sender.join().unwrap();

        assert!(result.is_err(), "received {result:?}");
        assert!(elapsed >= Duration::from_millis(40));
        assert!(elapsed < Duration::from_secs(1));
    }

    fn command_environment<'a>(command: &'a Command, name: &str) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| value)
    }

    #[test]
    fn orbit_uses_configured_argv_only_for_default_sessions() {
        let root = temporary_directory();
        let mut inputs = crate::fixtures::inputs();
        inputs.orbit = "/managed/orbit".into();
        inputs.venus = "/managed/venus".into();
        inputs.managed.shell_bin = Some("/managed/bin".into());
        inputs.defaults.ansi_palette = "101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010,101010";
        let config = root.as_path();
        let socket = Path::new("/runtime/orbit.sock");
        fs::write(
            root.join("config.toml"),
            "[shell]\ncommand = [\"eon-fish\", \"--no-config\"]\n",
        )
        .unwrap();

        let default = orbit_command(
            &inputs,
            config,
            socket,
            "session-1",
            "run-1",
            "component-1",
            root.as_path(),
            &[],
        )
        .unwrap();
        assert_eq!(
            default.get_args().map(OsString::from).collect::<Vec<_>>(),
            [
                "serve",
                "/runtime/orbit.sock",
                "--management-v1",
                "session-1",
                "run-1",
                "component-1",
                "--ansi-palette-v1",
                inputs.defaults.ansi_palette,
                "--",
                "eon-fish",
                "--no-config",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            command_environment(&default, "EON_CONFIG_HOME"),
            Some(Some(config.as_os_str()))
        );
        assert_eq!(command_environment(&default, "XDG_CONFIG_HOME"), None);
        assert_eq!(default.get_current_dir(), Some(root.as_path()));
        assert_eq!(
            std::env::split_paths(
                default
                    .get_envs()
                    .find(|(name, _)| *name == "PATH")
                    .unwrap()
                    .1
                    .unwrap(),
            )
            .next(),
            Some(PathBuf::from("/managed/bin"))
        );

        let explicit = orbit_command(
            &inputs,
            config,
            socket,
            "session-2",
            "run-2",
            "component-2",
            root.as_path(),
            &["codex".into(), "--model".into(), "test".into()],
        )
        .unwrap();
        assert_eq!(
            explicit.get_args().map(OsString::from).collect::<Vec<_>>(),
            [
                "serve",
                "/runtime/orbit.sock",
                "--management-v1",
                "session-2",
                "run-2",
                "component-2",
                "--ansi-palette-v1",
                inputs.defaults.ansi_palette,
                "--",
                "codex",
                "--model",
                "test",
            ]
            .map(OsString::from)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
