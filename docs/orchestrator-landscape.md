# Agent orchestrator landscape and dlgt positioning

Research snapshot: 2026-07-14.

## Executive summary

`dlgt` is best described as a **provider-aware local Session runtime**, not as
an end-to-end agent orchestrator.

It owns a Codex or Claude process, PTY, terminal screen, lifecycle state, and
retained result. It exposes those through a small JSON-first CLI and local JSONL
RPC. It deliberately does not decide what work should happen, decompose tasks,
route dependencies, isolate branches, merge code, or coordinate a team.

That boundary gives dlgt a distinct position among the surveyed tools:

- compared with **agmsg**, dlgt owns and controls processes while agmsg owns
  peer identity and communication;
- compared with **Agent Deck** and **OpenDray**, dlgt is an automation-facing
  control plane with provider-authoritative completion rather than primarily a
  human session dashboard or remote-access gateway;
- compared with **Agent Orchestrator** and **Gas City**, dlgt is a narrow
  runtime substrate rather than an opinionated software factory;
- compared with **LangGraph, CrewAI, and the OpenAI Agents SDK**, dlgt manages
  existing interactive coding-agent CLIs rather than constructing API-level
  agent graphs inside an application.

The strongest product direction is therefore not “add every orchestration
feature to dlgt.” It is: **make dlgt the reliable execution layer beneath an
orchestrator, and keep team/task policy in a separate layer.**

## The layers are different

The phrase “agent orchestrator” currently covers at least four different
products:

| Layer | Main object | Responsibility | Examples |
| --- | --- | --- | --- |
| Agent workflow framework | Agent, node, handoff, graph | Build an agent application from model/API calls | LangGraph, CrewAI, OpenAI Agents SDK |
| Coding workflow orchestrator | Task, issue, run, worktree, PR | Decompose and route work, enforce gates, recover and merge | Gas City, Agent Orchestrator, Overstory |
| Session runtime / manager | Process, PTY, Session | Keep coding CLIs alive, addressable, observable, and controllable | dlgt, Agent Deck, OpenDray |
| Coordination transport | Team, identity, message | Let independent agent sessions discover and communicate | agmsg |

dlgt sits in the third layer and provides a machine-facing contract that can
support the second layer. agmsg sits beside it rather than above or below it.

## Comparison matrix

Legend: “yes” means it is a first-class advertised capability, not merely
something a user could script externally.

| Project | Primary public object | Owns agent process | Durable semantic follow-up | Lifecycle source | Team mail | Task graph / routing | Worktree / merge / CI | Main operator surface |
| --- | --- | ---: | ---: | --- | ---: | ---: | ---: | --- |
| **dlgt** | Session | yes | yes | Codex app-server and Claude lifecycle hooks; PTY is presentation/fallback | no | no | no | JSON CLI, JSONL RPC, attach |
| **agmsg** | Team member and message | only optional peer spawn; no child ownership contract | message delivery to independent peers | hook or monitor delivery around a shared SQLite mailbox | yes | no | no | agent skill and shell scripts |
| **Agent Deck** | tmux-backed session | yes | interactive session control | UI-oriented status detection; no normalized lifecycle RPC contract documented | no; conductor is the coordination mechanism | conductor-driven, not a deterministic task graph | worktrees and optional Docker; no built-in PR factory | TUI, tmux, optional Telegram/Slack conductor |
| **OpenDray** | persistent remote session | yes | yes, including replies from remote channels | persistent PTY/session infrastructure | external chat channels, not peer-agent mail | no | no | web/mobile and six messaging channels |
| **Agent Orchestrator** | project session/worktree/PR | yes | yes | daemon combines agent data, process/terminal activity, and SCM state | centrally routed, not peer mail | issue/session orchestration and reactions | yes: worktrees, reviews, CI and merge feedback | desktop app plus daemon |
| **Gas City** | bead, formula, order, Session | yes through runtime providers | yes | controller reconciliation plus provider/runtime adapters | yes | yes: formulas, dependencies, loops, gates and policies | yes: project rigs, workers, judges/refinery | CLI, API, optional managed GUI |
| **Overstory** (archived) | task, agent role, orchestration run | yes | yes | runtime adapters, hooks, watchdogs and transcripts | yes, typed SQLite protocol | yes: coordinator hierarchy and task groups | yes: worktrees and tiered merge queue | web UI, TUI and CLI |

