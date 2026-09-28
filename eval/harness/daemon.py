"""Start, drive and kill one headless nevoflux daemon for one trial."""

import json
import os
import pathlib
import re
import socket
import subprocess
import time
import urllib.error
import urllib.request

TERMINAL = {"succeeded", "failed", "canceled"}


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


_LOADED = re.compile(r'Loaded agent config: llm\.provider=Some\("([^"]*)"\)')
_SERVICES = re.compile(r"LLM config set on services: provider=[^,]*, model=(\S+)")
_LOAD_FAILED = re.compile(r"Failed to load agent config: (.*)")


def parse_effective_config(log_text: str) -> dict:
    """What the daemon actually loaded, read from its log: an override that
    did not take would otherwise run the trial on the wrong provider silently."""
    strip = re.sub(r"\x1b\[[0-9;]*m", "", log_text)
    loaded, services, failed = _LOADED.search(strip), _SERVICES.search(strip), _LOAD_FAILED.search(strip)
    return {"provider": loaded.group(1) if loaded else None,
            "model": services.group(1) if services else None,
            "load_error": failed.group(1).strip() if failed else None}


def poll(get, timeout_secs, interval=2.0, clock=time.monotonic, sleep=time.sleep):
    deadline = clock() + timeout_secs
    last = {}
    while True:
        last = get()
        if last.get("status") in TERMINAL:
            return last, False
        if clock() >= deadline:
            return last, True
        sleep(interval)


class Daemon:
    def __init__(self, agent_exe, trial_dir, browser_bin, port):
        self.exe = pathlib.Path(agent_exe)
        self.dir = pathlib.Path(trial_dir)
        self.port = port
        self.base = f"http://127.0.0.1:{port}"
        self.env = {
            **os.environ,
            "NEVOFLUX_DATA_DIR": str(self.dir / "data"),
            "NEVOFLUX_CONFIG": str(self.dir / "config.toml"),
            "NEVOFLUX_BROWSER_BIN": str(browser_bin),
            "NEVOFLUX_BASE_PROFILES": str(self.dir / "base-profiles"),
            "NEVOFLUX_PROFILE_WORK": str(self.dir / "profiles"),
            "PYTHONIOENCODING": "utf-8",
        }
        self.proc = None
        self._log = None

    def _req(self, method, path, body=None, timeout=30):
        data = json.dumps(body, ensure_ascii=False).encode("utf-8") if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return json.loads(raw) if raw else {}

    def start(self, ready_timeout=60):
        for sub in ("data", "base-profiles", "profiles"):
            (self.dir / sub).mkdir(parents=True, exist_ok=True)
        self._log = open(self.dir / "daemon.stdout.log", "wb")
        # Without --port the daemon binds the fixed 19500 the desktop NevoFlux
        # uses; the proxy finds whichever port via daemon.port in the data dir.
        self.internal_port = free_port()
        self.proc = subprocess.Popen(
            [str(self.exe), "--daemon", "--headless", "--http-addr", f"127.0.0.1:{self.port}",
             "--port", str(self.internal_port)],
            env=self.env, stdout=self._log, stderr=subprocess.STDOUT, cwd=str(self.dir),
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0))
        deadline = time.monotonic() + ready_timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"daemon exited with {self.proc.returncode}; see {self.dir}")
            try:
                urllib.request.urlopen(self.base + "/metrics", timeout=2).read()
                return
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                time.sleep(0.5)
        raise RuntimeError(f"daemon not ready in {ready_timeout}s; see {self.dir}")

    def effective_config(self, wait_secs=10) -> dict:
        """Parse the loaded provider/model from the log (it lands shortly after start)."""
        deadline = time.monotonic() + wait_secs
        while True:
            self._log.flush()
            eff = parse_effective_config(
                (self.dir / "daemon.stdout.log").read_text(encoding="utf-8", errors="replace"))
            if eff["load_error"] or (eff["provider"] and eff["model"]) or time.monotonic() > deadline:
                return eff
            time.sleep(0.5)

    def submit(self, body) -> str:
        return self._req("POST", "/tasks", body)["id"]

    def get(self, task_id) -> dict:
        return self._req("GET", f"/tasks/{task_id}")

    def cancel(self, task_id) -> None:
        try:
            self._req("DELETE", f"/tasks/{task_id}")
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass

    def export_session(self, session_id, out) -> bool:
        r = subprocess.run([str(self.exe), "session", "export", session_id, "--out", str(out)],
                           env=self.env, capture_output=True, timeout=120)
        return r.returncode == 0 and pathlib.Path(out).exists()

    def stop(self):
        if self.proc and self.proc.poll() is None:
            subprocess.run(["taskkill", "/PID", str(self.proc.pid), "/T", "/F"],
                           capture_output=True)
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                pass
        if self._log:
            self._log.close()
            self._log = None
        # The browser launcher re-parents the real browser; kill whatever still
        # references this trial's directory on its command line — except this
        # PowerShell itself, whose own command line contains the path too.
        ps = ("Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $PID -and "
              "$_.CommandLine -like '*" + str(self.dir).replace("'", "''") + "*' } | "
              "ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }")
        subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True)
