"""Integration reproducers for the terminal-less re-check regressions."""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target/debug/dlgt"


class Harness:
    def __init__(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="dlgt-recheck-"))
        self.home = self.root / "home"
        self.home.mkdir()
        self.environment = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "USER": os.environ.get("USER", "test"),
            "TERM": "xterm",
            "DLGT_HOME": str(self.root / "dlgt"),
            "DLGT_CONFIG": str(self.root / "missing-config.toml"),
            "DLGT_CLAUDE_BIN": str(REPO / "tests/fixtures/fake-claude.sh"),
            "DLGT_CODEX_BIN": str(REPO / "tests/fixtures/fake-codex.sh"),
            "TMPDIR": str(self.root),
        }
        self.log = (self.root / "daemon.log").open("w")
        self.server = subprocess.Popen(
            [str(BIN), "server", "--foreground"],
            env=self.environment,
            stdout=self.log,
            stderr=self.log,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            sockets = list((self.root / "dlgt/run").glob("*/dlgt.sock"))
            if sockets:
                self.socket_path = sockets[0]
                break
            if self.server.poll() is not None:
                raise RuntimeError((self.root / "daemon.log").read_text())
            time.sleep(0.01)
        else:
            raise RuntimeError("daemon socket did not appear")

    def pending(self, method: str, params: dict, timeout: float = 45) -> socket.socket:
        connection = socket.socket(socket.AF_UNIX)
        connection.settimeout(timeout)
        connection.connect(str(self.socket_path))
        frame = json.dumps({"id": "recheck", "method": method, "params": params})
        connection.sendall((frame + "\n").encode())
        return connection

    @staticmethod
    def receive(connection: socket.socket) -> dict:
        with connection, connection.makefile("r", encoding="utf-8") as reader:
            return json.loads(reader.readline())

    def rpc(self, method: str, params: dict, timeout: float = 45) -> dict:
        return self.receive(self.pending(method, params, timeout))

    def provider_env(self, extra: dict | None = None) -> dict:
        result = dict(self.environment)
        result.update(extra or {})
        return result

    def create(self, harness: str, prompt: str, extra: dict | None = None, **params) -> dict:
        request = {
            "title": "recheck regression",
            "harness": harness,
            "cwd": str(self.root),
            "environment": self.provider_env(extra),
            "request_id": f"recheck-{time.time_ns()}",
            "prompt": prompt,
            **params,
        }
        return self.rpc("session.create", request)

    def state(self, session: str) -> dict:
        return self.rpc("session.read", {"session": session})["result"]

    def wait_state(self, session: str, expected: str, timeout: float = 5) -> dict:
        deadline = time.monotonic() + timeout
        latest = {}
        while time.monotonic() < deadline:
            latest = self.state(session)
            if latest["session"]["state"] == expected:
                return latest
            time.sleep(0.005)
        raise AssertionError(f"{session} did not reach {expected}: {latest}")

    def stop_provider_pid_file(self, path: Path) -> None:
        if path.exists():
            pid = int(path.read_text().strip())
            for sig in (signal.SIGCONT, signal.SIGKILL):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    break

    def close(self) -> None:
        for path in [*self.root.glob("*.pid"), self.home / ".fake-auth-child-pid", self.home / ".fake-auth-parent-pid"]:
            self.stop_provider_pid_file(path)
        try:
            if self.server.poll() is None:
                try:
                    self.rpc("server.stop", {}, timeout=2)
                except (OSError, TimeoutError, json.JSONDecodeError):
                    pass
                self.server.wait(timeout=3)
        except (subprocess.TimeoutExpired, OSError):
            self.server.kill()
            self.server.wait(timeout=3)
        self.log.close()
        import shutil

        shutil.rmtree(self.root, ignore_errors=True)


class RecheckRegressions(unittest.TestCase):
    def setUp(self) -> None:
        self.harness = Harness()

    def tearDown(self) -> None:
        self.harness.close()

    def test_subscription_auth_checks_the_selected_home_in_both_directions(self) -> None:
        h = self.harness
        selected = h.root / "selected-home"
        selected.mkdir()
        daemon_status = h.home / ".fake-auth-status"
        selected_status = selected / ".fake-auth-status"

        daemon_status.write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        selected_status.write_text('{"loggedIn":false,"authMethod":"none"}')
        rejected = h.create("claude", "selected-home-must-fail", {"HOME": str(selected)})
        if "result" in rejected:
            session = rejected["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.stop", {"session": session, "force": True})

        daemon_status.write_text('{"loggedIn":false,"authMethod":"none"}')
        selected_status.write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        accepted = h.create("claude", "selected-home-must-pass", {"HOME": str(selected)})
        if "result" in accepted:
            session = accepted["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.stop", {"session": session, "force": True})
        self.assertEqual(
            (rejected.get("error", {}).get("code"), "result" in accepted),
            ("AUTH_MISMATCH", True),
            {"selected_home_rejected": rejected, "selected_home_accepted": accepted},
        )

    def test_managed_permission_options_return_invalid_argument_over_rpc(self) -> None:
        h = self.harness
        claude_args = h.root / "claude-args.log"
        codex_args = h.root / "codex-args.log"
        for harness, option in [
            ("claude", "permission-mode=manual"),
            ("codex", 'profiles.wide.approval_policy="never"'),
        ]:
            with self.subTest(harness=harness, option=option):
                response = h.rpc(
                    "session.create",
                    {
                        "title": "invalid option",
                        "harness": harness,
                        "cwd": str(h.root),
                        "environment": h.provider_env(
                            {
                                "DLGT_FAKE_ARGS_FILE": str(claude_args),
                                "DLGT_FAKE_CODEX_LOG": str(codex_args),
                            }
                        ),
                        "request_id": f"invalid-{harness}",
                        "prompt": "should-not-launch",
                        "approvals": "ask",
                        "harness_options": [option],
                    },
                )
                self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
                self.assertIn(option.split("=")[0], response["error"]["message"])
        self.assertFalse(claude_args.exists() and claude_args.stat().st_size)
        self.assertFalse(codex_args.exists() and codex_args.stat().st_size)
        self.assertFalse((h.home / ".claude.json").exists(), "Claude workspace trust ran before validation")
        self.assertFalse((h.home / ".codex" / "config.toml").exists(), "Codex workspace trust ran before validation")

    def test_profile_parser_error_has_path_location_and_no_source_excerpt(self) -> None:
        h = self.harness
        config = h.root / "profiles.toml"
        secret = "profile-config-secret-value"
        config.write_text(f'token = "{secret}"\nthis is not TOML\n')
        environment = dict(h.environment, DLGT_CONFIG=str(config))
        process = subprocess.run(
            [str(BIN), "rpc", "--stdio"],
            input='{"id":"profiles","method":"profile.list"}\n',
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )
        response = json.loads(process.stdout)
        message = response.get("error", {}).get("message", "")

        self.assertIn(str(config), message)
        self.assertIn("2:", message)
        self.assertNotIn(secret, process.stdout + process.stderr)
        self.assertNotIn("this is not TOML", process.stdout + process.stderr)

    def test_codex_ambiguous_write_timeout_fails_closed_for_waiting_duplicate(self) -> None:
        h = self.harness
        pid_file = h.root / "codex.pid"
        log_file = h.root / "codex.log"
        response = h.create(
            "codex",
            "form",
            {"DLGT_FAKE_CODEX_PID_FILE": str(pid_file), "DLGT_FAKE_CODEX_LOG": str(log_file)},
            approvals="ask",
        )
        session = response["result"]["session"]["id"]
        h.wait_state(session, "blocked")
        provider_pid = int(pid_file.read_text().strip())
        os.kill(provider_pid, signal.SIGSTOP)
        answer = {
            "session": session,
            "request": "r1",
            "decision": "json",
            "json": {"name": "Ada", "count": 1, "extra": "x" * 300_000},
        }
        first = h.pending("session.answer", answer)
        time.sleep(0.1)
        duplicate = h.pending("session.answer", answer)
        first_response = h.receive(first)
        state = h.state(session)
        os.kill(provider_pid, signal.SIGCONT)
        duplicate_response = h.receive(duplicate)

        self.assertEqual(first_response.get("error", {}).get("code"), "REQUEST_DELIVERY_FAILED")
        self.assertEqual(state["session"]["state"], "failed", state)
        self.assertEqual(state["requests"][0]["answer_state"], "failed", state)
        self.assertEqual(
            (state.get("result") or {}).get("error", {}).get("code"),
            "REQUEST_DELIVERY_FAILED",
            state,
        )
        self.assertEqual(
            duplicate_response.get("error", {}).get("code"), "REQUEST_DELIVERY_FAILED",
            duplicate_response,
        )
        time.sleep(0.1)
        provider_responses = [
            line for line in log_file.read_text().splitlines()
            if '"id":"srv-1"' in line and '"result"' in line
        ] if log_file.exists() else []
        self.assertEqual(provider_responses, [], "timed-out answer bytes reached the provider")

    def test_restart_drops_old_answer_delivery_failure(self) -> None:
        h = self.harness
        pid_file = h.root / "claude.pid"
        response = h.create(
            "claude", "ask", {"DLGT_FAKE_PID_FILE": str(pid_file)}, approvals="ask"
        )
        session = response["result"]["session"]["id"]
        h.wait_state(session, "blocked")
        os.kill(int(pid_file.read_text().strip()), signal.SIGSTOP)
        answer = h.pending(
            "session.answer",
            {"session": session, "request": "r1", "decision": "deny", "message": "x" * 300_000},
        )
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if h.state(session)["requests"][0]["answer_state"] == "sending":
                break
            time.sleep(0.005)
        restarted = h.rpc(
            "session.restart",
            {
                "session": session,
                "environment": h.provider_env(),
                "startup_timeout_ms": 2_000,
            },
        )
        answer_response = h.receive(answer)
        state = h.state(session)

        self.assertIn("result", restarted, restarted)
        self.assertNotEqual(state["session"]["state"], "failed", state)
        self.assertNotEqual(answer_response.get("error", {}).get("code"), "LAUNCH_FAILED")

    def test_cancel_returns_the_captured_execution_after_a_new_turn_starts(self) -> None:
        h = self.harness
        ready = h.root / "interrupt-result-ready"
        release = h.root / "interrupt-result-release"
        response = h.create(
            "claude",
            "hold-old",
            {
                "DLGT_FAKE_INTERRUPT_RESULT_FILE": str(ready),
                "DLGT_FAKE_INTERRUPT_RELEASE_FILE": str(release),
            },
            approvals="ask",
        )
        session = response["result"]["session"]["id"]
        h.wait_state(session, "busy")
        cancel = h.pending("session.cancel", {"session": session, "timeout_ms": 3_000})
        deadline = time.monotonic() + 3
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.002)
        self.assertTrue(ready.exists(), "fake Claude did not emit the canceled result")
        accepted = None
        for attempt in range(100):
            next_request = h.pending(
                "session.send",
                {"session": session, "request_id": f"next-{attempt}", "prompt": "after-next"},
                timeout=5,
            )
            deadline = time.monotonic() + 0.05
            while time.monotonic() < deadline:
                current = h.state(session)
                if current["session"]["state"] == "busy" and current["execution_seq"] == 2:
                    accepted = current
                    break
                try:
                    next_request.settimeout(0.001)
                    next_request.recv(1, socket.MSG_PEEK)
                    candidate = h.receive(next_request)
                    if "result" in candidate:
                        accepted = candidate
                    break
                except TimeoutError:
                    time.sleep(0.002)
            next_request.close()
            if accepted is not None:
                break
        self.assertIsNotNone(accepted, "the next execution was not accepted in the cancel poll window")
        release.touch()
        h.wait_state(session, "idle")
        canceled = h.receive(cancel)

        self.assertEqual(canceled.get("result", {}).get("result", {}).get("execution_seq"), 1, canceled)

    def test_multiple_queued_claude_steers_are_canceled_before_followup(self) -> None:
        h = self.harness
        response = h.create("claude", "hold-queue", approvals="ask")
        session = response["result"]["session"]["id"]
        h.wait_state(session, "busy")
        for request_id, text in [("queued-one", "queued-first"), ("queued-two", "queued-second")]:
            accepted = h.rpc(
                "session.send",
                {"session": session, "request_id": request_id, "prompt": text, "steer": True},
            )
            self.assertIn("result", accepted, accepted)
        canceled = h.rpc("session.cancel", {"session": session, "timeout_ms": 3_000})
        self.assertEqual(canceled.get("result", {}).get("result", {}).get("status"), "canceled", canceled)
        history = h.rpc("session.fetch", {"session": session, "from_start": True, "messages": 64})
        steer_messages = [
            message
            for bucket in history.get("result", {}).get("sessions", [])
            for message in bucket.get("messages", [])
            if message.get("kind") == "steer"
        ]
        expired = [message for message in steer_messages if message.get("expired")]
        self.assertEqual(len(expired), 2, history)
        self.assertEqual({message["text"] for message in expired}, {"queued-first", "queued-second"})

        h.rpc("session.send", {"session": session, "request_id": "after-queues", "prompt": "after-queues"})
        final = h.wait_state(session, "idle")["result"]
        self.assertEqual(final["execution_seq"], 2, final)
        self.assertEqual(final["final_text"], "fake:after-queues", final)

    def test_auth_status_obeys_startup_deadline_and_server_stop_owns_its_process(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-delay").write_text("20")
        launch = h.pending(
            "session.create",
            {
                "title": "slow auth",
                "harness": "claude",
                "cwd": str(h.root),
                "environment": h.provider_env(),
                "request_id": "slow-auth",
                "prompt": "hello",
                "startup_timeout_ms": 20,
            },
            timeout=5,
        )
        child_file = h.home / ".fake-auth-child-pid"
        deadline = time.monotonic() + 3
        while not child_file.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(child_file.exists(), "auth child did not start")
        launch.settimeout(0.4)
        try:
            h.receive(launch)
            timed_out = False
        except TimeoutError:
            timed_out = True
        h.rpc("server.stop", {}, timeout=3)
        h.server.wait(timeout=3)
        self.assertFalse(timed_out, "auth status outlived the startup deadline")
        for pid_file in (child_file, h.home / ".fake-auth-parent-pid"):
            pid = int(pid_file.read_text().strip())
            state = subprocess.run(
                ["ps", "-o", "stat=", "-p", str(pid)],
                check=False,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertTrue(not state or state.startswith("Z"), f"auth process {pid} survived daemon stop: {state}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