Provider breadth also differs. dlgt intentionally supports Codex and Claude.
agmsg accepts most CLI agents because its transport is tool-agnostic. Agent
Deck and OpenDray advertise several common coding CLIs. Agent Orchestrator
advertises 23 worker adapters. Gas City exposes multiple provider and runtime
backends. That breadth is useful, but it is not equivalent to dlgt's stronger
per-provider lifecycle contract.

## Direct comparison with agmsg

agmsg is the most useful comparison because the two projects are complementary
and superficially easy to confuse.

| Concern | agmsg | dlgt |
| --- | --- | --- |
| Core promise | Stop humans copy-pasting messages between agents | Keep a coding-agent Session alive, addressable, observable, and controllable |
| Identity | Team + named peer | Immutable Session ID + active human alias |
| Transport | Shared WAL-mode SQLite mailbox | Local mode-0600 Unix socket and JSONL RPC |
| Process ownership | `spawn` launches an independent peer; agmsg explicitly does not manage it as a child | Daemon owns the harness process group, PTY, screen, controller, cancellation and stop |
| Completion | A message or delivery event is coordination state, not proof that provider work completed | Official provider lifecycle terminalizes a retained execution result |
| Input | Agent-to-agent messages surfaced by hooks/monitor modes | Semantic `send` to an idle Session; Codex uses app-server `turn/start`, Claude uses guarded terminal input |
| Concurrency | Multiple peers communicate through the shared floor | One controller and at most one active execution per Session; busy work is rejected, never queued |
| History | Durable room/message history and replay | Session metadata, events, results, input audit, rendered scrollback and bounded raw PTY history |
| Best role | Coordination plane | Execution plane |

A clean composition is:

```text
leader / task orchestrator
        |
        | chooses workers and work
        v
agmsg ---------------------- team identity, peer mail, handoff
        |
        | addresses owned workers
        v
dlgt ---------------------- Session lifecycle, semantic send, wait, cancel
        |
        v
Codex app-server / Claude CLI + hooks
```

Neither project needs to absorb the other. A thin adapter can register an
dlgt Session as an agmsg peer, correlate `Session ID <-> team identity`, and
turn normalized dlgt lifecycle events into coordination messages.

## What is genuinely differentiated in dlgt

### 1. Provider-authoritative completion

Many session managers can keep a tmux pane alive and show terminal output.
dlgt instead treats Codex app-server notifications and Claude lifecycle hooks
as authoritative. PTY silence, spinner disappearance, and screen text do not
prove completion. Loss of the authoritative transport fails closed.

Among the surveyed projects, this combination of an attachable real PTY with a
small, normalized, provider-backed lifecycle API is dlgt's clearest technical
wedge.

### 2. A deliberately small public state model

The Session is the only public runtime object. Provider turn IDs remain
private; `execution_seq` is correlation rather than another resource. This is
smaller than the task/agent/run/worktree/merge object graphs in full
orchestrators and makes controller behavior easier to reason about.

### 3. Explicit backpressure instead of a hidden queue

An idle Session accepts one execution. A busy Session returns `SESSION_BUSY`.
The controller owns sequencing and timeouts; a wait timeout observes without
canceling. This is a useful substrate contract because an upper-level
orchestrator can implement its own dependency and retry policy without
discovering stale prompts hidden in a runtime queue.

### 4. Machine-first local control with a human escape hatch

Control-plane commands emit deterministic JSON by default, and the public RPC
surface is allowlisted. At the same time, the same owned PTY supports rendered
scrollback and exclusive live attach. Competitors tend to optimize either for
a human dashboard/terminal or for headless workflow automation; dlgt keeps the
boundary between those surfaces unusually explicit.

### 5. Minimal deployment and local data ownership

dlgt is one Rust binary with an embedded agent skill, versioned local daemons,
and a Unix socket. It does not require tmux, Node/Bun, a web application,
Postgres, a task tracker, or a repository layout.

## What dlgt does not provide

These are gaps only if the desired product is a full orchestrator. They are not
defects in the current Session-runtime boundary.

1. **Durable work objects**  -  tasks, dependencies, runs, acceptance criteria,
   retry policy, and budgets.
2. **Team coordination**  -  stable agent roles, peer discovery, mailboxes,
   broadcast, escalation, and handoff.
3. **Workspace isolation**  -  branch/worktree/clone/container allocation and
   collision policy.
4. **Outcome integration**  -  test gates, CI feedback, review comments, PRs,
   merge queues, and conflict recovery.
5. **Fleet policy**  -  concurrency limits, provider quotas, scheduling,
   capability matching, and cost accounting.
6. **Remote and visual operations**  -  browser/mobile UI, notifications, and
   multi-host transport.
