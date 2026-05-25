# PowerShell Script Guidelines (for `run_command` / daemon contexts)

This document captures hard-won lessons from debugging a real `download_subtitles.ps1` that worked perfectly in a PowerShell terminal but hung for 10 minutes (with zero output) when invoked through nevoflux-agent's `run_command` tool. Use this as a checklist whenever you author or generate a `.ps1` script that may be invoked by a daemon, an agent, or any non-interactive parent.

## TL;DR

> A PowerShell script running inside a daemon is **not** the same environment as one running in your terminal. Working in a terminal is **necessary but not sufficient** — every script must be validated end-to-end through `run_command` (or whatever non-interactive parent will spawn it in production).

| Terminal | Daemon (`run_command`) |
| --- | --- |
| TTY present | No TTY |
| `stdin` is interactive | `stdin` is a pipe inherited from the daemon, never EOFs |
| Console window allocated | No console; some Win32 APIs behave differently |
| Python defaults to line-buffered stdout | Python defaults to 4 KB block-buffered stdout |
| `PATH` is your interactive shell's full `PATH` | `PATH` is whatever the daemon inherited at startup |
| You see process exit promptly | Pipe-handle inheritance can keep the parent blocked long after the child exits |

## 1. The Five Iron Rules

| # | Rule | Why |
| --- | --- | --- |
| 1 | Use `$env:VAR`, never `%VAR%` | PowerShell does not expand cmd.exe-style variables. `%APPDATA%` becomes a literal string (or worse: PowerShell tries to load it as a module). |
| 2 | Set `$env:PYTHONUNBUFFERED = "1"` before invoking any Python tool | Without a TTY, Python flips stdout to 4 KB block-buffered mode. All output stays inside Python's buffer until the process exits — looks exactly like a hang. |
| 3 | Redirect native command output through **cmd.exe's `>`**, not PowerShell's `>` / `*>` / `Start-Process -Redirect*` | PowerShell's "redirection" is implemented internally as a pipe plus a background drain task. Grandchildren (e.g. `deno`) inherit the pipe's write end; until they all exit, the pipe never EOFs and the parent hangs. cmd.exe's `>` is real Win32 handle replacement (`CreateFile` + `StartupInfo.hStdOutput`). |
| 4 | Feed `<nul` to every child's `stdin` | The child inherits a pipe to the daemon's stdin which never closes. Any `sys.stdin.read()` (or accidental TTY check) will block forever. |
| 5 | Always end with `exit 0` on success and `exit 1` on every error path | The parent agent uses the exit code to decide success vs. failure. Falling off the end of a script can yield ambiguous codes in some hosts. |

## 2. Script Header Template

Paste this at the top of every script:

```powershell
param (
    [Parameter(Mandatory=$true, Position=0)]
    [string]$inputArg
)

$ErrorActionPreference = "Stop"
$env:PYTHONUNBUFFERED  = "1"
$env:PYTHONIOENCODING  = "utf-8"
```

Do **not** rely on `$PSNativeCommandUseErrorActionPreference = $false` — that variable was introduced in PowerShell 7.3+. nevoflux's `run_command` on Windows invokes `powershell.exe` (i.e. Windows PowerShell 5.1), where setting that variable is a no-op.

## 3. The Standard Native-Command Wrapper

Use this helper for every `yt-dlp`, `python`, `ffmpeg`, etc. call. Do not invoke them directly with PowerShell's `>` operator.

```powershell
function Invoke-ViaCmd {
    param([string]$CmdLine)
    $batFile = Join-Path $env:TEMP ("nvfx_run_" + [guid]::NewGuid().ToString("N") + ".bat")
    "@echo off`r`n$CmdLine`r`nexit /b %ERRORLEVEL%`r`n" |
        Set-Content -Path $batFile -Encoding ASCII -NoNewline
    try {
        & cmd.exe /c $batFile
        return $LASTEXITCODE
    }
    finally {
        Remove-Item $batFile -Force -ErrorAction SilentlyContinue
    }
}
```

Why a `.bat` file rather than a direct `cmd /c "..."` string?

PowerShell 5.1's native-command argument quoting is unpredictable when the string contains embedded `"`, backslashes, and redirection operators. Writing the exact command into a `.bat` file sidesteps every PowerShell quoting bug — cmd.exe reads the file directly, and its parser handles the quotes correctly.

### Calling pattern

```powershell
$cmdLine = 'some-tool "' + $arg1 + '" "' + $arg2 + '"' + `
    ' <nul' + `                       # stdin must be nul
    ' 1>"' + $stdoutFile + '"' + `    # real Win32 redirect
    ' 2>"' + $stderrFile + '"'        # real Win32 redirect

$exit = Invoke-ViaCmd -CmdLine $cmdLine

