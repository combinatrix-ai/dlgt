# Install dlgt

This page is an agent-executable contract for installing the latest published
dlgt release for Codex or Claude. When asked to follow it, do not report
completion after installing only the binary. Completion requires verifying the
binary and every requested installed skill copy.

The installer downloads a platform-specific binary from GitHub Releases,
verifies its SHA-256 checksum, installs it as dlgt in a user-writable
directory, and registers the embedded skill when a user-level Codex or Claude
Harness is installed. Fresh Harness homes are created at the correct skill
roots when needed.

Normal users do not need Rust, Cargo, a compiler, or a source checkout. The
published targets cover macOS and Linux on x86_64 and arm64, with both glibc
and musl Linux packages.

## Prerequisites

Install and authenticate at least one supported harness in the same shell:

~~~sh
for harness in codex claude; do
  if command -v "$harness" >/dev/null 2>&1; then
    "$harness" --version
  fi
done
~~~

The installer requires curl, tar, and a SHA-256 utility (sha256sum on most
Linux systems or shasum on macOS). It supports macOS and Linux only. It does
not install Codex or Claude, and it does not provide their authentication.

## Install the latest release

The recommended one-line install uses the checked-in installer and its
HTTPS-only release downloads:

~~~sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --skill both
~~~

Use `--skill both` when following this page from the README. It deterministically
installs the embedded skill for Codex and Claude without relying on command or
existing-directory detection.

The default binary location is ~/.local/bin. The installer never requires
sudo or writes a shell profile. If that directory is not already on PATH, add
it for future shells and export it in the current shell:

~~~sh
export PATH="$HOME/.local/bin:$PATH"
~~~

For a reviewable install, download the script first and run it locally:

~~~sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  -o /tmp/install-dlgt.sh
sh /tmp/install-dlgt.sh --skill both
rm -f /tmp/install-dlgt.sh
~~~

## Install a specific version

Pass a release tag or a bare semantic version. The installer normalizes a
bare version to a v tag and validates the requested value before constructing
release URLs:

~~~sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --version v0.1.0
~~~

Use DLGT_BIN_DIR or --bin-dir for another user-writable location:

~~~sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | DLGT_BIN_DIR="$HOME/bin" sh -s -- --version v0.1.0
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --bin-dir "$HOME/bin" --version v0.1.0
~~~

The archive and checksum asset names are target-specific, for example
dlgt-v0.1.0-aarch64-apple-darwin.tar.gz and its .sha256 file. The installer
detects the local target and refuses unsupported OS or architectures.

## Register the embedded skill

The installed binary is the canonical source of the skill:

~~~sh
dlgt skill
~~~

By default, the installer detects installed `codex` and `claude` commands and
registers the skill in their user-level roots: Codex uses
`$CODEX_HOME/skills/dlgt/` (or `~/.codex/skills/dlgt/`) and Claude uses
`~/.claude/skills/dlgt/`. Existing Harness homes are also recognized even when
the command is not currently on `PATH`. Select the intended Harnesses
explicitly when needed, especially in container images:

~~~sh
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --skill codex
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --skill claude
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --skill both
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh \
  | sh -s -- --no-skill
~~~

Skill updates are atomic. If an existing different SKILL.md is present, the
installer preserves it beside the new copy with a timestamped .backup suffix.
Start a new Codex or Claude session after registration so it refreshes its
skill inventory.

For a source-built or otherwise preinstalled binary, skip release download and
register the skill from that exact executable:

~~~sh
cargo build --release --bin dlgt
sh install.sh --register-skills-from target/release/dlgt --skill both
~~~

This is the recommended container-image setup because it guarantees that both
Harnesses receive the skill embedded in the binary actually placed in the
image.

### Note: delegation runs auto-approved

The embedded skill lets an installed agent launch counterpart Sessions on its
own. By default dlgt marks each Session working directory trusted and starts
workers with approvals disabled
(`--dangerously-bypass-approvals-and-sandbox` for Codex,
`--permission-mode=auto` for Claude), so a delegated worker can edit
files and run commands there without asking. Install and delegate only in
workspaces you trust, and use `--no-auto-approve` or a Profile with
`auto_approve = false` to keep the Harness's own permission prompts.

## Verify the installation

Run these checks in the shell where you will use dlgt:

~~~sh
command -v dlgt
dlgt version
dlgt harnesses
dlgt skill | grep -q '^name: dlgt$'
~~~

The README installation is complete only when both installed copies match the
skill embedded in the installed binary:

~~~sh
skill_snapshot="$(mktemp)"
dlgt skill >"$skill_snapshot"
cmp -s "$skill_snapshot" "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md"
cmp -s "$skill_snapshot" "$HOME/.claude/skills/dlgt/SKILL.md"
rm -f "$skill_snapshot"
~~~

The first client command starts the local dlgt daemon automatically. State
and the Unix socket remain local under ~/.dlgt by default; use DLGT_HOME or
DLGT_SOCKET to relocate them.

## Contributor source builds

Source builds are for contributors and release maintainers, not the normal
installation path. From a dlgt checkout:

~~~sh
cargo build --bin dlgt
cargo build --release --bin dlgt
target/release/dlgt version
~~~

Set the Cargo package version to the matching v tag before pushing a release
tag. The workflow rejects a tag whose version does not match Cargo.toml.

The release workflow runs these explicit dlgt builds for version tags and
publishes the resulting target-named archives and checksums.

## If something fails

- **Unsupported platform:** the release installer supports macOS and Linux on
  x86_64 or arm64. Linux packages distinguish glibc and musl.
- **Checksum verification failed:** retry the download. Do not run an archive
  whose checksum does not match the release checksum asset.
- **dlgt: command not found:** export the selected DLGT_BIN_DIR or ~/.local/bin
  in the current shell and add it to the shell profile used by future login
  shells.
- **A skill is not visible:** verify the exact target path, compare it with
  dlgt skill, and start a new harness session. Review the timestamped backup if
  the installer replaced a different skill.
- **A Session cannot start:** verify the relevant codex or claude command and
  its authentication in the same shell. Set DLGT_CODEX_BIN or DLGT_CLAUDE_BIN
  if the harness executable is not on PATH.

Once these checks pass, the active harness can use the registered dlgt skill to
create, address, wait for, and inspect persistent Sessions. See the
[CLI reference](/cli) for the complete command contract.
