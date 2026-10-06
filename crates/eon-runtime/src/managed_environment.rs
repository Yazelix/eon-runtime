use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use eon_workspace_protocol::v7::{
    ALT, CTRL, MAX_ENTRIES, MAX_ENTRY_ID_BYTES, MAX_LABEL_BYTES, PopupGeometry, SHIFT, SUPER,
    Shortcut,
};

pub(super) fn nonempty_environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EonConfig {
    shell: ShellSettings,
    terminal: TerminalSettings,
    anima: AnimationSettings,
    popup: PopupSettings,
    popups: BTreeMap<String, PopupEntrySettings>,
}

pub struct Defaults {
    pub shell: ShellConfig,
    pub terminal: TerminalConfig,
    pub anima: StartupAnimation,
    pub popups: PopupCatalog,
    pub custom_popup_keep_alive: bool,
    pub ansi_palette: &'static str,
    pub agent_commands: &'static [&'static [&'static str]],
}

#[derive(Clone, Debug)]
pub struct StartupAnimation {
    pub enabled: bool,
    pub style: String,
    pub duration_seconds: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AnimationSettings {
    enabled: Option<bool>,
    style: Option<String>,
    duration_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PopupSettings {
    side_margin: Option<f32>,
    vertical_margin: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PopupEntrySettings {
    command: Option<PopupCommandSetting>,
    keybinding: Option<String>,
    label: Option<String>,
    enabled: Option<bool>,
    keep_alive: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PopupCommandSetting {
    Named(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PopupCommand {
    AgentAuto,
    Argv(Vec<OsString>),
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupDefinition {
    pub id: String,
    pub label: String,
    pub shortcut: Shortcut,
    pub command: PopupCommand,
    pub keep_alive: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PopupCatalog {
    pub geometry: PopupGeometry,
    pub entries: Vec<PopupDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub command: Vec<String>,
    pub starship: bool,
    pub zoxide: bool,
    pub atuin: bool,
    pub carapace: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShellSettings {
    command: Option<Vec<String>>,
    starship: Option<bool>,
    zoxide: Option<bool>,
    atuin: Option<bool>,
    carapace: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub background_opacity: f32,
    pub background_blur: bool,
    pub pane_frames: bool,
    pub cursor_trail_color: Option<String>,
    pub font_family: Option<String>,
    pub font_fallbacks: Vec<String>,
    pub font_size: Option<f32>,
    pub line_height: Option<f32>,
    pub columns: Option<u16>,
    pub rows: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TerminalSettings {
    background_opacity: Option<f32>,
    background_blur: Option<bool>,
    pane_frames: Option<bool>,
    cursor_trail_color: Option<String>,
    font_family: Option<String>,
    font_fallbacks: Option<Vec<String>>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    columns: Option<u16>,
    rows: Option<u16>,
}

impl TerminalConfig {
    pub(crate) fn requires_startup_admission(&self) -> bool {
        self.cursor_trail_color.is_some()
            || self.font_family.is_some()
            || !self.font_fallbacks.is_empty()
            || self.font_size.is_some()
            || self.line_height.is_some()
            || self.columns.is_some()
            || self.rows.is_some()
    }
}

pub struct ManagedPrograms {
    pub nu: PathBuf,
    pub bash: PathBuf,
    pub zsh: PathBuf,
    pub fish: PathBuf,
    pub helix: PathBuf,
    pub yazi: PathBuf,
    pub ya: PathBuf,
    pub lazygit: PathBuf,
    pub nu_vendor_autoload: Option<PathBuf>,
    pub bash_rc: Option<PathBuf>,
    pub zsh_config: Option<PathBuf>,
    pub fish_init: Option<PathBuf>,
    pub shell_bin: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    Nu,
    Bash,
    Zsh,
    Fish,
    Helix,
    Yazi,
    Ya,
    LazyGit,
}

pub(crate) fn tool(invocation: &OsStr) -> Option<Tool> {
    let name = Path::new(invocation).file_name()?.to_str()?;
    match name {
        "eon-nu" | "nu" => Some(Tool::Nu),
        "eon-bash" | "bash" => Some(Tool::Bash),
        "eon-zsh" | "zsh" => Some(Tool::Zsh),
        "eon-fish" | "fish" => Some(Tool::Fish),
        "eon-hx" | "hx" => Some(Tool::Helix),
        "eon-yazi" | "yazi" => Some(Tool::Yazi),
        "eon-ya" | "ya" => Some(Tool::Ya),
        "eon-lazygit" | "eon-lg" | "lazygit" => Some(Tool::LazyGit),
        _ => None,
    }
}

pub(crate) fn shell_command(root: &Path, defaults: &Defaults) -> Result<Vec<String>, String> {
    Ok(read_shell_config(root, defaults)?.command)
}

pub(crate) fn startup_animation(
    root: &Path,
    defaults: &Defaults,
) -> Result<Option<StartupAnimation>, String> {
    let settings = read_config(root)?.anima;
    let anima = StartupAnimation {
        enabled: settings.enabled.unwrap_or(defaults.anima.enabled),
        style: settings
            .style
            .unwrap_or_else(|| defaults.anima.style.clone()),
        duration_seconds: settings
            .duration_seconds
            .unwrap_or(defaults.anima.duration_seconds),
    };
    if anima.style.is_empty()
        || anima.style.trim() != anima.style
        || anima.style.len() > 128
        || anima.style.chars().any(char::is_control)
    {
        return Err("anima.style must be a nonempty trimmed name of at most 128 UTF-8 bytes without controls".into());
    }
    if !(1..=30).contains(&anima.duration_seconds) {
        return Err("anima.duration_seconds must be from 1 through 30".into());
    }
    Ok(anima.enabled.then_some(anima))
}

pub(crate) fn terminal_presentation(
    root: &Path,
    defaults: &Defaults,
) -> Result<TerminalConfig, String> {
    let settings = read_config(root)?.terminal;
    let base = &defaults.terminal;
    let terminal = TerminalConfig {
        background_opacity: settings
            .background_opacity
            .unwrap_or(base.background_opacity),
        background_blur: settings.background_blur.unwrap_or(base.background_blur),
        pane_frames: settings.pane_frames.unwrap_or(base.pane_frames),
        cursor_trail_color: settings
            .cursor_trail_color
            .or_else(|| base.cursor_trail_color.clone()),
        font_family: settings.font_family.or_else(|| base.font_family.clone()),
        font_fallbacks: settings
            .font_fallbacks
            .unwrap_or_else(|| base.font_fallbacks.clone()),
        font_size: settings.font_size.or(base.font_size),
        line_height: settings.line_height.or(base.line_height),
        columns: settings.columns.or(base.columns),
        rows: settings.rows.or(base.rows),
    };
    if !terminal.background_opacity.is_finite()
        || !(0.0..=1.0).contains(&terminal.background_opacity)
    {
        return Err(
            "terminal.background_opacity must be a finite number from 0.0 through 1.0".into(),
        );
    }
    for (field, value, minimum, maximum) in [
        ("font_size", terminal.font_size, 6.0, 96.0),
        ("line_height", terminal.line_height, 1.0, 3.0),
    ] {
        if value.is_some_and(|value| !value.is_finite() || !(minimum..=maximum).contains(&value)) {
            return Err(format!(
                "terminal.{field} must be a finite number from {minimum} through {maximum}"
            ));
        }
    }
    if terminal.font_fallbacks.len() > 8 {
        return Err("terminal.font_fallbacks accepts at most eight families".into());
    }
    for (field, family) in terminal
        .font_family
        .iter()
        .map(|family| ("font_family", family))
        .chain(
            terminal
                .font_fallbacks
                .iter()
                .map(|family| ("font_fallbacks", family)),
        )
    {
        if family.is_empty()
            || family.trim() != family
            || family.len() > 128
            || family.chars().any(char::is_control)
        {
            return Err(format!(
                "terminal.{field} requires nonempty trimmed family names of at most 128 UTF-8 bytes without controls"
            ));
        }
    }
    if terminal.columns == Some(0)
        || terminal.rows == Some(0)
        || u32::from(terminal.columns.unwrap_or(1)) * u32::from(terminal.rows.unwrap_or(1))
            > orbit_protocol::MAX_CELLS as u32
    {
        return Err("terminal.columns and terminal.rows must be positive and fit Orbit's 100,000-cell limit".into());
    }
    Ok(terminal)
}

pub(crate) fn popup_catalog(root: &Path, defaults: &Defaults) -> Result<PopupCatalog, String> {
    let mut config = read_config(root)?;
    let geometry = PopupGeometry {
        side_margin: config
            .popup
            .side_margin
            .unwrap_or(defaults.popups.geometry.side_margin),
        vertical_margin: config
            .popup
            .vertical_margin
            .unwrap_or(defaults.popups.geometry.vertical_margin),
    };
    for (field, value) in [
        ("side_margin", geometry.side_margin),
        ("vertical_margin", geometry.vertical_margin),
    ] {
        if !value.is_finite() || !(0.0..=128.0).contains(&value) {
            return Err(format!(
                "popup.{field} must be a finite number from 0 through 128"
            ));
        }
    }

    let mut entries = Vec::new();
    for builtin in &defaults.popups.entries {
        let id = builtin.id.as_str();
        let settings = config.popups.remove(id).unwrap_or_default();
        if id == "project" {
            if settings.command.is_some() {
                return Err("popups.project.command is Eon-owned".into());
            }
            if settings.enabled == Some(false) {
                return Err("popups.project.enabled cannot disable Eon's required chooser".into());
            }
            if settings.keep_alive.is_some_and(|value| value) {
                return Err("popups.project.keep_alive must remain false".into());
            }
        }
        if id == "anima" && settings.command.is_some() {
            return Err("popups.anima.command is Eon-owned".into());
        }
        if id == "anima" && settings.keep_alive == Some(true) {
            return Err("popups.anima.keep_alive must remain false".into());
        }
        if settings.enabled == Some(false) {
            continue;
        }
        let command = match settings.command {
            Some(command) => configured_popup_command(id, command)?,
            None => builtin.command.clone(),
        };
        entries.push(PopupDefinition {
            id: id.into(),
            label: settings.label.unwrap_or_else(|| builtin.label.clone()),
            shortcut: match settings.keybinding {
                Some(keybinding) => {
                    parse_shortcut(&format!("popups.{id}.keybinding"), &keybinding)?
                }
                None => builtin.shortcut.clone(),
            },
            command,
            keep_alive: settings.keep_alive.unwrap_or(builtin.keep_alive),
        });
    }

    for (id, settings) in config.popups {
        validate_popup_id(&id)?;
        if settings.enabled == Some(false) {
            continue;
        }
        let command = settings
            .command
            .ok_or_else(|| format!("popups.{id}.command is required"))?;
        let keybinding = settings
            .keybinding
            .ok_or_else(|| format!("popups.{id}.keybinding is required"))?;
        entries.push(PopupDefinition {
            label: settings.label.unwrap_or_else(|| id.clone()),
            shortcut: parse_shortcut(&format!("popups.{id}.keybinding"), &keybinding)?,
            command: configured_popup_command(&id, command)?,
            keep_alive: settings
                .keep_alive
                .unwrap_or(defaults.custom_popup_keep_alive),
            id,
        });
    }
    validate_popup_entries(&entries)?;
    Ok(PopupCatalog { geometry, entries })
}

fn configured_popup_command(
    id: &str,
    command: PopupCommandSetting,
) -> Result<PopupCommand, String> {
    match command {
        PopupCommandSetting::Named(value) if id == "agent" && value == "auto" => {
            Ok(PopupCommand::AgentAuto)
        }
        PopupCommandSetting::Named(_) => Err(format!(
            "popups.{id}.command must be a direct argv array{}",
            if id == "agent" { " or \"auto\"" } else { "" }
        )),
        PopupCommandSetting::Argv(argv) => {
            validate_popup_argv(id, &argv)?;
            Ok(PopupCommand::Argv(
                argv.into_iter().map(Into::into).collect(),
            ))
        }
    }
}

fn validate_popup_argv(id: &str, argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].is_empty() {
        return Err(format!(
            "popups.{id}.command requires a nonempty executable"
        ));
    }
    if argv.len() > 128 {
        return Err(format!("popups.{id}.command accepts at most 128 arguments"));
    }
    if argv.iter().any(|argument| argument.contains('\0')) {
        return Err(format!("popups.{id}.command must not contain NUL"));
    }
    if argv.iter().map(String::len).sum::<usize>() > 64 * 1024 {
        return Err(format!("popups.{id}.command exceeds 64 KiB"));
    }
    Ok(())
}

fn validate_popup_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ENTRY_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "popups.{id} id must be 1-{MAX_ENTRY_ID_BYTES} ASCII letters, digits, _ or -"
        ))
    }
}

fn parse_shortcut(path: &str, value: &str) -> Result<Shortcut, String> {
    let mut parts = value.split('+').collect::<Vec<_>>();
    let key = parts
        .pop()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("{path} must contain modifiers and one physical key joined by +"))?;
    let mut modifiers = 0;
    for modifier in parts {
        let bit = if modifier.eq_ignore_ascii_case("shift") {
            SHIFT
        } else if modifier.eq_ignore_ascii_case("ctrl") || modifier.eq_ignore_ascii_case("control")
        {
            CTRL
        } else if modifier.eq_ignore_ascii_case("alt") {
            ALT
        } else if modifier.eq_ignore_ascii_case("super") {
            SUPER
        } else {
            return Err(format!("{path} has unsupported modifier {modifier:?}"));
        };
        if modifiers & bit != 0 {
            return Err(format!("{path} repeats modifier {modifier:?}"));
        }
        modifiers |= bit;
    }
    let key = match key.as_bytes() {
        [letter] if letter.is_ascii_alphabetic() => {
            format!("Key{}", char::from(letter.to_ascii_uppercase()))
        }
        [digit] if digit.is_ascii_digit() => format!("Digit{}", char::from(*digit)),
        _ => key.into(),
    };
    let shortcut = Shortcut { modifiers, key };
    shortcut
        .validate()
        .map_err(|_| format!("{path} is not a supported modified physical key"))?;
    Ok(shortcut)
}

fn validate_popup_entries(entries: &[PopupDefinition]) -> Result<(), String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!(
            "popups accepts at most {MAX_ENTRIES} enabled entries"
        ));
    }
    let mut shortcuts = HashMap::new();
    for entry in entries {
        validate_popup_id(&entry.id)?;
        entry.shortcut.validate().map_err(|_| {
            format!(
                "popups.{}.keybinding is not a supported modified physical key",
                entry.id
            )
        })?;
        if entry.label.is_empty()
            || entry.label.len() > MAX_LABEL_BYTES
            || entry.label.chars().any(char::is_control)
        {
            return Err(format!(
                "popups.{}.label must be nonempty, at most {MAX_LABEL_BYTES} UTF-8 bytes, and contain no controls",
                entry.id
            ));
        }
        if workspace_shortcut(&entry.shortcut) {
            return Err(format!(
                "popups.{}.keybinding conflicts with an Eon workspace shortcut",
                entry.id
            ));
        }
        if let Some(previous) = shortcuts.insert(entry.shortcut.clone(), entry.id.as_str()) {
            return Err(format!(
                "popups.{}.keybinding conflicts with popups.{previous}.keybinding",
                entry.id
            ));
        }
    }
    Ok(())
}

fn workspace_shortcut(shortcut: &Shortcut) -> bool {
    let key = shortcut.key.as_str();
    (shortcut.modifiers == ALT
        && matches!(
            key,
            "KeyH"
                | "KeyJ"
                | "KeyK"
                | "KeyL"
                | "KeyM"
                | "Digit0"
                | "Digit1"
                | "Digit2"
                | "Digit3"
                | "Digit4"
                | "Digit5"
                | "Digit6"
                | "Digit7"
                | "Digit8"
                | "Digit9"
                | "Slash"
        ))
        || (shortcut.modifiers == (ALT | SHIFT) && matches!(key, "KeyT" | "KeyW"))
        || (shortcut.modifiers == (CTRL | ALT) && matches!(key, "KeyH" | "KeyJ" | "KeyK" | "KeyL"))
        || (shortcut.modifiers == (CTRL | SHIFT) && matches!(key, "KeyC" | "KeyV"))
}

pub(crate) fn prepare_popup_command(
    command: &PopupCommand,
    session_bin: Option<&Path>,
    directory: &Path,
    defaults: &Defaults,
) -> Result<Vec<OsString>, String> {
    match command {
        PopupCommand::Argv(argv) => {
            if !popup_executable(&argv[0], session_bin, directory)? {
                return Err(format!(
                    "popup executable {:?} is unavailable on Eon's Session PATH",
                    argv[0]
                ));
            }
            Ok(argv.clone())
        }
        PopupCommand::Project => Ok(Vec::new()),
        PopupCommand::AgentAuto => {
            for candidate in defaults.agent_commands {
                let argv = candidate.iter().map(OsString::from).collect::<Vec<_>>();
                if popup_executable(&argv[0], session_bin, directory)? {
                    return Ok(argv);
                }
            }
            Err("no supported Agent executable is available on Eon's Session PATH; install codex, grok, opencode, pi, or claude, or configure popups.agent.command".into())
        }
    }
}

fn popup_executable(
    program: &OsStr,
    session_bin: Option<&Path>,
    directory: &Path,
) -> Result<bool, String> {
    let search_path = session_bin.map(session_path).transpose()?;
    let program = Path::new(program);
    let executable = |path: &Path| {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if program.is_absolute() {
        return Ok(executable(program));
    }
    if program.components().count() > 1 {
        return Ok(executable(&directory.join(program)));
    }
    Ok(search_path
        .or_else(|| env::var_os("PATH"))
        .as_deref()
        .map(env::split_paths)
        .into_iter()
        .flatten()
        .any(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                directory.join(path)
            };
            executable(&path.join(program))
        }))
}

pub(crate) fn session_path(prefix: &Path) -> Result<OsString, String> {
    prepend_path(prefix, env::var_os("PATH").as_deref(), "Eon Session")
}

pub(crate) fn command(
    tool: Tool,
    programs: &ManagedPrograms,
    config: &Path,
    arguments: &[OsString],
    defaults: &Defaults,
) -> Result<Command, String> {
    let shell = match tool {
        Tool::Nu | Tool::Bash | Tool::Zsh | Tool::Fish => {
            Some(read_shell_config(config, defaults)?)
        }
        _ => None,
    };
    let program = match tool {
        Tool::Nu => &programs.nu,
        Tool::Bash => &programs.bash,
        Tool::Zsh => &programs.zsh,
        Tool::Fish => &programs.fish,
        Tool::Helix => &programs.helix,
        Tool::Yazi => &programs.yazi,
        Tool::Ya => &programs.ya,
        Tool::LazyGit => &programs.lazygit,
    };
    let mut command = Command::new(program);
    match tool {
        Tool::Nu => {
            command.arg("--experimental-options=native-clip");
            if let Some(path) = &programs.nu_vendor_autoload {
                command.env(
                    "NU_VENDOR_AUTOLOAD_DIR",
                    path.join(
                        integration_mask(
                            shell.as_ref().unwrap(),
                            env::var_os("ATUIN_NOBIND").is_some(),
                        )
                        .to_string(),
                    ),
                );
            }
        }
        Tool::Bash => {
            if let Some(path) = &programs.bash_rc {
                command.args([OsStr::new("--rcfile"), path.as_os_str()]);
            }
        }
        Tool::Zsh => {
            if let Some(path) = &programs.zsh_config {
                command.env("ZDOTDIR", path);
                if let Some(user) = env::var_os("EON_USER_ZDOTDIR")
                    .or_else(|| env::var_os("ZDOTDIR"))
                    .or_else(|| env::var_os("HOME"))
                {
                    command.env("EON_USER_ZDOTDIR", user);
                }
            }
        }
        Tool::Fish => {
            if let Some(path) = &programs.fish_init {
                command
                    .env("EON_FISH_INIT", path)
                    .args(["-C", "source \"$EON_FISH_INIT\""]);
            }
        }
        Tool::Yazi | Tool::Ya => {
            command.env_remove("YAZI_CONFIG_HOME");
        }
        Tool::LazyGit => {
            command
                .env_remove("CONFIG_DIR")
                .env_remove("LG_CONFIG_FILE")
                .env("XDG_CONFIG_DIRS", config);
        }
        Tool::Helix => {
            command
                .env_remove("CARGO_MANIFEST_DIR")
                .env_remove("HELIX_RUNTIME")
                .env_remove("HELIX_STEEL_CONFIG");
        }
    }
    if let Some(shell) = &shell {
        for (name, enabled) in [
            ("STARSHIP", shell.starship),
            ("ZOXIDE", shell.zoxide),
            ("ATUIN", shell.atuin),
            ("CARAPACE", shell.carapace),
        ] {
            command.env(format!("EON_SHELL_{name}"), if enabled { "1" } else { "0" });
        }
        if let Some(bin) = &programs.shell_bin {
            command.env(
                "PATH",
                prepend_path(bin, env::var_os("PATH").as_deref(), "managed shell")?,
            );
        }
    }
    command.args(arguments).env("EON_CONFIG_HOME", config);
    if shell.is_none() {
        command.env("XDG_CONFIG_HOME", config);
    }
    Ok(command)
}

fn integration_mask(shell: &ShellConfig, atuin_nobind: bool) -> u8 {
    u8::from(shell.starship)
        | (u8::from(shell.zoxide) << 1)
        | (u8::from(shell.atuin) << 2)
        | (u8::from(shell.carapace) << 3)
        | (u8::from(shell.atuin && atuin_nobind) << 4)
}

fn read_shell_config(root: &Path, defaults: &Defaults) -> Result<ShellConfig, String> {
    let settings = read_config(root)?.shell;
    let base = &defaults.shell;
    let shell = ShellConfig {
        command: settings.command.unwrap_or_else(|| base.command.clone()),
        starship: settings.starship.unwrap_or(base.starship),
        zoxide: settings.zoxide.unwrap_or(base.zoxide),
        atuin: settings.atuin.unwrap_or(base.atuin),
        carapace: settings.carapace.unwrap_or(base.carapace),
    };
    if shell.command.is_empty() || shell.command[0].is_empty() {
        return Err("shell.command must not be empty".into());
    }
    if shell.command.iter().any(|argument| argument.contains('\0')) {
        return Err("shell.command must not contain NUL".into());
    }
    Ok(shell)
}

fn read_config(root: &Path) -> Result<EonConfig, String> {
    let path = root.join("config.toml");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EonConfig::default());
        }
        Err(error) => {
            return Err(format!("cannot read {}: {error}", path.display()));
        }
    };
    toml::from_str(&source)
        .map_err(|error| format!("invalid Eon configuration {}: {error}", path.display()))
}

