#!/usr/bin/env python3
"""PTY fixture for the OpenCode and Pi harnesses.

Speaks the Claude-shaped hook bridge those adapters install. No model calls.
`opencode models` and `pi --list-models` print a stable catalog and exit.
"""
import json
import os
import subprocess
import sys
import tty
import uuid


def emit(agent, payload):
    if agent == "opencode":
        binary = os.environ["DLGT_OPENCODE_HOOK_BIN"]
        launch = os.environ["DLGT_OPENCODE_LAUNCH"]
    else:
        binary = os.environ["DLGT_PI_HOOK_BIN"]
        launch = os.environ["DLGT_PI_LAUNCH"]
    subprocess.run(
        [binary, "hook", "emit", launch, agent],
        input=json.dumps(payload).encode(),
        check=True,
    )


def interactive(agent):
    args = sys.argv[1:]
    if "--mode" in args:
        sys.stderr.write("dlgt pi adapter does not launch RPC mode\n")
        sys.exit(1)
    if "--session" in args:
        session = args[args.index("--session") + 1]
    else:
        session = str(uuid.uuid4())
    launch = os.environ[
        "DLGT_OPENCODE_LAUNCH" if agent == "opencode" else "DLGT_PI_LAUNCH"
    ]
    safe = launch.replace(":", "_")
    pid_path = os.path.join(os.environ["HOME"], f"{agent}-{safe}.pid")
    with open(pid_path, "w", encoding="utf-8") as pid_file:
        pid_file.write(str(os.getpid()))
    if sys.stdin.isatty():
        tty.setraw(0)
    print(f"{agent} fixture ready", flush=True)
    emit(
        agent,
        {
            "hook_event_name": "SessionStart",
            "session_id": session,
            "cwd": os.getcwd(),
        },
    )

    def turn(prompt):
        emit(
            agent,
            {
                "hook_event_name": "UserPromptSubmit",
                "session_id": session,
                "cwd": os.getcwd(),
                "prompt": prompt,
            },
        )
        print("result:" + prompt, flush=True)
        emit(
            agent,
            {
                "hook_event_name": "Stop",
                "session_id": session,
                "cwd": os.getcwd(),
                "prompt": prompt,
                "last_assistant_message": "result:" + prompt,
            },
        )

    buffer = b""
    while True:
        char = os.read(0, 1)
        if not char:
            break
        if char == b"\r":
            prompt = (
                buffer.replace(b"\x1b[200~", b"")
                .replace(b"\x1b[201~", b"")
                .decode()
            )
            buffer = b""
            turn(prompt)
        else:
            buffer += char


def main():
    args = sys.argv[1:]
    if "--version" in args:
        print("fake-pty-agent 0.0.0")
        return
    if args[:1] == ["models"]:
        print("xai/grok-4.7")
        print("xai/grok-4.6 (default)")
        return
    if "--list-models" in args:
        print("provider  model     context  max-out  thinking  images")
        print("xai       grok-4.7  128K     8K       yes       no")
        print("xai       grok-4    128K     8K       yes       no")
        return
    if os.environ.get("DLGT_OPENCODE_LAUNCH"):
        interactive("opencode")
    elif os.environ.get("DLGT_PI_LAUNCH"):
        interactive("pi")
    else:
        sys.stderr.write("fake pty agent expected a dlgt launch environment\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
