/// Create a `tokio::process::Command` that correctly resolves CLI tools on all
/// platforms.
///
/// On Windows, npm/node-installed CLIs are `.ps1`/`.cmd` scripts. Rust's
/// `Command::new("name")` only finds `.exe` files, so we wrap the call with
/// `cmd.exe /C` to let Windows resolve the script via `PATHEXT`.
pub(crate) fn cli_command(program: &str) -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(program);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        tokio::process::Command::new(program)
    }
}

/// Synchronous version of [`cli_command`].
pub(crate) fn cli_command_sync(program: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args(["/C", program]);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(program)
    }
}
