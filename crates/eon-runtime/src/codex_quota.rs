use eon_workspace_protocol::v7::{CodexQuota, CodexQuotaState, CodexQuotaWindow};
use serde_json::Value;
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_MESSAGES_PER_TICK: usize = 16;

#[derive(Clone, Copy)]
struct Timings {
    refresh: Duration,
    timeout: Duration,
    retry_max: Duration,
    tick: Duration,
    shutdown: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            refresh: Duration::from_secs(60),
            timeout: Duration::from_secs(10),
            retry_max: Duration::from_secs(15 * 60),
            tick: Duration::from_millis(25),
            shutdown: Duration::from_secs(1),
        }
    }
}

pub(crate) struct Provider {
    quota: Arc<Mutex<Option<CodexQuota>>>,
    worker: Option<(mpsc::Sender<()>, thread::JoinHandle<()>)>,
}

impl Provider {
    pub(crate) fn start() -> Self {
        Self::start_with("codex".into(), Timings::default())
    }

    fn start_with(program: OsString, timings: Timings) -> Self {
        let quota = Arc::new(Mutex::new(None));
        let worker_quota = Arc::clone(&quota);
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("eon-codex-quota".into())
            .spawn(move || run(program, timings, worker_quota, stopped))
            .ok()
            .map(|worker| (stop, worker));
        Self { quota, worker }
    }

    pub(crate) fn snapshot(&self) -> Option<CodexQuota> {
        let now = epoch_seconds()?;
        let mut quota = self.quota.lock().ok()?;
        expire_stale(&mut quota, now);
        quota.clone()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        if let Some((stop, worker)) = self.worker.take() {
            let _ = stop.send(());
            drop(stop);
            let _ = worker.join();
        }
    }
}

fn run(
    program: OsString,
    timings: Timings,
    quota: Arc<Mutex<Option<CodexQuota>>>,
    stop: mpsc::Receiver<()>,
) {
    let mut retry = timings.refresh;
    let mut account = None;
    loop {
        match run_session(&program, timings, &quota, &stop, &mut account) {
            SessionEnd::Stopped => return,
            SessionEnd::Failed { published } => {
                if published {
                    retry = timings.refresh;
                }
                if stop.recv_timeout(retry).is_ok() {
                    return;
                }
                if !published {
                    retry = retry.saturating_mul(2).min(timings.retry_max);
                }
            }
        }
    }
}

enum SessionEnd {
    Stopped,
    Failed { published: bool },
}

