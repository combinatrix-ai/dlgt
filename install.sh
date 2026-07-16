#!/bin/sh
# Install the latest published dlgt binary, or a requested version.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh | sh
#   sh install.sh --version v0.1.0 --skill codex

set -eu

DLGT_GITHUB_REPO="combinatrix-ai/dlgt"
DLGT_BINARY_NAME="dlgt"
DLGT_INSTALLER_NO_MAIN="${DLGT_INSTALLER_NO_MAIN:-0}"

die() {
  printf 'dlgt installer: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Install the published dlgt binary into a user-writable directory.

Options:
  --version VERSION  Install VERSION (for example v0.1.0); default: latest
  --bin-dir DIR      Install into DIR; default: $DLGT_BIN_DIR or ~/.local/bin
  --skill MODE       Register the embedded skill: auto, none, codex, claude, or both
  --no-skill         Alias for --skill none
  -h, --help         Show this help
EOF
}

need_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

detect_target() {
  target_os="$1"
  target_arch="$2"
  target_libc="${3:-auto}"

  case "$target_os" in
    Darwin) os_target="apple-darwin" ;;
    Linux) os_target="unknown-linux" ;;
    *)
      printf 'unsupported operating system: %s (supported: macOS and Linux)\n' "$target_os" >&2
      return 1
      ;;
  esac

  case "$target_arch" in
    x86_64|amd64) arch_target="x86_64" ;;
    aarch64|arm64) arch_target="aarch64" ;;
    *)
      printf 'unsupported architecture: %s (supported: x86_64 and arm64)\n' "$target_arch" >&2
      return 1
      ;;
  esac

  if [ "$target_os" = "Linux" ]; then
    case "$target_libc" in
      auto)
        if [ -f /etc/alpine-release ] || {
          command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl;
        }; then
          target_libc="musl"
        else
          target_libc="gnu"
        fi
        ;;
      gnu|musl) ;;
      *)
        printf 'unsupported Linux libc: %s (supported: glibc and musl)\n' "$target_libc" >&2
        return 1
        ;;
    esac
    printf '%s-%s-%s\n' "$arch_target" "$os_target" "$target_libc"
  else
    printf '%s-%s\n' "$arch_target" "$os_target"
  fi
}

release_asset_name() {
  release_tag="$1"
  target="$2"
  printf '%s-%s-%s.tar.gz\n' "$DLGT_BINARY_NAME" "$release_tag" "$target"
}

validate_version() {
  candidate="$1"
  case "$candidate" in
    *[!A-Za-z0-9.+_-]*) die "invalid version '$candidate'" ;;
  esac
  normalized="${candidate#v}"
  core="${normalized%%[-+]*}"
  old_ifs="$IFS"
  IFS=.
  set -- $core
  IFS="$old_ifs"
  [ "$#" -eq 3 ] || die "invalid version '$candidate'; expected v1.2.3 or 1.2.3"
  for component in "$@"; do
    case "$component" in
      ''|*[!0-9]*) die "invalid version '$candidate'; expected v1.2.3 or 1.2.3" ;;
    esac
  done
}

resolve_release_tag() {
  requested="$1"
  if [ "$requested" = "latest" ]; then
    latest_url="https://github.com/${DLGT_GITHUB_REPO}/releases/latest"
    final_url="$(curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
      --output /dev/null --write-out '%{url_effective}' "$latest_url")" \
      || die "could not resolve the latest dlgt release"
    release_tag="${final_url##*/}"
  else
    case "$requested" in
      v*) release_tag="$requested" ;;
      *) release_tag="v$requested" ;;
    esac
  fi
  validate_version "$release_tag"
  printf '%s\n' "$release_tag"
}

download() {
  source_url="$1"
  destination="$2"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    --output "$destination" "$source_url" \
    || die "download failed: $source_url"
}

sha256() {
  checksum_path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$checksum_path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$checksum_path" | awk '{print $1}'
  else
    die "required checksum command not found: sha256sum or shasum"
  fi
}

verify_checksum() {
  archive_path="$1"
  checksum_path="$2"
  expected="$(awk 'NF { print $1; exit }' "$checksum_path")"
  case "$expected" in
    "") die "checksum file is empty: $checksum_path" ;;
    *[!0-9A-Fa-f]*) die "checksum file contains an invalid digest: $checksum_path" ;;
  esac
  actual="$(sha256 "$archive_path")"
  expected="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
  [ "$actual" = "$expected" ] || die "checksum verification failed for $(basename "$archive_path")"
}

install_skill() {
  skill_target="$1"
  skill_directory="$(dirname "$skill_target")"
  mkdir -p "$skill_directory" || die "could not create skill directory: $skill_directory"
  skill_temp="$(mktemp "$skill_directory/.dlgt-skill.XXXXXX")" \
    || die "could not create a temporary skill file in $skill_directory"
  if ! "$installed_path" skill > "$skill_temp"; then
    rm -f "$skill_temp"
    die "could not read the embedded dlgt skill"
  fi

  if [ -f "$skill_target" ] && cmp -s "$skill_temp" "$skill_target"; then
    rm -f "$skill_temp"
    printf 'dlgt skill already current: %s\n' "$skill_target"
    return 0
  fi

  if [ -e "$skill_target" ] || [ -L "$skill_target" ]; then
    skill_backup="${skill_target}.backup.$(date +%Y%m%d%H%M%S)"
    mv "$skill_target" "$skill_backup" \
      || { rm -f "$skill_temp"; die "could not preserve existing skill: $skill_target"; }
    printf 'preserved existing skill: %s\n' "$skill_backup"
  fi
  chmod 644 "$skill_temp"
  mv -f "$skill_temp" "$skill_target" \
    || die "could not install skill: $skill_target"
  printf 'registered dlgt skill: %s\n' "$skill_target"
}

