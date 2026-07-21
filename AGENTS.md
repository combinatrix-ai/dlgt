# Development

- Build the `dlgt` binary with `cargo build --bin dlgt`.
- Run checks with `cargo fmt --check`, `cargo clippy --all-targets`, and
  `cargo test`.
- Keep the runtime as one Rust binary. Runtime assets such as the agent skill
  must be embedded and emitted by the binary.
- Prefer Codex and Claude lifecycle hooks over terminal-screen inference. PTY
  parsing is for presentation and fallback only.
- Backward compatibility is not required. Delete superseded paths instead of
  adding shims.
- Keep commits small and in English. Push each logical commit.

# Docker Harness E2E

Run the Docker Harness E2E before merging a release PR that changes Session
lifecycle, provider integration, installation, embedded Skills, sockets, or
process ownership. Do not tag a release until the local-source E2E passes.

The checked-in base environment is defined by `compose.e2e.yaml` and
`tests/docker/Dockerfile`. `.dlgt-e2e-home/` contains reusable Codex and Claude
authentication. Treat it as sensitive and never commit it.

## Build the PR source

Build the exact working-tree source into a temporary multi-stage image. Include
`assets/`, because the Skill is embedded at compile time.

```dockerfile
FROM rust:1-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked --bin dlgt

FROM dlgt-agent:latest
COPY --from=build /src/target/release/dlgt /usr/local/bin/dlgt
ENV PATH="/usr/local/bin:/root/.local/bin:${PATH}"
```

Tag the successful local image as `dlgt-agent:e2e-current`. Temporary
Dockerfiles and intermediate tags must not be committed.

## Isolate every run

- Use a new container, a new `/tmp/dlgt`, and new dlgt Sessions for every run.
- Reuse provider authentication only. Copy the required files from
  `.dlgt-e2e-home/` into a fresh `mktemp` home and bind-mount that copy at
  `/root`. Do not let the test modify the canonical authentication home.
- Set `DLGT_HOME=/tmp/dlgt`. Docker Desktop bind mounts cannot reliably apply
  the Unix-socket permissions used by dlgt.
- A persisted home may contain an old `/root/.local/bin/dlgt`. Invoke the PR
  binary as `/usr/local/bin/dlgt` for both the client and daemon; do not rely on
  `PATH`.
The known-good minimal authentication copy is:

```sh
mkdir -p "$e2e_home/.codex" "$e2e_home/.claude"
cp .dlgt-e2e-home/.codex/auth.json "$e2e_home/.codex/auth.json"
cp .dlgt-e2e-home/.codex/config.toml "$e2e_home/.codex/config.toml"
cp .dlgt-e2e-home/.codex/installation_id "$e2e_home/.codex/installation_id"
cp .dlgt-e2e-home/.claude/.credentials.json "$e2e_home/.claude/.credentials.json"
cp .dlgt-e2e-home/.claude/settings.json "$e2e_home/.claude/settings.json"
cp .dlgt-e2e-home/.claude.json "$e2e_home/.claude.json"
```

Do not copy provider histories, dlgt state, installed binaries, or existing
Skill directories into the fresh home.

Emit both Skill copies from the exact PR binary before testing:

```sh
mkdir -p /root/.codex/skills/dlgt /root/.claude/skills/dlgt
/usr/local/bin/dlgt skill > /root/.codex/skills/dlgt/SKILL.md
/usr/local/bin/dlgt skill > /root/.claude/skills/dlgt/SKILL.md
```

Verify the binary version and compare both installed Skill files byte-for-byte
with `/usr/local/bin/dlgt skill` before starting a Harness.

## Required pre-release matrix

Run at least three fresh containers for each Harness:

1. Create a Claude Session with a prompt such as
   `Reply with exactly CLAUDE_E2E_<N>_OK. Do not use tools, edit files, or delegate.`
2. Create a Codex Session with the equivalent `CODEX_E2E_<N>_OK` prompt.
3. Give every `--wait` an explicit `--timeout 5m`.

`dlgt new` and `session.create` require a non-empty initial prompt. To verify
post-daemon continuity without creating duplicates, use the returned
`resume_ref` (`codex:<provider_session_id>` or `claude:<provider_session_id>`)
with `dlgt send <resume_ref> --resume -- <PROMPT>`; plain `send` never launches
a replacement and returns `SESSION_NOT_RUNNING` with that hint.

Every run passes only when all of the following are true:

- the command exits zero and returns `ok: true`;
- the result has `status: "completed"` and the exact non-empty marker in
  `final_text`;
- `provider_session_id` is non-empty;
- `dlgt list --all-versions` reports the expected `runtime_version` and
  `$DLGT_HOME/run/<version>/dlgt.sock`;
- the Session is stopped, and the container and temporary authentication copy
  are removed.

Also verify one same-Session follow-up with `dlgt send --wait`. Lifecycle
completion must come from provider events and `wait`, never from PTY silence.

## Daemon ownership and memory lifetime

In a fresh container, create an idle Harness Session, confirm the provider is
running, and issue `dlgt server stop`. The versioned socket must disappear and
no running provider process may remain. A minimal Docker PID 1 such as
`sleep infinity` may leave a dead provider as a `Z` zombie because it does not
reap adopted children; check the process state and reject non-zombie provider
processes rather than treating a zombie as live.

Repeat the ownership check by sending `SIGKILL` to the daemon PID. The sibling
reaper must observe control-pipe EOF and terminate the registered provider
process groups, including the Codex app-server group.

Also deliver `SIGINT` to the daemon process group. The reaper runs in a
separate process group and ignores ordinary shutdown signals, so it must still
enforce provider cleanup after the daemon disappears.

Start the daemon again and run `dlgt list --all`. It must return an empty
Session list: dlgt runtime state is memory-only and must not recover after
daemon exit.

## Failure diagnostics

On a timeout or non-zero result, preserve and inspect the failing container
before cleanup:

```sh
dlgt list --all --pretty
dlgt events <SESSION_ID>
dlgt scrollback <SESSION_ID> --lines 100
dlgt show <SESSION_ID>
```

Use `attach` only when a first-run or authentication screen needs human input.
A timeout, blocked Session, missing lifecycle event, empty `final_text`, or
provider process left running is a failed E2E. Fix the cause, rebuild the local
image, and repeat the fresh-container matrix.

## Post-release verification

After the tag workflow publishes all release assets, use a fresh base container
and install through the public checked-in installer rather than copying the
local binary:

```sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --skill both
```

Verify the published version, both embedded Skill copies, one real Claude
delegation, one real Codex delegation, `provider_session_id`, and the versioned
socket. Confirm that the GitHub Release is neither a draft nor a prerelease and
contains all six platform archives, six per-archive SHA-256 files, and the
checksum manifest.
