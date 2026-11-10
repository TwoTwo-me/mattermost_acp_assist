#!/usr/bin/env python3
"""Verify a native release archive without Mattermost or model credentials."""

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import tarfile
import tempfile
import threading
import time

FILES = {
    "mattermost-acp-assist", "README.md", ".env.example", "acp/agents.json",
    "acp/registry.json", "package.json", "package-lock.json",
    "THIRD_PARTY_LICENSES.txt",
}


def request(process, responses, request_id, method, params):
    process.stdin.write(json.dumps({
        "jsonrpc": "2.0", "id": request_id, "method": method, "params": params,
    }) + "\n")
    process.stdin.flush()
    notifications = []
    deadline = time.monotonic() + 30
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"{method} timed out")
        try:
            line = responses.get(timeout=remaining)
        except queue.Empty:
            raise TimeoutError(f"{method} timed out") from None
        if line is None:
            raise RuntimeError(f"agent exited before {method} completed")
        message = json.loads(line)
        if message.get("id") == request_id:
            if "error" in message:
                raise RuntimeError(f"{method} failed: {message['error']}")
            return message["result"], notifications
        if method == "session/prompt":
            notifications.append(message)


def check_agent(command, cwd, environment, prompt=False):
    with tempfile.TemporaryFile(mode="w+") as stderr:
        process = subprocess.Popen(
            command, cwd=cwd, env=environment, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True,
        )
        responses = queue.Queue()

        def receive():
            for line in process.stdout:
                responses.put(line)
            responses.put(None)

        reader = threading.Thread(target=receive, daemon=True)
        reader.start()
        try:
            initialized, _ = request(process, responses, 1, "initialize", {
                "protocolVersion": 1, "clientCapabilities": {},
                "clientInfo": {"name": "release-check", "version": "1"},
            })
            if initialized.get("protocolVersion") != 1:
                raise RuntimeError("agent did not negotiate ACP protocol version 1")
            if prompt:
                session, _ = request(process, responses, 2, "session/new", {
                    "cwd": str(cwd), "mcpServers": [],
                })
                result, updates = request(process, responses, 3, "session/prompt", {
                    "sessionId": session["sessionId"],
                    "prompt": [{"type": "text", "text": "release package check"}],
                })
                chunks = []
                for update in updates:
                    content = update.get("params", {}).get("update", {})
                    if content.get("sessionUpdate") == "agent_message_chunk":
                        chunks.append(content["content"]["text"])
                if result.get("stopReason") != "end_turn" or "".join(chunks) != "STUB:release package check":
                    raise RuntimeError("extracted stub returned an unexpected response")
        except Exception:
            stderr.seek(0)
            print(stderr.read())
            raise
        finally:
            term_sent = False
            for shutdown_signal in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(process.pid, shutdown_signal)
                    if shutdown_signal == signal.SIGTERM:
                        term_sent = True
                except ProcessLookupError:
                    pass
                except PermissionError as error:
                    # XNU can return EPERM for a group containing only an unreaped zombie.
                    if not (
                        sys.platform == "darwin"
                        and error.errno == errno.EPERM
                        and shutdown_signal == signal.SIGKILL
                        and term_sent
                        and not reader.is_alive()
                    ):
                        raise
                if shutdown_signal == signal.SIGTERM:
                    reader.join(timeout=2)
            process.wait(timeout=5)
            reader.join(timeout=5)
            process.stdin.close()
            if reader.is_alive():
                raise RuntimeError("agent stdout remained open after process group shutdown")
            process.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--install-adapters", action="store_true", help="run npm ci and ACP initialize for the packaged adapters")
    args = parser.parse_args()
    archive = args.archive.resolve()
    checksum = archive.with_name(f"{archive.name}.sha256").read_text().split()
    with archive.open("rb") as content:
        actual = hashlib.file_digest(content, "sha256").hexdigest()
    if checksum != [actual, archive.name]:
        raise RuntimeError("archive SHA256 does not match its .sha256 file")
    if not archive.name.endswith(".tar.gz"):
        raise RuntimeError("expected a .tar.gz archive")
    name = archive.name.removesuffix(".tar.gz")
    expected = {name, f"{name}/acp", *(f"{name}/{file}" for file in FILES)}
    with tempfile.TemporaryDirectory(prefix="acp-assist-release-check-") as temporary:
        temporary = Path(temporary).resolve()
        with tarfile.open(archive, "r:gz") as bundle:
            members = bundle.getmembers()
            if len(members) != len(expected) or {entry.name for entry in members} != expected:
                raise RuntimeError("archive contents do not match the runtime allowlist")
            for entry in members:
                directory = entry.name in {name, f"{name}/acp"}
                mode = 0o755 if directory or entry.name == f"{name}/mattermost-acp-assist" else 0o644
                if entry.mode != mode or (directory and not entry.isdir()) or (not directory and not entry.isfile()):
                    raise RuntimeError(f"unexpected member type or permissions: {entry.name}")
            bundle.extractall(temporary, filter="data")
        installation = temporary / name
        notices = (installation / "THIRD_PARTY_LICENSES.txt").read_text(encoding="utf-8")
        for marker in ["Cargo.lock SHA256:", "Rust toolchain runtime notices", "Compiler: rustc ", "COPYRIGHT-library.html"]:
            if marker not in notices:
                raise RuntimeError(f"packaged dependency/runtime notices are missing {marker}")
        home = temporary / "home"
        scratch = temporary / "tmp"
        workspace = temporary / "workspace"
        for directory in [home, home / "codex", home / "config", home / "cache", scratch, workspace]:
            directory.mkdir()
        environment = {
            "PATH": os.environ.get("PATH", os.defpath), "HOME": str(home),
            "TMPDIR": str(scratch), "TMP": str(scratch), "TEMP": str(scratch),
            "XDG_CONFIG_HOME": str(home / "config"), "XDG_CACHE_HOME": str(home / "cache"),
            "CODEX_HOME": str(home / "codex"),
        }
        binary = installation / "mattermost-acp-assist"
        cleaned = subprocess.run(
            [str(binary), "cleanup-scratch", "--dry-run"], cwd=workspace,
            env=environment, check=True, capture_output=True, text=True, timeout=15,
        )
        if "removed=0 would_remove=0 active_or_uncertain=0 unverified=0 errors=0" not in cleaned.stdout:
            raise RuntimeError("extracted cleanup command did not report an empty isolated root")
        check_agent([str(binary), "stub-agent"], workspace, environment, prompt=True)
        if args.install_adapters:
            subprocess.run(
                ["npm", "ci", "--omit=dev", "--no-audit", "--no-fund"],
                cwd=installation, env=environment, check=True, timeout=300,
            )
            agents = json.loads((installation / "acp/agents.json").read_text())
            for name, spec in agents.items():
                if name == "stub":
                    continue
                command = list(spec["command"])
                if "/" in command[0] and not Path(command[0]).is_absolute():
                    command[0] = str(installation / command[0])
                agent_environment = environment.copy()
                for key, value in spec.get("env", {}).items():
                    if value is None:
                        agent_environment.pop(key, None)
                    else:
                        agent_environment[key] = value
                check_agent(command, workspace, agent_environment)
        print(f"PASS {archive.name}: checksum, allowlist, modes, isolated cleanup, embedded ACP stub")
        if args.install_adapters:
            print("PASS installed adapter ACP initialize; no model prompt sent")


if __name__ == "__main__":
    main()
