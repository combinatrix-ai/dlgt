# Development

- Build the `dlgt` binary with `cargo build --bin dlgt`.
- Run checks with `cargo fmt --check`, `cargo clippy --all-targets`, and `cargo test`.
- Keep the runtime as one Rust binary. Runtime assets such as the agent skill must be embedded and emitted by the binary.
- Prefer Codex and Claude lifecycle hooks over terminal-screen inference. PTY parsing is for presentation and fallback only.
- Backward compatibility is not required. Delete superseded paths instead of adding shims.
- Keep commits small and in English. Push each logical commit.