if (Test-Path $stdoutFile) { Write-Host (Get-Content $stdoutFile -Raw).TrimEnd() }
if (Test-Path $stderrFile) { Write-Host (Get-Content $stderrFile -Raw).TrimEnd() }
Write-Host "exit code: $exit"
```

### `.bat` file gotchas

- `%` must be written as `%%`. cmd.exe halves it when interpreting the batch file. So `subtitle.%(ext)s` (a yt-dlp template) becomes `subtitle.%%(ext)s` in the PowerShell string.
- `&`, `|`, `<`, `>`, `^`, `(`, `)` inside argument values must be wrapped in `"..."` so cmd.exe does not treat them as operators.
- Always wrap URLs in `"..."` — YouTube URLs often contain `&` (e.g. `&t=756s`) which would otherwise be parsed as a command separator.

## 4. Anti-Patterns

| Wrong | Why it fails | Correct |
| --- | --- | --- |
| `cmd 2>&1 \| Out-String` | With `$ErrorActionPreference = "Stop"`, the first WARNING line from the native command is wrapped as an `ErrorRecord` in the pipeline and immediately throws. The child process is killed mid-execution. | `cmd <nul 1>"out" 2>"err"` via `Invoke-ViaCmd` |
| `Start-Process -RedirectStandardOutput "file" -Wait` | Internally uses a pipe + `StreamReader.CopyTo(FileStream)`. Same grandchild-holds-pipe hang as `>`. | `Invoke-ViaCmd` |
| `yt-dlp ... > $logFile` | PowerShell's `>` for native commands is a pipe + drain, not a file handle. | cmd.exe's `>` inside the `.bat` |
| `$PSNativeCommandUseErrorActionPreference = $false` | Does not exist in PowerShell 5.1, which is what the daemon invokes. | Don't depend on PS7+ features |
| `if (Test-Path $output) { "success" }` | A stale file from a previous run will be detected as "success". | `Remove-Item $output -Force -EA SilentlyContinue` before invoking the tool |
| Running the same tool twice for two variants | Doubles network requests, doubles JS-challenge solver time, doubles 429 risk. | Merge flags into one invocation (e.g. yt-dlp `--write-subs --write-auto-subs`) |

## 5. Bounding Network-Tool Time

For any network-dependent tool, add aggressive timeouts so a slow proxy or transient block does not push total runtime past the parent's timeout:

```text
--socket-timeout 10    # cap each connection at 10s
--retries 1            # at most one retry
--no-update            # skip self-update probes
--no-progress          # don't waste pipe bandwidth on progress bars
```

Without these, yt-dlp's default `--retries 10` with no socket timeout can turn one bad request into a 5-minute hang.

## 6. Debugging Recipes (when something still hangs)

1. **Add timestamps at every significant step.**
   ```powershell
   Write-Host "[$(Get-Date -Format 'HH:mm:ss.fff')] >>> yt-dlp"
   $exit = Invoke-ViaCmd -CmdLine $cmdLine
   Write-Host "[$(Get-Date -Format 'HH:mm:ss.fff')] <<< yt-dlp returned exit=$exit"
   ```
   Only with timestamps can you distinguish "slow step" from "infinite hang".

2. **Always write the child's stdout/stderr to disk**, even when you also `Write-Host` it. If `run_command` times out, the file remains on disk and you can read it from a separate call.

3. **After a timeout, issue a second `run_command` to inspect state**:
   ```powershell
   Write-Host "=== output file ==="
   if (Test-Path "$env:TEMP\subtitle\subtitle.txt") {
       $info = Get-Item "$env:TEMP\subtitle\subtitle.txt"
       Write-Host "EXISTS size=$($info.Length) modified=$($info.LastWriteTime)"
   } else { Write-Host "NOT FOUND" }

   Write-Host "=== stderr ==="
   if (Test-Path "$env:TEMP\subtitle\py.err") {
       Get-Content "$env:TEMP\subtitle\py.err" -Raw
   }

   Write-Host "=== orphan processes ==="
   Get-Process python, cmd, yt-dlp, deno -ErrorAction SilentlyContinue |
       Select-Object Name, Id, StartTime
   ```
   This tells you whether the job actually finished (file exists, no orphans) or whether something is stuck (orphans present).

## 7. Pre-Flight Checklist

Before shipping a `.ps1` that will be invoked by `run_command`:

- [ ] `$ErrorActionPreference = "Stop"` and `PYTHONUNBUFFERED = "1"` at the top
- [ ] Every native command goes through `Invoke-ViaCmd` with `<nul` + `1>file 2>file`
- [ ] Stale output files are deleted at the start of each run
- [ ] Success is checked by **both** `$LASTEXITCODE -eq 0` **and** `Test-Path $expectedFile`
- [ ] Every error branch has an explicit `exit 1`; the success path ends with `exit 0`
- [ ] All paths and URLs in command lines are wrapped in `"..."`
- [ ] Network tools have `--socket-timeout` and `--retries` set
- [ ] Script tested through `run_command`, not just in a terminal

## 8. Background: Why the Daemon Side Behaves This Way

`run_command` is implemented in `crates/daemon/src/agent/tools.rs`. On Windows it spawns:

```text
powershell -NoProfile -NonInteractive -Command <your-command>
```

via `tokio::process::Command::output()` with a 120-second timeout. Two things follow from this:

1. **The child process is not killed on timeout.** `tokio::process::Child` defaults to `kill_on_drop = false`. When the future is cancelled by `tokio::time::timeout`, the Child handle is dropped but the underlying Windows process continues running until it exits on its own. The daemon already returned `"Command timed out after 120 seconds"` to the agent, even though the script may finish a few seconds later and write its output file. From the agent's perspective the operation failed.

2. **The daemon awaits stdout/stderr EOF, not just process exit.** `cmd.output()` reads both pipes to completion. If any descendant of the script (a deno solver, a python helper, an ffmpeg subprocess) inherits the pipe's write end and keeps it open, those pipes never EOF. tokio waits 120 seconds and times out — even though the script's "real work" finished long ago.

Both behaviours are what the rules in section 1 are designed to dodge. By the time output reaches the daemon, every child must have already released every inherited pipe handle; the cleanest way to ensure that is to never let the child see a daemon pipe in the first place, which is exactly what cmd.exe's `>` redirection achieves.