fn run_session(
    program: &OsString,
    timings: Timings,
    quota: &Arc<Mutex<Option<CodexQuota>>>,
    stop: &mpsc::Receiver<()>,
    account: &mut Option<String>,
) -> SessionEnd {
    let mut child = match Command::new(program)
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return SessionEnd::Failed { published: false },
    };
    let mut input = child.stdin.take().expect("piped Codex stdin");
    let mut output = child.stdout.take().expect("piped Codex stdout");
    if set_nonblocking(&output).is_err()
        || input
            .write_all(b"{\"id\":0,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"eon\",\"version\":\"0.1.0\"},\"capabilities\":{\"experimentalApi\":false}}}\n")
            .is_err()
    {
        return fail_session(&mut child, input, quota, false, timings);
    }

    let mut buffer = Vec::new();
    let mut initialized = false;
    let mut deadline = Instant::now() + timings.timeout;
    let mut pending = None;
    let mut next_id = 1_u64;
    let mut next_read = Instant::now();
    let mut last_read = None;
    let mut published = false;

    loop {
        if stop.try_recv().is_ok() {
            stop_child(&mut child, input, timings.shutdown);
            return SessionEnd::Stopped;
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return fail_session(&mut child, input, quota, published, timings);
        }
        let now = Instant::now();
        if now >= deadline && (!initialized || pending.is_some()) {
            return fail_session(&mut child, input, quota, published, timings);
        }
        if initialized && pending.is_none() && now >= next_read {
            let id = next_id;
            next_id = next_id.saturating_add(1);
            if writeln!(
                input,
                "{{\"id\":{id},\"method\":\"account/rateLimits/read\",\"params\":{{\"excludeResetCreditDetails\":true,\"supportsLunaReserve\":false}}}}"
            )
            .is_err()
            {
                return fail_session(&mut child, input, quota, published, timings);
            }
            pending = Some(id);
            last_read = Some(now);
            deadline = now + timings.timeout;
        }

        match read_lines(&mut output, &mut buffer) {
            Ok((lines, eof)) => {
                for line in lines {
                    let message = match parse_message(&line, initialized, pending) {
                        Ok(message) => message,
                        Err(()) => {
                            return fail_session(&mut child, input, quota, published, timings);
                        }
                    };
                    match message {
                        Message::Initialized => {
                            if input.write_all(b"{\"method\":\"initialized\"}\n").is_err() {
                                return fail_session(&mut child, input, quota, published, timings);
                            }
                            initialized = true;
                            next_read = Instant::now();
                        }
                        Message::Quota(result) => {
                            let observed_at = match epoch_seconds() {
                                Some(value) => value,
                                None => {
                                    return fail_session(
                                        &mut child, input, quota, published, timings,
                                    );
                                }
                            };
                            let (new_account, fresh) = match normalize(&result, observed_at) {
                                Ok(value) => value,
                                Err(()) => {
                                    return fail_session(
                                        &mut child, input, quota, published, timings,
                                    );
                                }
                            };
                            if let Some(new_account) = new_account {
                                if account
                                    .as_ref()
                                    .is_some_and(|current| current != &new_account)
                                {
                                    publish(quota, None);
                                }
                                *account = Some(new_account);
                            }
                            publish(quota, Some(fresh));
                            published = true;
                            pending = None;
                            next_read = Instant::now() + timings.refresh;
                        }
                        Message::RateLimitsUpdated => {
                            next_read = last_read
                                .map(|read| read + timings.refresh)
                                .unwrap_or_else(Instant::now);
                        }
                        Message::AccountUpdated => {
                            *account = None;
                            publish(quota, None);
                            if pending.is_some() {
                                return fail_session(&mut child, input, quota, published, timings);
                            }
                            next_read = last_read
                                .map(|read| read + timings.refresh)
                                .unwrap_or_else(Instant::now);
                        }
                        Message::Other => {}
                    }
                }
                if eof {
                    return fail_session(&mut child, input, quota, published, timings);
                }
            }
            Err(()) => {
                return fail_session(&mut child, input, quota, published, timings);
            }
        }
        if stop.recv_timeout(timings.tick).is_ok() {
            stop_child(&mut child, input, timings.shutdown);
            return SessionEnd::Stopped;
        }
    }
}

enum Message {
    Initialized,
    Quota(Value),
    RateLimitsUpdated,
    AccountUpdated,
    Other,
}

fn parse_message(line: &[u8], initialized: bool, pending: Option<u64>) -> Result<Message, ()> {
    let message: Value = serde_json::from_slice(line).map_err(|_| ())?;
    let object = message.as_object().ok_or(())?;
    if let Some(id) = object.get("id") {
        let id = id.as_u64().ok_or(())?;
        if object.contains_key("error") {
            return Err(());
        }
        let result = object.get("result").ok_or(())?;
        if !initialized {
            if id == 0 && result.is_object() {
                return Ok(Message::Initialized);
            }
            return Err(());
        }
        if pending == Some(id) && result.is_object() {
            return Ok(Message::Quota(result.clone()));
        }
        return Err(());
    }
    let method = object.get("method").and_then(Value::as_str).ok_or(())?;
    Ok(match method {
        "account/rateLimits/updated" => Message::RateLimitsUpdated,
        "account/updated" => Message::AccountUpdated,
        _ => Message::Other,
    })
}