7. **Broader harness coverage**  -  Gemini, OpenCode, Cursor, Aider, and others.

Adding all seven directly to dlgt would erase its simplest advantage. They fit
better in a separate controller that uses `session.*`, `event.subscribe`, and
the structured error contract.

## Recommended positioning

Suggested one-line category:

> **dlgt is the local process and lifecycle substrate for orchestrating Codex
> and Claude Sessions.**

Suggested longer description:

> dlgt gives an orchestrator live, addressable Codex and Claude Sessions
> with semantic input, provider-authoritative completion, structured events,
> bounded waits, cancellation, scrollback, and live attach - without imposing a
> task graph, worktree policy, queue, or UI.

This avoids competing with mature products on their broadest surface. It also
makes dlgt useful to more than one orchestration style: a shell script, agmsg
team, bespoke leader agent, TUI, CI bot, or future workflow engine can all use
the same runtime contract.

## Recommended next moves

### Keep inside dlgt

- harden Codex and Claude lifecycle conformance and recovery;
- stabilize/version the JSONL RPC and event schemas;
- make blocked-input and provider-retry behavior easy for controllers to test;
- add conformance fixtures for any future harness before advertising support;
- document adapter examples for `events --follow` and `rpc --stdio`;
- expose enough resource/usage metadata for an outer scheduler without adding
  scheduling policy to the daemon.

### Build beside dlgt

- an **dlgt + agmsg bridge** for Session/team identity and completion notices;
- a small reference controller demonstrating fan-out, bounded wait, failure
  collection, and explicit cleanup;
- optional worktree allocation as controller policy, not Session semantics;
- a dashboard that consumes public RPC/events instead of becoming a private
  daemon API;
- if needed, a separate orchestration project whose public objects are `Task`,
  `Run`, `Worker`, and `Workspace`, with dlgt Session IDs as runtime handles.

### Avoid in the runtime

- server-side prompt queues;
- implicit task decomposition or automatic retries;
- hard-coded worker roles;
- repository or branch ownership assumptions;
- PR/CI provider logic;
- arbitrary daemon-executed hooks.

Those policies belong to the controller and would weaken the current explicit
ownership and failure model.

## Surveyed projects and primary sources

- [agmsg README](https://github.com/fujibee/agmsg)  -  peer messaging through a
  shared local SQLite database; explicitly not a subagent manager or broker.
- [Agent Deck README](https://github.com/asheshgoplani/agent-deck)  -  tmux-backed
  TUI session management, worktrees, Docker sandboxes, forks, and conductor
  agents.
- [OpenDray](https://opendray.dev/) and
  [repository](https://github.com/Opendray/opendray)  -  self-hosted persistent
  PTYs, web/mobile access, remote messaging channels, and local-first memory.
- [Agent Orchestrator README](https://github.com/AgentWrapper/agent-orchestrator)
   -  desktop meta-harness with isolated worktrees, terminal supervision, PR
  state, reviews, and automatic CI/review feedback loops.
- [Gas City README](https://github.com/gastownhall/gascity) and
  [product overview](https://gascity.com/)  -  configurable multi-agent software
  factory with runtime providers, work routing, formulas, health patrol, gates,
  and controller reconciliation.
- [Gas Town README](https://github.com/gastownhall/gastown)  -  the earlier,
  opinionated workspace/factory model built around Mayor, Beads, convoys,
  workers, watchdogs, and a merge refinery.
- [Overstory README](https://github.com/jayminwest/overstory)  -  useful prior art
  for a coordinator, typed SQLite mail, worktrees, watchdogs, and tiered merging;
  the repository now states that it is archived and points new development to
  Warren.
- [LangGraph workflows and agents](https://docs.langchain.com/oss/python/langgraph/workflows-agents),
  [CrewAI documentation](https://docs.crewai.com/), and
  [OpenAI Agents SDK orchestration](https://openai.github.io/openai-agents-js/guides/multi-agent/)
   -  API-level workflow frameworks, included to clarify the category boundary
  rather than as direct CLI-runtime competitors.

## Research caveats

- The ecosystem changes quickly; this is a dated snapshot, not a permanent
  market map.
- Claims describe documented public contracts, not exhaustive code audits of
  every project.
- “Provider-authoritative” is used narrowly: completion is derived from the
  provider's lifecycle interface rather than inferred only from terminal text.
- Feature breadth and lifecycle reliability are separate axes. Supporting more
  agent names does not by itself imply stronger readiness/completion semantics.
