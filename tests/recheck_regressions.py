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
    def __init__(self, daemon_env: dict | None = None) -> None:
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
        self.daemon_env = dict(self.environment, **(daemon_env or {}))
        self.log = (self.root / "daemon.log").open("w")
        self.server = subprocess.Popen(
            [str(BIN), "server", "--foreground"],
            env=self.daemon_env,
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
        for path in [
            *self.root.glob("*.pid"),
            self.home / ".fake-auth-child-pid",
            self.home / ".fake-auth-parent-pid",
            self.home / ".fake-auth-holder-pid",
            self.home / ".fake-auth-linger-pid",
        ]:
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
        escaped = self.home / ".fake-auth-escape-pids"
        if escaped.exists():
            for line in escaped.read_text().split():
                try:
                    os.kill(int(line), signal.SIGKILL)
                except (ProcessLookupError, ValueError):
                    pass
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

    def test_managed_permission_option_returns_invalid_argument_from_cli(self) -> None:
        h = self.harness
        args_file = h.root / "claude-cli-args.log"
        process = subprocess.run(
            [
                str(BIN),
                "new",
                "--title",
                "invalid CLI option",
                "--harness",
                "claude",
                "--cwd",
                str(h.root),
                "--request-id",
                "invalid-cli-option",
                "--approvals",
                "ask",
                "--harness-option",
                "permission-mode=manual",
                "--",
                "must not launch",
            ],
            env=h.provider_env({"DLGT_FAKE_ARGS_FILE": str(args_file)}),
            check=False,
            capture_output=True,
            text=True,
        )
        response = json.loads(process.stdout)
        self.assertNotEqual(process.returncode, 0, response)
        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        self.assertIn("permission-mode", response["error"]["message"])
        self.assertFalse(args_file.exists() and args_file.stat().st_size)

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

    def test_provider_config_error_has_path_location_and_no_source_excerpt(self) -> None:
        h = self.harness
        codex_home = h.home / ".codex"
        codex_home.mkdir()
        config = codex_home / "config.toml"
        secret = "provider-config-secret-value"
        config.write_text(f'token = "{secret}"\nthis is not TOML\n')

        response = h.create("codex", "provider config must fail before launch")
        message = response.get("error", {}).get("message", "")

        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        self.assertIn(str(config), message)
        self.assertIn("2:", message)
        self.assertNotIn(secret, json.dumps(response))
        self.assertNotIn("this is not TOML", json.dumps(response))

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
            "json": {"name": "A" * 300_000, "count": 1},
        }
        first = h.pending("session.answer", answer)
        time.sleep(0.1)
        duplicate = h.pending("session.answer", answer)
        first_response = h.receive(first)
        state = h.state(session)
        try:
            os.kill(provider_pid, signal.SIGCONT)
        except ProcessLookupError:
            pass
        duplicate_response = h.receive(duplicate)

        self.assertEqual(first_response.get("error", {}).get("code"), "REQUEST_DELIVERY_FAILED")
        self.assertEqual(state["session"]["state"], "failed", state)
        self.assertEqual(state["requests"], [], "failed Requests are no longer answerable")
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
        self.assertFalse(
            any(request["answer_state"] == "sending" for request in state["requests"]),
            "the withdrawn old-provider attempt must settle without affecting the replacement Session",
        )

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

    def test_a_steer_racing_claude_cancel_is_refused_or_canceled(self) -> None:
        self.harness.close()
        self.harness = Harness({"DLGT_TEST_PAUSE_AFTER_CANCEL_SNAPSHOT_MS": "400"})
        h = self.harness
        session = h.create("claude", "hold-queue", approvals="ask")["result"]["session"]["id"]
        h.wait_state(session, "busy")
        cancel = h.pending("session.cancel", {"session": session, "timeout_ms": 3_000})
        time.sleep(0.1)
        steer = h.rpc(
            "session.send",
            {"session": session, "request_id": "racing-steer", "prompt": "racing", "steer": True},
        )
        canceled = h.receive(cancel)
        self.assertEqual(canceled.get("result", {}).get("result", {}).get("status"), "canceled", canceled)
        if "result" in steer:
            history = h.rpc("session.fetch", {"session": session, "from_start": True, "messages": 64})
            racing = [
                message
                for bucket in history["result"]["sessions"]
                for message in bucket.get("messages", [])
                if message.get("kind") == "steer" and message.get("text") == "racing"
            ]
            self.assertTrue(racing and racing[-1].get("expired"), history)
        else:
            self.assertEqual(steer["error"]["code"], "NO_ACTIVE_EXECUTION", steer)

    def test_a_claude_prompt_canceled_at_submission_never_runs(self) -> None:
        self.harness.close()
        self.harness = Harness({"DLGT_TEST_PAUSE_BEFORE_CLAUDE_RESERVATION_MS": "400"})
        h = self.harness
        session = h.create("claude", "first")["result"]["session"]["id"]
        h.wait_state(session, "idle", timeout=10)
        send = h.pending("session.send", {"session": session, "request_id": "second", "prompt": "second"})
        time.sleep(0.1)
        canceled = h.rpc("session.cancel", {"session": session, "timeout_ms": 3_000})
        h.receive(send)
        self.assertEqual(canceled.get("result", {}).get("result", {}).get("status"), "canceled", canceled)
        h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "third", "prompt": "third"})
        final = h.wait_state(session, "idle", timeout=10)["result"]
        self.assertEqual((final["execution_seq"], final["final_text"]), (3, "fake:third"), final)
        history = h.rpc("session.fetch", {"session": session, "from_start": True, "messages": 64})
        for bucket in history["result"]["sessions"]:
            for message in bucket.get("messages", []):
                if message.get("text") == "fake:second":
                    self.assertEqual(message.get("execution_seq"), 2, history)

    def test_a_duplicate_answer_during_delivery_replays_the_delivered_answer(self) -> None:
        self.harness.close()
        # The provider finishes while the receipt is held, so only dlgt's own
        # completion notification can wake the waiting duplicate.
        self.harness = Harness(
            {
                "DLGT_TEST_PAUSE_BEFORE_ANSWER_DELIVERY_MS": "400",
                "DLGT_TEST_PAUSE_BEFORE_ANSWER_RECEIPT_MS": "1000",
            }
        )
        h = self.harness
        session = h.create("claude", "ask", approvals="ask")["result"]["session"]["id"]
        h.wait_state(session, "blocked")
        answer = {"session": session, "request": "r1", "decision": "allow"}
        first = h.pending("session.answer", answer)
        time.sleep(0.1)
        duplicate_started = time.monotonic()
        duplicate = h.rpc("session.answer", answer)
        self.assertLess(time.monotonic() - duplicate_started, 4, "duplicate missed the delivery notification")
        first = h.receive(first)
        self.assertIn("result", first, first)
        self.assertTrue(duplicate.get("result", {}).get("replayed"), duplicate)
        self.assertEqual(h.wait_state(session, "idle")["result"]["final_text"], "allowed")

    def test_a_late_denial_receipt_counts_against_its_own_execution(self) -> None:
        self.harness.close()
        self.harness = Harness({"DLGT_TEST_PAUSE_BEFORE_ANSWER_RECEIPT_MS": "600"})
        h = self.harness
        session = h.create("claude", "ask", approvals="ask")["result"]["session"]["id"]
        h.wait_state(session, "blocked")
        answer = h.pending("session.answer", {"session": session, "request": "r1", "decision": "deny"})
        h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "next", "prompt": "hold-next"})
        h.wait_state(session, "busy")
        self.assertIn("result", h.receive(answer))
        history = h.rpc("session.fetch", {"session": session, "from_start": True, "messages": 64})
        approvals = [
            message
            for bucket in history["result"]["sessions"]
            for message in bucket.get("messages", [])
            if message.get("kind") == "approval"
        ]
        self.assertEqual([message.get("execution_seq") for message in approvals], [1], history)
        first = [
            result
            for bucket in history["result"]["sessions"]
            for result in bucket.get("results", [])
            if result.get("execution_seq") == 1
        ]
        self.assertEqual([result.get("denied_requests") for result in first], [1], history)
        h.rpc("session.cancel", {"session": session, "timeout_ms": 3_000})
        latest = h.wait_state(session, "idle")["result"]
        self.assertEqual((latest["execution_seq"], latest["denied_requests"]), (2, 0), latest)

    def test_cancel_settles_when_the_provider_exits_mid_cancel(self) -> None:
        h = self.harness
        pid_file = h.root / "claude.pid"
        session = h.create(
            "claude",
            "hold-exit",
            {"DLGT_FAKE_INTERRUPT_DELAY": "20", "DLGT_FAKE_PID_FILE": str(pid_file)},
        )["result"]["session"]["id"]
        h.wait_state(session, "busy")
        cancel = h.pending("session.cancel", {"session": session, "timeout_ms": 5_000})
        time.sleep(0.3)
        os.killpg(os.getpgid(int(pid_file.read_text())), signal.SIGKILL)
        started = time.monotonic()
        canceled = h.receive(cancel)
        self.assertLess(time.monotonic() - started, 4, canceled)
        self.assertIn("result", canceled, canceled)
        self.assertEqual(canceled["result"]["session"]["state"], "failed", canceled)

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
                "startup_timeout_ms": 1000,
            },
            timeout=5,
        )
        child_file = h.home / ".fake-auth-child-pid"
        parent_file = h.home / ".fake-auth-parent-pid"
        deadline = time.monotonic() + 3
        def written(path: Path) -> bool:
            return path.exists() and path.read_text().strip() != ""

        while not (written(child_file) and written(parent_file)) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(child_file.exists(), "auth child did not start")
        first_pids = [int(child_file.read_text()), int(parent_file.read_text())]
        launch.settimeout(3)
        timeout_response = h.receive(launch)
        self.assertEqual(timeout_response.get("error", {}).get("code"), "LAUNCH_FAILED", timeout_response)
        for pid in first_pids:
            state = subprocess.run(
                ["ps", "-o", "stat=", "-p", str(pid)],
                check=False,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertTrue(not state or state.startswith("Z"), f"auth process {pid} survived startup timeout: {state}")

        child_file.unlink()
        parent_file.unlink()
        second_launch = h.pending(
            "session.create",
            {
                "title": "auth stopped with daemon",
                "harness": "claude",
                "cwd": str(h.root),
                "environment": h.provider_env(),
                "request_id": "slow-auth-stop",
                "prompt": "hello",
                "startup_timeout_ms": 10_000,
            },
            timeout=5,
        )
        deadline = time.monotonic() + 3
        while not (written(child_file) and written(parent_file)) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(child_file.exists(), "second auth child did not start")
        second_pids = [int(child_file.read_text()), int(parent_file.read_text())]
        h.rpc("server.stop", {}, timeout=3)
        h.server.wait(timeout=3)
        second_launch.close()
        for pid in second_pids:
            state = subprocess.run(
                ["ps", "-o", "stat=", "-p", str(pid)],
                check=False,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertTrue(not state or state.startswith("Z"), f"auth process {pid} survived daemon stop: {state}")

    def test_auth_status_retains_only_bounded_output(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-output-bytes").write_text("131072")

        response = h.create("claude", "oversized auth status output")
        if "result" in response:
            session = response["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.stop", {"session": session, "force": True})

        self.assertEqual(response.get("error", {}).get("code"), "AUTH_MISMATCH", response)

    def test_auth_status_rejects_a_valid_prefix_followed_by_discarded_bytes(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-trailing-bytes").write_text("70000")
        response = h.create("claude", "valid prefix then garbage")
        self.assertEqual(response.get("error", {}).get("code"), "AUTH_MISMATCH", response)

    def test_auth_status_deadline_covers_a_descendant_holding_stdout(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-hold-stdout").write_text("20")
        started = time.monotonic()
        response = h.create("claude", "held stdout", startup_timeout_ms=1000)
        elapsed = time.monotonic() - started
        self.assertEqual(response.get("error", {}).get("code"), "LAUNCH_FAILED", response)
        self.assertLess(elapsed, 3, elapsed)
        holder = int((h.home / ".fake-auth-holder-pid").read_text())
        time.sleep(0.2)
        state = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(holder)], check=False, capture_output=True, text=True
        ).stdout.strip()
        self.assertTrue(not state or state.startswith("Z"), f"stdout holder survived: {state}")

    def test_a_successful_auth_check_leaves_no_process_in_its_group(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-linger").write_text("")
        response = h.create("claude", "lingering auth child")
        self.assertIn("result", response, response)
        lingering = int((h.home / ".fake-auth-linger-pid").read_text())
        time.sleep(0.2)
        state = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(lingering)], check=False, capture_output=True, text=True
        ).stdout.strip()
        self.assertTrue(not state or state.startswith("Z"), f"auth group member survived: {state}")

    def test_auth_readers_end_when_an_escaped_process_holds_the_pipes(self) -> None:
        h = self.harness
        (h.home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (h.home / ".fake-auth-escape").write_text("")

        def threads() -> int:
            task_dir = Path(f"/proc/{h.server.pid}/task")
            if task_dir.exists():
                return len(list(task_dir.iterdir()))
            listing = subprocess.run(
                ["ps", "-M", "-p", str(h.server.pid)], check=False, capture_output=True, text=True
            ).stdout
            return len(listing.splitlines()) - 1

        baseline = threads()
        for attempt in range(3):
            response = h.create("claude", f"escaped {attempt}", startup_timeout_ms=500)
            self.assertEqual(response.get("error", {}).get("code"), "LAUNCH_FAILED", response)
        time.sleep(0.3)
        self.assertLessEqual(threads(), baseline + 1, "auth reader threads leaked")

    def test_oversized_session_metadata_is_rejected_at_the_rpc_boundary(self) -> None:
        h = self.harness
        created = h.create("claude", "big title", title="t" * 300_000)
        self.assertEqual(created.get("error", {}).get("code"), "INVALID_ARGUMENT", str(created)[:300])
        resumed = h.rpc(
            "session.send",
            {
                "session": "claude:" + "a" * 280_000,
                "resume": True,
                "request_id": "huge-resume",
                "prompt": "hello",
                "cwd": str(h.root),
                "environment": h.provider_env(),
            },
        )
        self.assertEqual(resumed.get("error", {}).get("code"), "INVALID_ARGUMENT", str(resumed)[:300])

    def test_native_config_errors_are_invalid_arguments_at_their_own_location(self) -> None:
        h = self.harness
        claude_state = h.home / ".claude.json"
        claude_state.write_text('{\n  "nested": {"projects": {}},\n  "projects": 3\n}\n')
        response = h.create("claude", "bad claude projects")
        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        self.assertIn(f"{claude_state}:3:", response["error"]["message"])
        claude_state.write_text("{not json")
        response = h.create("claude", "bad claude json")
        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        claude_state.unlink()

        codex_home = h.home / ".codex"
        codex_home.mkdir()
        config = codex_home / "config.toml"
        config.write_text('model = "m"\n\n# note\n\nprojects = 3\n')
        response = h.create("codex", "bad codex projects")
        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        self.assertIn(f"{config}:5:", response["error"]["message"])

    def test_a_codex_transport_lost_during_turn_start_is_session_unavailable(self) -> None:
        h = self.harness
        pid_file = h.root / "codex.pid"
        session = h.create("codex", "first", {"DLGT_FAKE_CODEX_PID_FILE": str(pid_file)})["result"]["session"]["id"]
        h.wait_state(session, "idle")
        provider = int(pid_file.read_text())
        os.kill(provider, signal.SIGSTOP)
        send = h.pending("session.send", {"session": session, "request_id": "lost", "prompt": "second"})
        time.sleep(0.3)
        os.killpg(os.getpgid(provider), signal.SIGKILL)
        response = h.receive(send)
        self.assertEqual(response.get("error", {}).get("code"), "SESSION_UNAVAILABLE", response)

    def test_unavailable_codex_discovery_is_a_launch_failure(self) -> None:
        self.harness.close()
        self.harness = Harness({"DLGT_CODEX_BIN": "/nonexistent/codex"})
        response = self.harness.rpc("model.list", {"harness": "codex"})
        self.assertEqual(response.get("error", {}).get("code"), "LAUNCH_FAILED", response)

    def test_stdio_profile_list_keeps_a_config_error_code(self) -> None:
        h = self.harness
        config = h.root / "bad-config.toml"
        config.write_text("[profiles\n")
        proxy = subprocess.run(
            [str(BIN), "rpc", "--stdio"],
            input=json.dumps({"id": "p", "method": "profile.list", "params": {}}) + "\n",
            env=dict(h.environment, DLGT_CONFIG=str(config)),
            capture_output=True,
            text=True,
            timeout=20,
        )
        line = next(line for line in proxy.stdout.splitlines() if line.strip())
        self.assertEqual(json.loads(line).get("error", {}).get("code"), "INVALID_ARGUMENT", proxy.stdout)

    def test_codex_cancel_settles_when_the_terminal_event_precedes_the_interrupt_reply(self) -> None:
        h = self.harness
        session = h.create("codex", "hold-order", {"DLGT_FAKE_INTERRUPT_REPLY_LAST": "1"})["result"]["session"]["id"]
        h.wait_state(session, "busy")
        canceled = h.rpc("session.cancel", {"session": session, "timeout_ms": 3_000})
        self.assertEqual(canceled.get("result", {}).get("result", {}).get("status"), "canceled", canceled)
        h.wait_state(session, "idle")
        followup = h.rpc("session.send", {"session": session, "request_id": "after-order", "prompt": "next"})
        self.assertIn("result", followup, followup)

    def test_a_send_racing_restart_never_strands_the_replacement(self) -> None:
        for harness in ("claude", "codex"):
            self.harness.close()
            self.harness = Harness({"DLGT_TEST_PAUSE_BEFORE_SUBMIT_RESERVATION_MS": "800"})
            h = self.harness
            session = h.create(harness, "hello")["result"]["session"]["id"]
            h.wait_state(session, "idle", timeout=10)
            send = h.pending("session.send", {"session": session, "request_id": "racy", "prompt": "racy"})
            time.sleep(0.2)
            restarted = h.rpc(
                "session.restart",
                {"session": session, "environment": h.provider_env(), "startup_timeout_ms": 5_000},
            )
            self.assertIn("result", restarted, restarted)
            raced = h.receive(send)
            self.assertEqual(raced.get("error", {}).get("code"), "SESSION_UNAVAILABLE", (harness, raced))
            h.wait_state(session, "idle")
            followup = h.rpc("session.send", {"session": session, "request_id": "later", "prompt": "later"})
            self.assertIn("result", followup, (harness, followup))

    def test_a_stale_restart_cannot_strand_a_completed_restart(self) -> None:
        h = self.harness
        session = h.create("claude", "hello")["result"]["session"]["id"]
        h.wait_state(session, "idle")
        slow_home = h.root / "slow-home"
        slow_home.mkdir()
        (slow_home / ".fake-auth-status").write_text('{"loggedIn":true,"authMethod":"claude.ai"}')
        (slow_home / ".fake-auth-delay").write_text("1.5")
        stale = h.pending(
            "session.restart",
            {
                "session": session,
                "environment": h.provider_env({"HOME": str(slow_home)}),
                "startup_timeout_ms": 10_000,
            },
            timeout=20,
        )
        time.sleep(0.2)
        completed = h.rpc(
            "session.restart",
            {"session": session, "environment": h.provider_env(), "startup_timeout_ms": 5_000},
        )
        self.assertIn("result", completed, completed)
        stale_response = h.receive(stale)
        self.assertEqual(stale_response.get("error", {}).get("code"), "SESSION_UNAVAILABLE", stale_response)
        h.wait_state(session, "idle")
        followup = h.rpc("session.send", {"session": session, "request_id": "after-restarts", "prompt": "after"})
        self.assertIn("result", followup, followup)

    def test_a_provider_exit_is_seen_while_a_descendant_holds_its_output(self) -> None:
        h = self.harness
        for harness in ("claude", "codex"):
            holder_file = h.root / f"{harness}-holder.pid"
            session = h.create(harness, "first", {"DLGT_FAKE_HOLDER_PID_FILE": str(holder_file)})["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.send", {"session": session, "request_id": f"{harness}-crash", "prompt": "crash-hold"})
            h.wait_state(session, "failed", timeout=5)
            deadline = time.monotonic() + 3
            while not (holder_file.exists() and holder_file.read_text().strip()) and time.monotonic() < deadline:
                time.sleep(0.01)
            time.sleep(0.2)
            holder = int(holder_file.read_text())
            state = subprocess.run(
                ["ps", "-o", "stat=", "-p", str(holder)], check=False, capture_output=True, text=True
            ).stdout.strip()
            self.assertTrue(not state or state.startswith("Z"), f"{harness} descendant survived: {state}")

    def test_workspace_trust_is_written_where_the_launch_environment_points(self) -> None:
        h = self.harness
        selected = h.root / "selected-home"
        selected.mkdir()
        response = h.create("claude", "trust", {"HOME": str(selected)})
        self.assertIn("result", response, response)
        self.assertTrue((selected / ".claude.json").exists())
        self.assertFalse((h.home / ".claude.json").exists())
        codex_home = h.root / "selected-codex"
        response = h.create("codex", "trust", {"CODEX_HOME": str(codex_home)})
        self.assertIn("result", response, response)
        self.assertTrue((codex_home / "config.toml").exists())
        self.assertFalse((h.home / ".codex" / "config.toml").exists())
        response = h.create("codex", "trust", {"CODEX_HOME": "relative-codex"})
        self.assertIn("result", response, response)
        self.assertTrue((h.root / "relative-codex" / "config.toml").exists())

    def test_answer_receipts_keep_their_fields_when_retention_trims_the_request(self) -> None:
        self.harness.close()
        self.harness = Harness(
            {
                "DLGT_TEST_RESOLVED_RETENTION": "0",
                "DLGT_TEST_PAUSE_BEFORE_ANSWER_DELIVERY_MS": "400",
            }
        )
        h = self.harness
        session = h.create("codex", "ask", approvals="ask")["result"]["session"]["id"]
        h.wait_state(session, "blocked")
        answer = {"session": session, "request": "r1", "decision": "deny", "message": "no"}
        first = h.pending("session.answer", answer)
        time.sleep(0.1)
        duplicate = h.rpc("session.answer", answer)
        first = h.receive(first)
        for receipt in (first, duplicate):
            self.assertIs(receipt.get("result", {}).get("request", {}).get("reason_delivered"), True, receipt)
        h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "ask-alone", "prompt": "ask"})
        h.wait_state(session, "blocked")
        alone = h.rpc("session.answer", {"session": session, "request": "r2", "decision": "deny", "message": "no"})
        self.assertIs(alone.get("result", {}).get("request", {}).get("reason_delivered"), True, alone)

    def test_configuration_failures_keep_configuration_codes(self) -> None:
        h = self.harness
        (h.home / ".claude.json").mkdir()
        response = h.create("claude", "directory config")
        self.assertEqual(response.get("error", {}).get("code"), "INVALID_ARGUMENT", response)
        (h.home / ".claude.json").rmdir()

        def stdio_profiles(config: Path) -> dict:
            proxy = subprocess.run(
                [str(BIN), "rpc", "--stdio"],
                input=json.dumps({"id": "p", "method": "profile.list", "params": {}}) + "\n",
                env=dict(h.environment, DLGT_CONFIG=str(config)),
                capture_output=True,
                text=True,
                timeout=20,
            )
            return json.loads(next(line for line in proxy.stdout.splitlines() if line.strip()))

        directory = h.root / "config-directory"
        directory.mkdir()
        self.assertEqual(stdio_profiles(directory).get("error", {}).get("code"), "INVALID_ARGUMENT")
        scalar = h.root / "scalar.toml"
        scalar.write_text("profiles = 3\n")
        self.assertEqual(stdio_profiles(scalar).get("error", {}).get("code"), "INVALID_ARGUMENT")
        typo = h.root / "typo.toml"
        typo.write_text('[defualts.codex]\napprovals = "ask"\n')
        self.assertEqual(stdio_profiles(typo).get("error", {}).get("code"), "INVALID_ARGUMENT")

        missing = subprocess.run(
            [str(BIN), "profiles", "show", "missing"],
            env=dict(h.environment, DLGT_CONFIG=str(h.root / "none.toml")),
            capture_output=True,
            text=True,
            timeout=20,
        )
        self.assertIn('"NOT_FOUND"', missing.stdout + missing.stderr)

    def test_a_slow_codex_acknowledgement_returns_a_pending_receipt(self) -> None:
        h = self.harness
        release = h.root / "ack-release"
        started = time.monotonic()
        created = h.create("codex", "slow ack", {"DLGT_FAKE_DELAY_TURN_START_ACK": str(release)})
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 8, elapsed)
        self.assertEqual(created.get("result", {}).get("submission"), "pending", created)
        session = created["result"]["session"]["id"]
        release.touch()
        final = h.wait_state(session, "idle", timeout=10)["result"]
        self.assertEqual(final["final_text"], "fake:slow ack", final)

    def test_settled_codex_turns_stop_reconciling(self) -> None:
        h = self.harness
        reads = h.root / "thread-reads"
        session = h.create("codex", "one", {"DLGT_FAKE_THREAD_READ_LOG": str(reads)})["result"]["session"]["id"]
        h.wait_state(session, "idle")
        for index in range(5):
            h.rpc("session.send", {"session": session, "request_id": f"quick-{index}", "prompt": f"quick {index}"})
            h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "holding", "prompt": "hold-reads"})
        h.wait_state(session, "busy")
        time.sleep(0.5)
        before = len(reads.read_text().split()) if reads.exists() else 0
        time.sleep(1.0)
        after = len(reads.read_text().split()) if reads.exists() else 0
        self.assertLessEqual(after - before, 6, "settled turns kept polling thread/read")

    def events_of(self, session: str) -> list:
        history = self.harness.rpc("session.fetch", {"session": session, "from_start": True})
        return [
            event.get("type")
            for bucket in history.get("result", {}).get("sessions", [])
            for event in bucket.get("events", [])
        ]

    def test_a_settled_cancel_announces_idle_once(self) -> None:
        h = self.harness
        for harness, extra in (("claude", {}), ("codex", {}), ("codex", {"DLGT_FAKE_INTERRUPT_REPLY_LAST": "1"})):
            session = h.create(harness, "hold-events", extra)["result"]["session"]["id"]
            h.wait_state(session, "busy")
            canceled = h.rpc("session.cancel", {"session": session, "timeout_ms": 3_000})
            self.assertEqual(canceled.get("result", {}).get("result", {}).get("status"), "canceled", canceled)
            events = self.events_of(session)
            self.assertEqual(events[events.index("session.canceling"):], ["session.canceling", "session.idle"], (harness, extra, events))

    def test_a_rejected_codex_start_during_cancel_fails_the_session(self) -> None:
        h = self.harness
        release = h.root / "reject-release"
        created = h.create(
            "codex",
            "first",
            {"DLGT_FAKE_DELAY_TURN_START_ACK": str(release), "DLGT_FAKE_REJECT_TURN_START": "1"},
        )
        self.assertEqual(created.get("result", {}).get("submission"), "pending", created)
        session = created["result"]["session"]["id"]
        cancel = h.pending("session.cancel", {"session": session, "timeout_ms": 4_000})
        time.sleep(0.3)
        release.touch()
        canceled = h.receive(cancel)
        state = h.wait_state(session, "failed")
        self.assertIn(state["result"]["status"], ("failed", "canceled"), (canceled, state))
        followup = h.rpc("session.send", {"session": session, "request_id": "after-reject", "prompt": "x"})
        self.assertEqual(followup.get("error", {}).get("code"), "SESSION_NOT_RUNNING", followup)

    def test_a_stop_can_be_forced_while_the_provider_ignores_it(self) -> None:
        h = self.harness
        session = h.create("claude", "first", {"DLGT_FAKE_IGNORE_TERM": "1"})["result"]["session"]["id"]
        h.wait_state(session, "idle")
        self.assertIn("result", h.rpc("session.stop", {"session": session}))
        h.wait_state(session, "stopping")
        forced = h.rpc("session.stop", {"session": session, "force": True})
        self.assertIn("result", forced, forced)
        h.wait_state(session, "stopped")

    def test_a_provider_exit_is_seen_while_an_escaped_process_holds_its_output(self) -> None:
        h = self.harness
        for harness in ("claude", "codex"):
            holder_file = h.root / f"{harness}-escaped.pid"
            session = h.create(
                harness,
                "first",
                {"DLGT_FAKE_HOLDER_PID_FILE": str(holder_file), "DLGT_FAKE_ESCAPE_HOLDER": "1"},
            )["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.send", {"session": session, "request_id": f"{harness}-escape", "prompt": "crash-hold"})
            h.wait_state(session, "failed", timeout=6)

    def test_create_returns_the_id_claude_rotated_to(self) -> None:
        h = self.harness
        created = h.create("claude", "rotate", {"DLGT_FAKE_ROTATE_AT_START": "1"})
        session = created["result"]["session"]["id"]
        self.assertIn("result", h.rpc("session.read", {"session": session}), created)
        h.wait_state(session, "idle")
        followup = h.rpc("session.send", {"session": session, "request_id": "after-rotate", "prompt": "next"})
        self.assertIn("result", followup, followup)

    def test_a_failed_create_never_fails_the_restart_that_replaced_it(self) -> None:
        for harness in ("claude", "codex"):
            self.harness.close()
            self.harness = Harness({"DLGT_TEST_PAUSE_BEFORE_SUBMIT_RESERVATION_MS": "1500"})
            h = self.harness
            create = h.pending(
                "session.create",
                {
                    "title": "create race",
                    "harness": harness,
                    "cwd": str(h.root),
                    "environment": h.provider_env(),
                    "request_id": f"create-race-{harness}",
                    "prompt": "hello",
                },
            )
            deadline = time.monotonic() + 5
            session = None
            while session is None and time.monotonic() < deadline:
                listed = h.rpc("session.list", {}).get("result", [])
                session = next((item["id"] for item in listed if item.get("state") == "idle"), None)
                time.sleep(0.05)
            self.assertIsNotNone(session, "the created Session never became idle")
            restarted = h.rpc(
                "session.restart",
                {"session": session, "environment": h.provider_env(), "startup_timeout_ms": 5_000},
            )
            self.assertIn("result", restarted, restarted)
            self.assertIn("error", h.receive(create))
            h.wait_state(session, "idle")
            followup = h.rpc("session.send", {"session": session, "request_id": "after-race", "prompt": "after"})
            self.assertIn("result", followup, (harness, followup))

    def test_a_late_rotation_from_a_replaced_claude_never_renames_its_replacement(self) -> None:
        self.harness.close()
        self.harness = Harness({"DLGT_TEST_PAUSE_BEFORE_CLAUDE_REKEY_MS": "5000"})
        h = self.harness
        session = h.create("claude", "first")["result"]["session"]["id"]
        h.wait_state(session, "idle", timeout=10)
        rotating = h.rpc(
            "session.restart",
            {
                "session": session,
                "environment": h.provider_env({"DLGT_FAKE_ROTATE_AT_START": "1"}),
                "startup_timeout_ms": 5_000,
            },
        )
        self.assertIn("result", rotating, rotating)
        # The prompt makes the rotating process report its new ID; its rekey
        # is held past the next restart.
        h.pending("session.send", {"session": session, "request_id": "rotate", "prompt": "rotate"})
        time.sleep(0.3)
        replaced = h.rpc(
            "session.restart",
            {"session": session, "environment": h.provider_env(), "startup_timeout_ms": 10_000},
            timeout=20,
        )
        self.assertIn("result", replaced, replaced)
        time.sleep(5.5)
        h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "after-rotation", "prompt": "next"})
        final = h.wait_state(session, "idle", timeout=10)["result"]
        self.assertEqual(final["final_text"], "fake:next", final)
    def test_a_provider_that_closes_its_output_fails_the_session(self) -> None:
        h = self.harness
        for harness in ("claude", "codex"):
            session = h.create(harness, "first")["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.send", {"session": session, "request_id": f"{harness}-close", "prompt": "close-output"})
            failed = h.wait_state(session, "failed", timeout=8)
            self.assertIsNotNone(failed["result"], failed)

    def test_output_from_a_dead_process_never_reaches_its_resumed_session(self) -> None:
        h = self.harness
        holder = h.root / "old-holder.pid"
        extra = {
            "DLGT_FAKE_HOLDER_PID_FILE": str(holder),
            "DLGT_FAKE_ESCAPE_HOLDER": "1",
            "DLGT_FAKE_ESCAPE_EMIT_AFTER": "5",
        }
        session = h.create("claude", "first", extra)["result"]["session"]["id"]
        h.wait_state(session, "idle")
        h.rpc("session.send", {"session": session, "request_id": "crash", "prompt": "crash-hold"})
        h.wait_state(session, "failed", timeout=6)
        resumed = h.rpc(
            "session.send",
            {
                "session": session,
                "resume": True,
                "request_id": "resume",
                "prompt": "hold-new",
                "cwd": str(h.root),
                "environment": h.provider_env(),
            },
        )
        self.assertIn("result", resumed, resumed)
        current = resumed["result"]["session"]["id"]
        time.sleep(5)
        state = h.state(current)
        self.assertEqual(state["session"]["state"], "busy", state)
        self.assertNotIn("OLD_PROCESS_RESULT", json.dumps(state), state)

    def test_a_stalled_claude_write_still_returns_a_pending_receipt(self) -> None:
        h = self.harness
        pid_file = h.root / "claude.pid"
        session = h.create("claude", "first", {"DLGT_FAKE_PID_FILE": str(pid_file)})["result"]["session"]["id"]
        h.wait_state(session, "idle")
        provider = int(pid_file.read_text())
        os.kill(provider, signal.SIGSTOP)
        try:
            started = time.monotonic()
            sent = h.rpc(
                "session.send",
                {"session": session, "request_id": "stalled", "prompt": "x" * 300_000},
                timeout=20,
            )
            self.assertLess(time.monotonic() - started, 8, sent)
            self.assertEqual(sent.get("result", {}).get("submission"), "pending", str(sent)[:300])
        finally:
            os.kill(provider, signal.SIGCONT)

    def test_output_readers_end_after_an_escaped_holder_outlives_the_provider(self) -> None:
        h = self.harness

        def threads() -> int:
            task_dir = Path(f"/proc/{h.server.pid}/task")
            if task_dir.exists():
                return len(list(task_dir.iterdir()))
            listing = subprocess.run(
                ["ps", "-M", "-p", str(h.server.pid)], check=False, capture_output=True, text=True
            ).stdout
            return len(listing.splitlines()) - 1

        baseline = threads()
        for harness in ("claude", "codex"):
            holder_file = h.root / f"{harness}-reader.pid"
            session = h.create(
                harness,
                "first",
                {"DLGT_FAKE_HOLDER_PID_FILE": str(holder_file), "DLGT_FAKE_ESCAPE_HOLDER": "1"},
            )["result"]["session"]["id"]
            h.wait_state(session, "idle")
            h.rpc("session.send", {"session": session, "request_id": f"{harness}-reader", "prompt": "crash-hold"})
            h.wait_state(session, "failed", timeout=6)
        time.sleep(1.5)
        self.assertLessEqual(threads(), baseline + 2, "provider output readers outlived their exit")


if __name__ == "__main__":
    unittest.main(verbosity=2)