fn normalize(result: &Value, observed_at: u64) -> Result<(Option<String>, CodexQuota), ()> {
    let object = result.as_object().ok_or(())?;
    let account = match object.get("accountId") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.len() <= 1024 => Some(value.clone()),
        _ => return Err(()),
    };
    let state = match object.get("ordinaryUsageAllowed") {
        Some(Value::Bool(true)) => CodexQuotaState::Fresh,
        Some(Value::Bool(false)) => CodexQuotaState::Blocked,
        None | Some(Value::Null) => CodexQuotaState::Unknown,
        _ => return Err(()),
    };
    if state != CodexQuotaState::Fresh {
        return Ok((
            account,
            CodexQuota {
                state,
                observed_at,
                windows: Vec::new(),
            },
        ));
    }
    let codex_bucket = match object.get("rateLimitsByLimitId") {
        None | Some(Value::Null) => None,
        Some(Value::Object(limits)) => limits.get("codex"),
        _ => return Err(()),
    };
    let bucket = codex_bucket
        .or_else(|| object.get("rateLimits"))
        .and_then(Value::as_object)
        .ok_or(())?;
    let mut windows = Vec::new();
    for name in ["primary", "secondary"] {
        match bucket.get(name) {
            None | Some(Value::Null) => {}
            Some(window) => windows.push(normalize_window(window, observed_at)?),
        }
    }
    if windows.is_empty() {
        return Err(());
    }
    Ok((
        account,
        CodexQuota {
            state,
            observed_at,
            windows,
        },
    ))
}

fn normalize_window(value: &Value, observed_at: u64) -> Result<CodexQuotaWindow, ()> {
    let object = value.as_object().ok_or(())?;
    let duration_minutes = u32::try_from(
        object
            .get("windowDurationMins")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or(())?,
    )
    .map_err(|_| ())?;
    let used = object
        .get("usedPercent")
        .and_then(Value::as_i64)
        .filter(|value| (0..=100).contains(value))
        .ok_or(())?;
    let resets_at = match object.get("resetsAt") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            u64::try_from(value.as_i64().filter(|reset| *reset > 0).ok_or(())?).map_err(|_| ())?,
        ),
    };
    if resets_at.is_some_and(|reset| reset <= observed_at) {
        return Err(());
    }
    Ok(CodexQuotaWindow {
        duration_minutes,
        remaining_percent: u8::try_from(100 - used).map_err(|_| ())?,
        resets_at,
    })
}

fn read_lines(output: &mut ChildStdout, buffer: &mut Vec<u8>) -> Result<(Vec<Vec<u8>>, bool), ()> {
    let mut chunk = [0; 8192];
    let mut eof = false;
    for _ in 0..8 {
        match output.read(&mut chunk) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(()),
        }
    }
    let mut lines = Vec::new();
    while lines.len() < MAX_MESSAGES_PER_TICK {
        let Some(end) = buffer.iter().position(|byte| *byte == b'\n') else {
            break;
        };
        if end > MAX_LINE_BYTES {
            return Err(());
        }
        let mut line = buffer.drain(..=end).collect::<Vec<_>>();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            return Err(());
        }
        lines.push(line);
    }
    if buffer.len() > MAX_LINE_BYTES {
        return Err(());
    }
    Ok((lines, eof))
}

