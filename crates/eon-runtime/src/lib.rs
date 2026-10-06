//! Eon's runtime mechanisms, invoked with assembly-owned product inputs.

mod cli;
mod codex_quota;
mod control;
mod generation;
mod managed_environment;
mod sessions;
mod supervisor;
mod windows;
mod workspace;

pub use cli::{ComponentFacts, Inputs, run};
pub use managed_environment::{
    Defaults, ManagedPrograms, PopupCatalog, PopupCommand, PopupDefinition, ShellConfig,
    StartupAnimation, TerminalConfig,
};

#[cfg(test)]
mod fixtures;
