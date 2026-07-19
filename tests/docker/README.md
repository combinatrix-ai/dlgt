# Docker installation E2E

The container persists both Harness homes in the gitignored
`.dlgt-e2e-home/` directory. Authenticate Codex and Claude once each:

~~~sh
docker compose -f compose.e2e.yaml run --rm agent codex login --device-auth
docker compose -f compose.e2e.yaml run --rm agent claude
~~~

Later runs reuse both credentials. Run the same short instruction published in
the project README:

~~~sh
docker compose -f compose.e2e.yaml run --rm agent \
  codex exec --dangerously-bypass-approvals-and-sandbox \
  "Install and verify dlgt. Fetch https://combinatrix.ai/dlgt/installation.md with curl and follow its instructions."
~~~

The E2E passes only when Codex installs the binary and both embedded skill
copies, launches Claude through dlgt, observes `DLGT_OK`, and stops the
verification Session.

`DLGT_HOME` points at `/tmp/dlgt` because Docker Desktop bind mounts do not
support the Unix-socket permission operation used by the daemon. Credentials,
Harness onboarding, and installed skills remain persistent under
`.dlgt-e2e-home/`; only disposable dlgt runtime state stays inside the
container.
