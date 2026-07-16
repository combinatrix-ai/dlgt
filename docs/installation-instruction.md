# Install dlgt

This page is an executable installation recipe for a Codex or Claude agent.
It installs one local Rust binary and registers the embedded dlgt skill for the
harnesses you use.

The supported installation scope is macOS and Linux. The runtime uses Unix
process and socket APIs, so do not advertise this recipe as a Windows install.

## Prerequisites

Run these checks in the shell where you will use dlgt:

~~~sh
case "$(uname -s)" in
  Darwin|Linux) ;;
  *) echo "dlgt installation supports macOS and Linux only" >&2; exit 1 ;;
esac

for command in git cargo rustc; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

git --version
cargo --version
rustc --version
~~~

You also need at least one supported harness installed and authenticated:

~~~sh
found_harness=0
for harness in codex claude; do
  if command -v "$harness" >/dev/null 2>&1; then
    "$harness" --version
    found_harness=1
  fi
done
test "$found_harness" -eq 1 || {
  echo "install and authenticate codex or claude before installing dlgt" >&2
  exit 1
}
~~~

If a command is not on PATH, fix that harness installation before installing
dlgt. The dlgt binary does not provide Codex or Claude, and it does not provide
their authentication.

If Rust is missing, install the stable toolchain with [rustup](https://rustup.rs/),
then open a new shell and repeat the checks. A native C compiler may also be
needed by Cargo dependencies: install Xcode Command Line Tools on macOS or the
standard build tools for your Linux distribution.

## Clone or update the source safely

The following recipe uses ~/src/dlgt by default. Set DLGT_SOURCE_DIR if you
want another location. It refuses to update a dirty checkout, a detached
checkout, or a directory whose origin is not the dlgt repository. It never
deletes or resets local files.

~~~sh
set -eu

repo_dir="${DLGT_SOURCE_DIR:-$HOME/src/dlgt}"
expected_remote="https://github.com/combinatrix-ai/dlgt.git"

if [ -e "$repo_dir" ]; then
  if [ ! -d "$repo_dir/.git" ]; then
    echo "refusing to use $repo_dir: it exists but is not a Git checkout" >&2
    exit 1
  fi

  cd "$repo_dir"
  remote="$(git remote get-url origin 2>/dev/null || true)"
  case "$remote" in
    "$expected_remote"|https://github.com/combinatrix-ai/dlgt|git@github.com:combinatrix-ai/dlgt|git@github.com:combinatrix-ai/dlgt.git) ;;
    *)
      echo "refusing to update: origin is $remote, not $expected_remote" >&2
      exit 1
      ;;
  esac

  test "$(git branch --show-current)" = main || {
    echo "refusing to update: checkout must be on the main branch" >&2
    exit 1
  }
  test -z "$(git status --porcelain)" || {
    echo "refusing to update: checkout has uncommitted or untracked files" >&2
    exit 1
  }

  git fetch --prune origin
  git pull --ff-only origin main
else
  mkdir -p "$(dirname "$repo_dir")"
  git clone --branch main --single-branch "$expected_remote" "$repo_dir"
  cd "$repo_dir"
fi
~~~

If this stops, inspect the reported checkout with git status and git remote -v.
Resolve the situation deliberately, or choose a fresh DLGT_SOURCE_DIR; do not
add git reset --hard to the recipe.

## Build and install the binary

Build the release binary, then copy only that binary into the user-writable
~/.local/bin directory. No sudo or system-wide installation is required.

~~~sh
set -eu

repo_dir="${DLGT_SOURCE_DIR:-$HOME/src/dlgt}"
cd "$repo_dir"
cargo build --release --locked

bin_dir="${DLGT_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$bin_dir"
install -m 755 target/release/dlgt "$bin_dir/dlgt"

# Make the new location available in this shell immediately.
case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *) export PATH="$bin_dir${PATH:+:$PATH}" ;;
esac
~~~

For future shells, add the same directory to the startup file used by your
login shell. On macOS with zsh this is commonly ~/.zprofile; on Linux it is
commonly ~/.profile, ~/.bashrc, or ~/.zshrc:

~~~sh
export PATH="$HOME/.local/bin:$PATH"
~~~

Use the same DLGT_BIN_DIR value if you chose a custom install directory.

## Register the skill

