//! Daemon system commands a Canvas panel may never send (M3-0 design spec
//! §4.1), and what makes a declared command entry unusable.
//!
//! One copy, shared by the daemon's canvas gate (which refuses these at run
//! time) and manifest validation (which refuses a pack that declares them, so
//! the install review never lists a capability the runtime will not grant).
//!
//! Names are compared exactly because the daemon dispatcher matches command
//! names exactly. The browser's `canvas-bridge-policy.js` keeps a copy of
//! these lists; change both together.

/// Command prefixes no panel may send, declared or not: settings, pairing,
/// accounts, raw storage, souls, local models and the KB installer.
pub const FORBIDDEN_COMMAND_PREFIXES: &[&str] = &[
    "config.",
    "remote.",
    "account.",
    "content_store.",
    "soul.",
    "local.",
    "kb.wizard.",
];

/// Individual commands no panel may send, declared or not.
pub const FORBIDDEN_COMMANDS: &[&str] = &[
    "mcp.add",
    "mcp.update",
    "mcp.delete",
    "mcp.connect",
    "mcp.disconnect",
    "pack.install",
    "pack.update",
    "pack.uninstall",
    "jev.set",
    "models.download",
    "session.delete",
    "session.clone",
    "file.pick",
    // Returns the asset-plane bearer token and rewrites its CORS origin.
    "status",
    // The caller picks the session, so it could read another session's tabs.
    "tabs.list",
    // Call history.
    "usage.export_calls",
    // Publishes the knowledge base / fetches arbitrary URLs into it.
    "brain.share_create",
    "brain.share_import_url",
    "skill.reload",
];

/// Whether a command is off limits to every panel.
pub fn is_forbidden_command(command: &str) -> bool {
    FORBIDDEN_COMMANDS.contains(&command)
        || FORBIDDEN_COMMAND_PREFIXES
            .iter()
            .any(|p| command.starts_with(p))
}

/// Why a `dashboard.capabilities.commands` entry cannot be granted, or `None`
/// when it is a usable declaration.
pub fn declared_command_problem(command: &str) -> Option<&'static str> {
    if command.is_empty() {
        Some("empty command name")
    } else if command.chars().any(char::is_whitespace) {
        Some("command name contains whitespace")
    } else if is_forbidden_command(command) {
        Some("command is forbidden to every panel")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_prefixes_match_whole_segments_only() {
        assert!(is_forbidden_command("config.llm.set"));
        assert!(is_forbidden_command("remote.agent_set_tier"));
        assert!(is_forbidden_command("account.device_grant_start"));
        assert!(is_forbidden_command("content_store.set"));
        assert!(is_forbidden_command("kb.wizard.install_bun"));
        assert!(!is_forbidden_command("configure"));
        assert!(!is_forbidden_command("accounting.report"));
        assert!(!is_forbidden_command("mcp.list"));
        assert!(!is_forbidden_command("session.list"));
        assert!(!is_forbidden_command("schedule.run_now"));
    }

    #[test]
    fn every_listed_exact_name_is_forbidden_and_only_exactly() {
        for c in FORBIDDEN_COMMANDS {
            assert!(is_forbidden_command(c), "{c}");
        }
        for c in [
            "status",
            "tabs.list",
            "usage.export_calls",
            "brain.share_create",
            "brain.share_import_url",
            "skill.reload",
        ] {
            assert!(is_forbidden_command(c), "{c}");
        }
        // Exact names, not prefixes.
        assert!(!is_forbidden_command("status.extra"));
        assert!(!is_forbidden_command("tabs.list_all"));
        assert!(!is_forbidden_command("brain.share_list"));
        assert!(!is_forbidden_command("skill.list"));
    }

    #[test]
    fn declared_command_problems() {
        assert_eq!(declared_command_problem("schedule.run_now"), None);
        assert!(declared_command_problem("").is_some());
        assert!(declared_command_problem("schedule. run_now").is_some());
        assert!(declared_command_problem(" daemon.info").is_some());
        assert!(declared_command_problem("daemon.info\n").is_some());
        assert!(declared_command_problem("remote.unpair").is_some());
        assert!(declared_command_problem("status").is_some());
    }
}