register_skills() {
  skill_mode="$1"
  registered=0
  case "$skill_mode" in
    none) return 0 ;;
    auto)
      if [ -n "${CODEX_HOME:-}" ] || [ -d "$HOME/.codex" ]; then
        install_skill "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md"
        registered=1
      fi
      if [ -d "$HOME/.claude" ]; then
        install_skill "$HOME/.claude/skills/dlgt/SKILL.md"
        registered=1
      fi
      ;;
    codex)
      install_skill "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md"
      registered=1
      ;;
    claude)
      install_skill "$HOME/.claude/skills/dlgt/SKILL.md"
      registered=1
      ;;
    both)
      install_skill "${CODEX_HOME:-$HOME/.codex}/skills/dlgt/SKILL.md"
      install_skill "$HOME/.claude/skills/dlgt/SKILL.md"
      registered=1
      ;;
    *) die "invalid skill mode '$skill_mode'; expected auto, none, codex, claude, or both" ;;
  esac
  if [ "$registered" -eq 0 ]; then
    printf 'dlgt skill registration skipped; use --skill codex, --skill claude, or --skill both\n'
  fi
}

main() {
  requested_version="latest"
  bin_dir="${DLGT_BIN_DIR:-$HOME/.local/bin}"
  skill_mode="auto"

  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version)
        [ "$#" -ge 2 ] || die "--version requires a value"
        requested_version="$2"
        shift 2
        ;;
      --bin-dir)
        [ "$#" -ge 2 ] || die "--bin-dir requires a value"
        bin_dir="$2"
        shift 2
        ;;
      --skill)
        [ "$#" -ge 2 ] || die "--skill requires a value"
        skill_mode="$2"
        shift 2
        ;;
      --no-skill)
        skill_mode="none"
        shift
        ;;
      -h|--help)
        usage
        return 0
        ;;
      *) die "unknown option '$1'; use --help for usage" ;;
    esac
  done

  need_command curl
  need_command tar
  need_command awk
  need_command mktemp
  need_command cmp
  target="$(detect_target "$(uname -s)" "$(uname -m)")" \
    || die "could not map this machine to a published dlgt target"
  release_tag="$(resolve_release_tag "$requested_version")"
  archive_name="$(release_asset_name "$release_tag" "$target")"
  release_base="https://github.com/${DLGT_GITHUB_REPO}/releases/download/${release_tag}"

  temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/dlgt-install.XXXXXX")" \
    || die "could not create a temporary download directory"
  installed_path="$bin_dir/$DLGT_BINARY_NAME"
  install_temp=""
  cleanup() {
    rm -rf "${temporary_directory:-}" 2>/dev/null || true
    if [ -n "${install_temp:-}" ]; then
      rm -f "$install_temp" 2>/dev/null || true
    fi
  }
  trap cleanup 0 1 2 15

  archive_path="$temporary_directory/$archive_name"
  checksum_path="${archive_path}.sha256"
  printf 'installing dlgt %s for %s\n' "$release_tag" "$target"
  download "$release_base/$archive_name" "$archive_path"
  download "$release_base/${archive_name}.sha256" "$checksum_path"
  verify_checksum "$archive_path" "$checksum_path"

  extract_directory="$temporary_directory/extract"
  mkdir -p "$extract_directory"
  archive_entries="$(tar -tzf "$archive_path")"
  [ "$archive_entries" = "$DLGT_BINARY_NAME" ] \
    || die "unexpected files in dlgt archive: $archive_name"
  tar -xzf "$archive_path" -C "$extract_directory" \
    || die "could not extract dlgt archive: $archive_name"
  [ -f "$extract_directory/$DLGT_BINARY_NAME" ] \
    || die "dlgt archive did not contain an executable"

  mkdir -p "$bin_dir" || die "could not create install directory: $bin_dir"
  install_temp="$(mktemp "$bin_dir/.dlgt.XXXXXX")" \
    || die "could not create a temporary binary in $bin_dir"
  cp "$extract_directory/$DLGT_BINARY_NAME" "$install_temp" \
    || die "could not copy dlgt into $bin_dir"
  chmod 755 "$install_temp"
  mv -f "$install_temp" "$installed_path" \
    || die "could not install dlgt into $bin_dir"
  install_temp=""
  printf 'installed dlgt at %s\n' "$installed_path"

  register_skills "$skill_mode"
  case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) printf 'add dlgt to future shells with: export PATH="%s:\$PATH"\n' "$bin_dir" ;;
  esac
}

if [ "$DLGT_INSTALLER_NO_MAIN" -eq 0 ]; then
  main "$@"
fi