fn prepend_path(prefix: &Path, path: Option<&OsStr>, owner: &str) -> Result<OsString, String> {
    let mut paths = path
        .map(env::split_paths)
        .into_iter()
        .flatten()
        .filter(|path| path != prefix)
        .collect::<Vec<_>>();
    paths.insert(0, prefix.into());
    env::join_paths(paths).map_err(|error| format!("cannot construct {owner} PATH: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedPrograms, PopupCommand, Tool, command as managed_command, integration_mask,
        popup_catalog, prepare_popup_command, prepend_path, read_shell_config, startup_animation,
        terminal_presentation, tool,
    };
    use std::{
        ffi::{OsStr, OsString},
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
    };

    fn command_environment<'a>(command: &'a Command, name: &str) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| value)
    }

    #[test]
    fn supplied_defaults_survive_partial_overrides_and_repeated_reads() {
        let root = crate::supervisor::temporary_directory();
        let mut defaults = crate::fixtures::inputs().defaults;
        defaults.shell.command = vec!["supplied-shell".into(), "--interactive".into()];
        defaults.shell.starship = false;
        defaults.terminal.background_opacity = 0.37;
        defaults.terminal.background_blur = false;
        defaults.terminal.font_fallbacks = vec!["Supplied Font".into()];
        defaults.anima.style = "aquarium".into();
        defaults.anima.duration_seconds = 7;
        defaults.popups.geometry.side_margin = 23.0;
        defaults.custom_popup_keep_alive = false;
        defaults.popups.entries[1].label = "Supplied Git".into();
        defaults.popups.entries[1].command = PopupCommand::Argv(vec!["supplied-git".into()]);

        assert_eq!(
            super::shell_command(&root, &defaults).unwrap(),
            defaults.shell.command
        );
        assert!(!super::read_shell_config(&root, &defaults).unwrap().starship);
        assert_eq!(
            super::terminal_presentation(&root, &defaults)
                .unwrap()
                .font_fallbacks,
            ["Supplied Font"]
        );
        assert_eq!(
            super::startup_animation(&root, &defaults)
                .unwrap()
                .unwrap()
                .duration_seconds,
            7
        );
        assert_eq!(
            super::popup_catalog(&root, &defaults).unwrap(),
            defaults.popups
        );

        fs::write(root.join("config.toml"), "[shell]\nzoxide = false\n[terminal]\npane_frames = false\nfont_fallbacks = []\n[anima]\nduration_seconds = 2\n[popup]\nvertical_margin = 0\n[popups.git]\nkeep_alive = false\n[popups.extra]\ncommand = ['supplied-extra']\nkeybinding = 'Alt+Shift+F'\n").unwrap();
        let shell = super::read_shell_config(&root, &defaults).unwrap();
        assert_eq!(shell.command, defaults.shell.command);
        assert!(!shell.starship && !shell.zoxide && shell.atuin && shell.carapace);
        let terminal = super::terminal_presentation(&root, &defaults).unwrap();
        assert_eq!(terminal.background_opacity, 0.37);
        assert!(!terminal.background_blur && !terminal.pane_frames);
        assert!(terminal.font_fallbacks.is_empty());
        let anima = super::startup_animation(&root, &defaults).unwrap().unwrap();
        assert_eq!(
            (anima.style.as_str(), anima.duration_seconds),
            ("aquarium", 2)
        );
        let popups = super::popup_catalog(&root, &defaults).unwrap();
        assert_eq!(
            (popups.geometry.side_margin, popups.geometry.vertical_margin),
            (23.0, 0.0)
        );
        assert_eq!(
            popups.entries[1].command,
            defaults.popups.entries[1].command
        );
        assert_eq!(popups.entries[1].label, "Supplied Git");
        assert!(!popups.entries[1].keep_alive);
        assert!(!popups.entries.last().unwrap().keep_alive);

        fs::write(root.join("config.toml"), "[shell]\ncommand = ['later-shell']\n[terminal]\nbackground_opacity = 0.0\n[anima]\nenabled = false\n").unwrap();
        assert_eq!(
            super::shell_command(&root, &defaults).unwrap(),
            ["later-shell"]
        );
        assert_eq!(
            super::terminal_presentation(&root, &defaults)
                .unwrap()
                .background_opacity,
            0.0
        );
        assert!(
            super::startup_animation(&root, &defaults)
                .unwrap()
                .is_none()
        );
        fs::write(root.join("config.toml"), "[terminal]\nunknown = true\n").unwrap();
        assert!(
            super::terminal_presentation(&root, &defaults)
                .unwrap_err()
                .contains("unknown field")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shell_configuration_is_strict_and_defaults_without_a_file() {
        let inputs = crate::fixtures::inputs();
        let root = std::env::temp_dir().join(format!(
            "eon-managed-environment-test-{}-0",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let default = read_shell_config(&root, &inputs.defaults).unwrap();
        assert_eq!(default.command, ["fixture-shell"]);
        assert!(default.starship && default.zoxide && default.atuin && default.carapace);

        fs::write(
            root.join("config.toml"),
            "[shell]\ncommand = [\"eon-fish\", \"--no-config\"]\nstarship = false\nzoxide = false\natuin = false\ncarapace = false\n",
        )
        .unwrap();
        let configured = read_shell_config(&root, &inputs.defaults).unwrap();
        assert_eq!(configured.command, ["eon-fish", "--no-config"]);
        assert!(
            !configured.starship && !configured.zoxide && !configured.atuin && !configured.carapace
        );
        assert_eq!(integration_mask(&configured, true), 0);
        assert_eq!(integration_mask(&default, false), 15);
        assert_eq!(integration_mask(&default, true), 31);

        for (source, expected) in [
            ("[shell]\ncommand = []\n", "shell.command must not be empty"),
            (
                "[shell]\ncommand = [\"eon-nu\"]\nunknown = true\n",
                "unknown field `unknown`",
            ),
            ("[shell\n", "invalid Eon configuration"),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            assert!(
                read_shell_config(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains(expected)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_animation_settings_are_validated_and_can_be_disabled() {
        let inputs = crate::fixtures::inputs();
        let root = std::env::temp_dir().join(format!(
            "eon-managed-environment-test-{}-anima",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("config.toml"), "[anima]\nenabled = false\n").unwrap();
        assert!(
            startup_animation(&root, &inputs.defaults)
                .unwrap()
                .is_none()
        );
        fs::write(
            root.join("config.toml"),
            "[anima]\nstyle = 'aquarium'\nduration_seconds = 5\n",
        )
        .unwrap();
        let configured = startup_animation(&root, &inputs.defaults).unwrap().unwrap();
        assert_eq!(
            (configured.style.as_str(), configured.duration_seconds),
            ("aquarium", 5)
        );

        for (source, field) in [
            ("[anima]\nduration_seconds = 0\n", "anima.duration_seconds"),
            ("[anima]\nduration_seconds = 31\n", "anima.duration_seconds"),
            ("[anima]\nstyle = ' '\n", "anima.style"),
            ("[anima]\nstyle = \"bad\\nstyle\"\n", "anima.style"),
            ("[anima]\nenabled = 'yes'\n", "enabled"),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            assert!(
                startup_animation(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains(field)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_presentation_is_strict_and_bounded() {
        let inputs = crate::fixtures::inputs();
        let root = std::env::temp_dir().join(format!(
            "eon-managed-environment-test-{}-presentation",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let presentation = terminal_presentation(&root, &inputs.defaults).unwrap();
        assert_eq!(presentation.background_opacity, 0.6);
        assert!(presentation.background_blur);

        for (source, expected_opacity, expected_blur) in [
            ("", 0.6, true),
            ("[shell]\nstarship = false\n", 0.6, true),
            ("[terminal]\nbackground_opacity = 0.0\n", 0.0, true),
            (
                "[terminal]\nbackground_opacity = 0.88\nbackground_blur = false\n",
                0.88,
                false,
            ),
            (
                "[terminal]\nbackground_opacity = 1.0\nbackground_blur = true\n",
                1.0,
                true,
            ),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            let presentation = terminal_presentation(&root, &inputs.defaults).unwrap();
            assert_eq!(presentation.background_opacity, expected_opacity);
            assert_eq!(presentation.background_blur, expected_blur);
        }

        for (source, field) in [
            (
                "[terminal]\nbackground_opacity = nan\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_opacity = inf\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_opacity = -0.01\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_opacity = 1.01\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_opacity = \"0.88\"\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_opacity = 0.5\nbackground_opacity = 0.6\n",
                "background_opacity",
            ),
            (
                "[terminal]\nbackground_blur = \"true\"\n",
                "background_blur",
            ),
            (
                "[terminal]\nbackground_blur = true\nbackground_blur = false\n",
                "background_blur",
            ),
            ("[terminal]\nunknown = true\n", "unknown"),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            assert!(
                terminal_presentation(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains(field),
                "invalid {field} configuration did not name its field"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn popup_configuration_owns_defaults_overrides_and_collisions() {
        let inputs = crate::fixtures::inputs();
        let root = std::env::temp_dir().join(format!(
            "eon-managed-environment-test-{}-popups",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();

        let defaults = popup_catalog(&root, &inputs.defaults).unwrap();
        assert_eq!(
            defaults
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["project", "git", "agent", "anima"]
        );
        assert_eq!(
            (
                defaults.geometry.side_margin,
                defaults.geometry.vertical_margin
            ),
            (11.0, 7.0)
        );

        fs::write(
            root.join("config.toml"),
            r#"[popup]
side_margin = 12
vertical_margin = 0

[popups.git]
enabled = false

[popups.agent]
command = ["opencode", "--continue"]
keybinding = "Super+A"

[popups.files]
command = ["eon-yazi"]
keybinding = "Alt+Shift+F"
label = "Files"
keep_alive = false
"#,
        )
        .unwrap();
        let configured = popup_catalog(&root, &inputs.defaults).unwrap();
        assert_eq!(
            (
                configured.geometry.side_margin,
                configured.geometry.vertical_margin
            ),
            (12.0, 0.0)
        );
        assert_eq!(
            configured
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["project", "agent", "anima", "files"]
        );

        fs::write(
            root.join("config.toml"),
            "[popups.files]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(
            popup_catalog(&root, &inputs.defaults)
                .unwrap()
                .entries
                .len(),
            4
        );

        for (source, expected) in [
            (
                "[popups.anima]\nkeep_alive = true\n",
                "popups.anima.keep_alive",
            ),
            (
                "[popups.anima]\ncommand = ['other']\n",
                "popups.anima.command",
            ),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            assert!(
                popup_catalog(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains(expected)
            );
        }

        for keybinding in [
            "Alt+Slash",
            "Alt+0",
            "Alt+1",
            "Alt+2",
            "Alt+3",
            "Alt+4",
            "Alt+5",
            "Alt+6",
            "Alt+7",
            "Alt+8",
            "Alt+9",
        ] {
            fs::write(
                root.join("config.toml"),
                format!("[popups.extra]\ncommand = [\"tool\"]\nkeybinding = \"{keybinding}\"\n"),
            )
            .unwrap();
            assert!(
                popup_catalog(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains("conflicts with an Eon workspace shortcut"),
                "{keybinding} was not reserved"
            );
        }

        fs::write(
            root.join("config.toml"),
            "[popups.extra]\ncommand = [\"tool\"]\nkeybinding = \"Ctrl+Shift+O\"\n",
        )
        .unwrap();
        assert!(
            popup_catalog(&root, &inputs.defaults)
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.id == "extra")
        );

        for (source, expected) in [
            (
                "[popups.extra]\ncommand = [\"tool\"]\nkeybinding = \"Alt+M\"\n",
                "conflicts with an Eon workspace shortcut",
            ),
            (
                "[popups.extra]\ncommand = [\"tool\"]\nkeybinding = \"Alt+Z\"\n",
                "conflicts with popups.project.keybinding",
            ),
            (
                "[popups.project]\ncommand = [\"other\"]\n",
                "popups.project.command is Eon-owned",
            ),
        ] {
            fs::write(root.join("config.toml"), source).unwrap();
            assert!(
                popup_catalog(&root, &inputs.defaults)
                    .unwrap_err()
                    .contains(expected)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn popup_executable_preflight_uses_the_session_context() {
        let inputs = crate::fixtures::inputs();
        let root = std::env::temp_dir().join(format!(
            "eon-managed-environment-test-{}-popup-cwd",
            std::process::id()
        ));
        let launch = root.join("launch");
        fs::create_dir_all(&launch).unwrap();
        let executable = launch.join("tool");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let command = PopupCommand::Argv(vec!["./tool".into()]);

        assert_eq!(
            prepare_popup_command(&command, None, &launch, &inputs.defaults).unwrap(),
            [OsString::from("./tool")]
        );
        assert!(prepare_popup_command(&command, None, &root, &inputs.defaults).is_err());
        fs::create_dir(launch.join("bin")).unwrap();
        let path_tool = launch.join("bin/path-tool");
        fs::write(&path_tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path_tool, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            prepare_popup_command(
                &PopupCommand::Argv(vec!["path-tool".into()]),
                Some(Path::new("bin")),
                &launch,
                &inputs.defaults
            )
            .unwrap(),
            [OsString::from("path-tool")]
        );
        assert!(
            prepare_popup_command(
                &PopupCommand::Argv(vec![path_tool.into_os_string()]),
                Some(Path::new("invalid:path")),
                &launch,
                &inputs.defaults
            )
            .unwrap_err()
            .contains("cannot construct Eon Session PATH")
        );
        let mut defaults = crate::fixtures::inputs().defaults;
        defaults.agent_commands = &[
            &["./preferred-agent", "--preferred"],
            &["./tool", "--fallback"],
        ];
        assert_eq!(
            prepare_popup_command(&PopupCommand::AgentAuto, None, &launch, &defaults).unwrap(),
            [OsString::from("./tool"), OsString::from("--fallback")]
        );
        let preferred = launch.join("preferred-agent");
        fs::write(&preferred, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&preferred, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            prepare_popup_command(&PopupCommand::AgentAuto, None, &launch, &defaults).unwrap(),
            [
                OsString::from("./preferred-agent"),
                OsString::from("--preferred")
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_invocation_names_are_bounded() {
        use Tool::{Bash, Fish, Helix, LazyGit, Nu, Ya, Yazi, Zsh};

        for (name, expected) in [
            ("eon-nu", Some(Nu)),
            ("nu", Some(Nu)),
            ("eon-bash", Some(Bash)),
            ("bash", Some(Bash)),
            ("eon-zsh", Some(Zsh)),
            ("zsh", Some(Zsh)),
            ("eon-fish", Some(Fish)),
            ("fish", Some(Fish)),
            ("eon-hx", Some(Helix)),
            ("hx", Some(Helix)),
            ("eon-yazi", Some(Yazi)),
            ("yazi", Some(Yazi)),
            ("eon-ya", Some(Ya)),
            ("ya", Some(Ya)),
            ("eon-lazygit", Some(LazyGit)),
            ("eon-lg", Some(LazyGit)),
            ("lazygit", Some(LazyGit)),
            ("lg", None),
            ("eon-starship", None),
            ("eon-zoxide", None),
            ("eon", None),
        ] {
            assert_eq!(tool(Path::new(name).as_os_str()), expected);
        }
        assert_eq!(
            tool(Path::new("/nix/store/example/bin/eon-nu").as_os_str()),
            Some(Nu)
        );
    }

    #[test]
    fn managed_commands_use_exact_programs_and_private_configuration() {
        let inputs = crate::fixtures::inputs();
        use Tool::{Bash, Fish, Helix, LazyGit, Nu, Ya, Yazi, Zsh};

        let programs = ManagedPrograms {
            nu: "/managed/nu".into(),
            bash: "/managed/bash".into(),
            zsh: "/managed/zsh".into(),
            fish: "/managed/fish".into(),
            helix: "/managed/hx".into(),
            yazi: "/managed/yazi".into(),
            ya: "/managed/ya".into(),
            lazygit: "/managed/lazygit".into(),
            nu_vendor_autoload: Some("/managed/autoload".into()),
            bash_rc: Some("/managed/bashrc".into()),
            zsh_config: Some("/managed/zsh-config".into()),
            fish_init: Some("/managed/fish-init".into()),
            shell_bin: Some("/managed/bin".into()),
        };
        let config = Path::new("/private/eon");
        for (tool, program, removed_variables) in [
            (
                Helix,
                "/managed/hx",
                &["CARGO_MANIFEST_DIR", "HELIX_RUNTIME", "HELIX_STEEL_CONFIG"][..],
            ),
            (Yazi, "/managed/yazi", &["YAZI_CONFIG_HOME"][..]),
            (Ya, "/managed/ya", &["YAZI_CONFIG_HOME"][..]),
            (
                LazyGit,
                "/managed/lazygit",
                &["CONFIG_DIR", "LG_CONFIG_FILE"][..],
            ),
        ] {
            let command = managed_command(
                tool,
                &programs,
                config,
                &["--version".into()],
                &inputs.defaults,
            )
            .unwrap();
            assert_eq!(command.get_program(), program);
            assert_eq!(
                command.get_args().map(OsString::from).collect::<Vec<_>>(),
                vec![OsString::from("--version")]
            );
            for variable in ["EON_CONFIG_HOME", "XDG_CONFIG_HOME"] {
                assert_eq!(
                    command_environment(&command, variable),
                    Some(Some(config.as_os_str()))
                );
            }
            for variable in removed_variables {
                assert_eq!(command_environment(&command, variable), Some(None));
            }
            if tool == LazyGit {
                assert_eq!(
                    command_environment(&command, "XDG_CONFIG_DIRS"),
                    Some(Some(config.as_os_str()))
                );
            }
        }

        let command = managed_command(
            Nu,
            &programs,
            config,
            &["--version".into()],
            &inputs.defaults,
        )
        .unwrap();
        assert_eq!(command.get_program(), "/managed/nu");
        assert_eq!(
            command.get_args().map(OsString::from).collect::<Vec<_>>(),
            ["--experimental-options=native-clip", "--version"].map(OsString::from)
        );
        assert_eq!(
            command_environment(&command, "EON_CONFIG_HOME"),
            Some(Some(config.as_os_str()))
        );
        assert_eq!(
            command_environment(&command, "NU_VENDOR_AUTOLOAD_DIR"),
            Some(Some(OsStr::new("/managed/autoload/15")))
        );
        assert_eq!(command_environment(&command, "XDG_CONFIG_HOME"), None);
        assert_eq!(command_environment(&command, "STARSHIP_CONFIG"), None);

        for (tool, program, arguments) in [
            (
                Bash,
                "/managed/bash",
                vec!["--rcfile", "/managed/bashrc", "--version"],
            ),
            (Zsh, "/managed/zsh", vec!["--version"]),
            (
                Fish,
                "/managed/fish",
                vec!["-C", "source \"$EON_FISH_INIT\"", "--version"],
            ),
        ] {
            let command = managed_command(
                tool,
                &programs,
                config,
                &["--version".into()],
                &inputs.defaults,
            )
            .unwrap();
            assert_eq!(command.get_program(), program);
            assert_eq!(
                command.get_args().map(OsString::from).collect::<Vec<_>>(),
                arguments
                    .into_iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            );
            for integration in ["STARSHIP", "ZOXIDE", "ATUIN", "CARAPACE"] {
                assert_eq!(
                    command_environment(&command, &format!("EON_SHELL_{integration}")),
                    Some(Some(OsStr::new("1")))
                );
            }
        }
        assert_eq!(
            command_environment(
                &managed_command(Zsh, &programs, config, &[], &inputs.defaults).unwrap(),
                "ZDOTDIR"
            ),
            Some(Some(OsStr::new("/managed/zsh-config")))
        );
        assert_eq!(
            command_environment(
                &managed_command(Fish, &programs, config, &[], &inputs.defaults).unwrap(),
                "EON_FISH_INIT"
            ),
            Some(Some(OsStr::new("/managed/fish-init")))
        );
    }

    #[test]
    fn session_path_contains_one_managed_prefix() {
        let path = prepend_path(
            Path::new("/managed/bin"),
            Some(OsStr::new("/managed/bin:/usr/bin:/managed/bin")),
            "test",
        )
        .unwrap();

        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            ["/managed/bin", "/usr/bin"].map(PathBuf::from)
        );
    }
}