fn set_nonblocking(output: &ChildStdout) -> std::io::Result<()> {
    let fd = output.as_raw_fd();
    // SAFETY: fcntl reads and updates flags for this live pipe descriptor only.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn stop_child(child: &mut Child, input: ChildStdin, timeout: Duration) {
    drop(input);
    let deadline = Instant::now() + timeout;
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().ok().flatten().is_none() {
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: a negative pid targets only the process group created for this child.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn fail_session(
    child: &mut Child,
    input: ChildStdin,
    quota: &Arc<Mutex<Option<CodexQuota>>>,
    published: bool,
    timings: Timings,
) -> SessionEnd {
    mark_stale(quota);
    stop_child(child, input, timings.shutdown);
    SessionEnd::Failed { published }
}

fn publish(shared: &Arc<Mutex<Option<CodexQuota>>>, quota: Option<CodexQuota>) {
    if let Ok(mut current) = shared.lock() {
        *current = quota;
    }
}

fn mark_stale(shared: &Arc<Mutex<Option<CodexQuota>>>) {
    let Some(now) = epoch_seconds() else { return };
    let Ok(mut current) = shared.lock() else {
        return;
    };
    if let Some(quota) = current.as_mut() {
        if quota.state == CodexQuotaState::Fresh {
            quota.state = CodexQuotaState::Stale;
        } else if quota.state != CodexQuotaState::Stale {
            *current = None;
            return;
        }
    }
    expire_stale(&mut current, now);
}

fn expire_stale(quota: &mut Option<CodexQuota>, now: u64) {
    let Some(current) = quota
        .as_mut()
        .filter(|quota| quota.state == CodexQuotaState::Stale)
    else {
        return;
    };
    current
        .windows
        .retain(|window| window.resets_at.is_some_and(|reset| reset > now));
    if current.windows.is_empty() {
        *quota = None;
    }
}

fn epoch_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, thread};

    fn executable(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn wait_for(provider: &Provider, expected: Option<CodexQuotaState>) -> Option<CodexQuota> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let quota = provider.snapshot();
            if quota.as_ref().map(|quota| quota.state) == expected {
                return quota;
            }
            assert!(
                Instant::now() < deadline,
                "Codex quota state did not change"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn normalization_exposes_only_bounded_truthful_facts() {
        let observed = 1_800_000_000;
        let result: Value = serde_json::from_str(
            r#"{"ordinaryUsageAllowed":true,"accountId":"private","rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1800003600},"secondary":{"usedPercent":60,"windowDurationMins":10080,"resetsAt":null}},"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":10,"windowDurationMins":60,"resetsAt":1800007200},"secondary":{"usedPercent":20,"windowDurationMins":60,"resetsAt":1800010800}}},"rateLimitResetCredits":{"availableCount":4},"rateLimitUpsell":{"secret":"ignored"}}"#,
        )
        .unwrap();
        let (account, quota) = normalize(&result, observed).unwrap();
        assert_eq!(account.as_deref(), Some("private"));
        assert_eq!(quota.state, CodexQuotaState::Fresh);
        assert_eq!(quota.windows.len(), 2);
        assert_eq!(quota.windows[0].remaining_percent, 90);
        assert_eq!(quota.windows[0].duration_minutes, 60);
        assert_eq!(quota.windows[1].duration_minutes, 60);

        for (permission, state) in [
            ("false", CodexQuotaState::Blocked),
            ("null", CodexQuotaState::Unknown),
        ] {
            let value: Value = serde_json::from_str(&format!(
                "{{\"ordinaryUsageAllowed\":{permission},\"accountId\":null}}"
            ))
            .unwrap();
            let (_, quota) = normalize(&value, observed).unwrap();
            assert_eq!(quota.state, state);
            assert!(quota.windows.is_empty());
        }

        for invalid in [
            r#"{"ordinaryUsageAllowed":true,"rateLimits":{"primary":{"usedPercent":101,"windowDurationMins":60,"resetsAt":1800007200}}}"#,
            r#"{"ordinaryUsageAllowed":true,"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":0,"resetsAt":1800007200}}}"#,
            r#"{"ordinaryUsageAllowed":true,"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":60,"resetsAt":1799999999}}}"#,
            r#"{"ordinaryUsageAllowed":true,"rateLimits":{}}"#,
            r#"{"ordinaryUsageAllowed":true,"rateLimitsByLimitId":[],"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":60,"resetsAt":1800007200}}}"#,
        ] {
            assert!(normalize(&serde_json::from_str(invalid).unwrap(), observed).is_err());
        }

        let mut buffer = vec![b'x'; MAX_LINE_BYTES + 1];
        let mut child = Command::new("printf")
            .arg("")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = child.stdout.take().unwrap();
        child.wait().unwrap();
        set_nonblocking(&output).unwrap();
        assert!(read_lines(&mut output, &mut buffer).is_err());
        buffer.clear();
        assert_eq!(
            read_lines(&mut output, &mut buffer).unwrap(),
            (vec![], true)
        );
        assert!(parse_message(b"not-json", true, Some(1)).is_err());
        assert!(parse_message(b"{}", true, None).is_err());
        assert!(matches!(
            parse_message(br#"{"method":"future/event"}"#, true, None),
            Ok(Message::Other)
        ));
        assert!(parse_message(b"{\"id\":2,\"result\":{}}", true, Some(1)).is_err());
    }

    #[test]
    fn provider_lifecycle_is_fail_closed_and_bounded() {
        let root = std::env::temp_dir().join(format!("eon-codex-quota-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let program = root.join("codex");
        let stopped = root.join("stopped");
        executable(
            &program,
            &format!(
                r#"#!/bin/sh
reads=0
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{{"id":0,"result":{{}}}}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"account/rateLimits/read"'*)
      case "$line" in
        *'"excludeResetCreditDetails":true'*'"supportsLunaReserve":false'*) ;;
        *) exit 9 ;;
      esac
      reads=$((reads + 1))
      if [ "$reads" = 1 ]; then
        printf '%s\n' '{{"id":1,"result":{{"ordinaryUsageAllowed":true,"accountId":"private-a","rateLimits":{{"primary":{{"usedPercent":25,"windowDurationMins":300,"resetsAt":4000000000}},"secondary":null}}}}}}'
        sleep 0.05
        printf '%s\n' '{{"method":"account/updated","params":{{}}}}'
      elif [ "$reads" = 2 ]; then
        printf '%s\n' '{{"id":2,"result":{{"ordinaryUsageAllowed":false,"accountId":"private-b"}}}}'
      else
        printf '%s\n' '{{"method":"account/updated","params":{{}}}}'
        sleep 0.05
        printf '%s\n' '{{"id":3,"result":{{"ordinaryUsageAllowed":true,"accountId":"private-b","rateLimits":{{"primary":{{"usedPercent":5,"windowDurationMins":300,"resetsAt":4000000000}}}}}}}}'
      fi ;;
  esac
done
printf stopped > '{}'
"#,
                stopped.display()
            ),
        );
        let timings = Timings {
            refresh: Duration::from_millis(100),
            timeout: Duration::from_millis(300),
            retry_max: Duration::from_millis(400),
            tick: Duration::from_millis(5),
            shutdown: Duration::from_millis(200),
        };
        let provider = Provider::start_with(program.clone().into_os_string(), timings);
        let fresh = wait_for(&provider, Some(CodexQuotaState::Fresh)).unwrap();
        assert_eq!(fresh.windows[0].remaining_percent, 75);
        wait_for(&provider, None);
        wait_for(&provider, Some(CodexQuotaState::Blocked));
        wait_for(&provider, None);
        thread::sleep(Duration::from_millis(75));
        assert!(provider.snapshot().is_none());
        drop(provider);
        assert_eq!(fs::read_to_string(&stopped).unwrap(), "stopped");

        let launches = root.join("launches");
        let first_failure = root.join("first-failure");
        let second_failure = root.join("second-failure");
        executable(
            &program,
            &format!(
                r#"#!/bin/sh
if [ ! -e '{}' ]; then printf x > '{}'; exit 1; fi
if [ ! -e '{}' ]; then printf x > '{}'; exit 1; fi
printf x >> '{}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{{"id":0,"result":{{}}}}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"account/rateLimits/read"'*)
      printf '%s\n' '{{"id":1,"result":{{"ordinaryUsageAllowed":true,"accountId":"private","rateLimits":{{"primary":{{"usedPercent":10,"windowDurationMins":60,"resetsAt":4000000000}},"secondary":{{"usedPercent":20,"windowDurationMins":10080,"resetsAt":null}}}}}}}}'
      sleep 0.05
      printf 'not-json\n'
      exit 1 ;;
  esac
done
"#,
                first_failure.display(),
                first_failure.display(),
                second_failure.display(),
                second_failure.display(),
                launches.display()
            ),
        );
        let provider = Provider::start_with(program.clone().into_os_string(), timings);
        wait_for(&provider, Some(CodexQuotaState::Fresh));
        let stale = wait_for(&provider, Some(CodexQuotaState::Stale)).unwrap();
        assert_eq!(stale.windows.len(), 1);
        let deadline = Instant::now() + Duration::from_millis(300);
        while fs::read_to_string(&launches).unwrap() != "xx" && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fs::read_to_string(&launches).unwrap(), "xx");
        drop(provider);

        let ready = root.join("ready");
        executable(
            &program,
            &format!(
                "#!/bin/sh\nprintf ready > '{}'\nwhile :; do sleep 1; done\n",
                ready.display()
            ),
        );
        let provider = Provider::start_with(
            program.into_os_string(),
            Timings {
                shutdown: Duration::from_millis(20),
                ..timings
            },
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ready.exists());
        let started = Instant::now();
        drop(provider);
        assert!(started.elapsed() < Duration::from_millis(500));
        fs::remove_dir_all(root).unwrap();
    }
}