dlgt embeds the agent-facing contract from assets/dlgt-skill.md. After the
binary is installed, dlgt skill is the canonical source to register. Do not
copy a skill from a different checkout or edit the embedded contract by hand.

The function below writes atomically. When updating an existing different
skill, it preserves that file beside the new copy with a timestamped `.backup`
suffix. Run it once for each harness you actively use:

~~~sh
set -eu

bin_dir="${DLGT_BIN_DIR:-$HOME/.local/bin}"
export PATH="$bin_dir${PATH:+:$PATH}"

install_dlgt_skill() {
  target="$1"
  skill_dir="${target%/*}"
  mkdir -p "$skill_dir"

  temporary="$(mktemp "${target}.tmp.XXXXXX")"
  dlgt skill >"$temporary"

  if [ -f "$target" ] && cmp -s "$temporary" "$target"; then
    rm -f "$temporary"
    return 0
  fi
  if [ -e "$target" ] || [ -L "$target" ]; then
    backup="${target}.backup.$(date +%Y%m%d%H%M%S)"
    mv "$target" "$backup"
    echo "preserved previous skill at $backup"
  fi

  chmod 644 "$temporary"
  mv "$temporary" "$target"
}
~~~

For Codex, install into the user-level skill root. This honors a custom
CODEX_HOME when one is already configured:

~~~sh
install_dlgt_skill "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md"
~~~

For Claude Code, install into the personal skill root shared by your projects:

~~~sh
install_dlgt_skill "$HOME/.claude/skills/dlgt/SKILL.md"
~~~

Start a new Codex or Claude session after registering the skill so the harness
refreshes its skill inventory. The skill is guidance for the agent; it does
not run a daemon or change harness settings by itself.

## Verify the installation

Run the binary and compare every installed copy with the binary's embedded
skill:

~~~sh
set -eu

bin_dir="${DLGT_BIN_DIR:-$HOME/.local/bin}"
export PATH="$bin_dir${PATH:+:$PATH}"

command -v dlgt
dlgt version
dlgt harnesses

skill_snapshot="$(mktemp)"
dlgt skill >"$skill_snapshot"
grep -q '^name: dlgt$' "$skill_snapshot"
grep -q '^description:' "$skill_snapshot"

found_skill=0
for target in \
  "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md" \
  "$HOME/.claude/skills/dlgt/SKILL.md"; do
  if [ -f "$target" ]; then
    cmp -s "$skill_snapshot" "$target" || {
      echo "skill does not match the installed binary: $target" >&2
      rm -f "$skill_snapshot"
      exit 1
    }
    echo "verified $target"
    found_skill=1
  fi
done

test "$found_skill" -eq 1 || {
  echo "no registered dlgt skill was found" >&2
  rm -f "$skill_snapshot"
  exit 1
}
rm -f "$skill_snapshot"
~~~

The minimum successful result is a path to dlgt, a version string, a
successful harnesses response containing the supported codex and claude
harnesses, and a matching SKILL.md for each harness you registered.

## If something fails

- **cargo, rustc, or a compiler is missing:** install Rust with rustup and the
  platform's native build tools, open a new shell, and rerun the checks.
- **git clone or update is refused:** keep the existing directory intact;
  inspect git status and git remote -v, or choose a new DLGT_SOURCE_DIR.
- **install: target/release/dlgt: No such file:** run
  cargo build --release --locked
  from the repository root and check the Cargo error before retrying.
- **dlgt: command not found:** export the selected DLGT_BIN_DIR (or
  $HOME/.local/bin) into the current shell's PATH, then run hash -r in bash or
  rehash in zsh. Try the absolute path if needed.
- **A skill is not visible:** verify the exact path and `name: dlgt` front
  matter, compare it with `dlgt skill`, and start a new harness session. Review
  the timestamped backup if the installer replaced a different existing copy.
- **A session cannot start:** verify the relevant codex or claude command and
  its authentication in the same shell. If the executable has a custom name or
  location, set DLGT_CODEX_BIN or DLGT_CLAUDE_BIN before launching dlgt.
- **The daemon uses unexpected state:** the default local state is under
  ~/.dlgt. Set DLGT_HOME to an explicit writable directory, or set DLGT_SOCKET
  when only the Unix socket location must change.

Once these checks pass, the active harness can use the registered dlgt skill to
create, address, wait for, and inspect persistent Sessions. See the
[CLI reference](/cli) for the complete command contract.
