//! The Session lifecycle as one pure transition function.
//!
//! Every change to a Session's phase goes through [`Lifecycle::apply`]. An
//! event names the provider generation it comes from, and execution events
//! also name their execution sequence, so an event from a replaced process or
//! a finished execution is refused here instead of being checked, or not, at
//! each call site. `apply` returns the effects the store must carry out and
//! the lifecycle events it must record; it never touches anything itself.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::SessionState;

pub type Generation = u64;

/// Generations are never reused within a daemon, across Sessions, restarts,
/// or a resume that recreates a Session: an event from any earlier process
/// can never be mistaken for the current one's.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn fresh_generation() -> Generation {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Lifecycle {
    generation: Generation,
    phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Phase {
    /// A provider process for this generation is launching.
    Starting {
        spawned: bool,
    },
    Idle,
    Active(Execution),
    /// The previous generation's process is being replaced.
    Restarting {
        old_alive: bool,
    },
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Execution {
    pub turn_id: String,
    pub seq: i64,
    pub acknowledged: bool,
    pub blocked: bool,
    pub cancel: Option<CancelProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CancelProgress {
    pub pending_controls: usize,
    pub provider_finished: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<'a> {
    /// The provider process for this generation exists.
    Spawned,
    /// The provider process is ready for prompts.
    Ready,
    /// A prompt was accepted. `acknowledged` is true when the provider itself
    /// started the execution (an unsolicited Codex `turn/started`).
    Submit {
        turn_id: &'a str,
        seq: i64,
        acknowledged: bool,
    },
    Started {
        seq: i64,
    },
    Blocked {
        seq: i64,
    },
    Unblocked {
        seq: i64,
    },
    /// The provider reported the execution's terminal result.
    Finished {
        seq: i64,
        outcome: Outcome,
    },
    /// The caller canceled; `controls` provider controls will be dispatched.
    Cancel {
        seq: i64,
        controls: usize,
    },
    /// One more cancel control (for example a deferred interrupt) will be sent.
    ControlReserved {
        seq: i64,
    },
    ControlDone {
        seq: i64,
    },
    /// The provider or its transport failed. `seq`, when given, must be the
    /// active execution's.
    Fail {
        seq: Option<i64>,
    },
    /// The provider process for this generation exited.
    Exited,
    Stop {
        force: bool,
    },
    Restart,
    /// The replacement process is about to launch under the next generation.
    RestartLaunch,
    RestartTimeout,
}

/// What the store must do because a transition happened, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Record a lifecycle event of this kind against the Session, attached to
    /// `turn_id` when the event concerns an execution.
    Record {
        kind: &'static str,
        turn_id: Option<String>,
    },
    /// Close every pending Request as canceled.
    CloseRequests,
    /// Terminalize the active execution's result because its process is going
    /// away without reporting one.
    EndTurn {
        turn_id: String,
        outcome: Outcome,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The event came from a generation or execution that no longer owns the
    /// Session.
    Stale,
    Busy,
    Blocked,
    NotRunning,
    NoActiveExecution,
    Unavailable(SessionState),
}

impl Refusal {
    /// The public error for a refused caller operation on `session_id`.
    pub fn into_error(self, session_id: &str) -> anyhow::Error {
        use crate::error::{Code, coded};
        match self {
            Self::Stale => coded(
                Code::SessionUnavailable,
                format!(
                    "Session {session_id} changed state or was restarted by another request; check it before retrying"
                ),
            ),
            Self::Busy => coded(Code::SessionBusy, "session already has an active turn"),
            Self::Blocked => coded(Code::SessionBlocked, "session blocked on input"),
            Self::NotRunning => coded(
                Code::SessionNotRunning,
                format!(
                    "Session {session_id} is not running; retry with --resume using the same Session ID"
                ),
            ),
            Self::NoActiveExecution => coded(
                Code::NoActiveExecution,
                format!("Session {session_id} has no running execution"),
            ),
            Self::Unavailable(state) => coded(
                Code::SessionUnavailable,
                format!("session is unavailable in state {}", state.public_name()),
            ),
        }
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            generation: fresh_generation(),
            phase: Phase::Starting { spawned: false },
        }
    }

    pub const fn generation(&self) -> Generation {
        self.generation
    }

    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn active(&self) -> Option<&Execution> {
        match &self.phase {
            Phase::Active(execution) => Some(execution),
            _ => None,
        }
    }

    /// The public state this lifecycle presents.
    pub const fn public_state(&self) -> SessionState {
        match &self.phase {
            Phase::Starting { spawned: false } => SessionState::Starting,
            Phase::Starting { spawned: true } => SessionState::Running,
            Phase::Idle => SessionState::Idle,
            Phase::Active(Execution {
                cancel: Some(_), ..
            }) => SessionState::Quiescing,
            Phase::Active(Execution { blocked: true, .. }) => SessionState::Blocked,
            Phase::Active(_) => SessionState::Busy,
            Phase::Restarting { .. } => SessionState::Restarting,
            Phase::Stopping => SessionState::Stopping,
            Phase::Stopped => SessionState::Stopped,
            Phase::Failed => SessionState::Failed,
        }
    }

    /// Apply one event from `generation`. On refusal nothing changes.
    pub fn apply(
        &mut self,
        generation: Generation,
        event: &Event<'_>,
    ) -> Result<Vec<Effect>, Refusal> {
        if generation != self.generation {
            return Err(Refusal::Stale);
        }
        let (phase, effects) = self.next(event)?;
        if let Some(phase) = phase {
            if matches!(event, Event::RestartLaunch) {
                self.generation = fresh_generation();
            }
            self.phase = phase;
        }
        Ok(effects)
    }

    fn refuse_for_phase(&self) -> Refusal {
        match &self.phase {
            Phase::Active(Execution { blocked: true, .. }) => Refusal::Blocked,
            Phase::Active(_) => Refusal::Busy,
            Phase::Stopped | Phase::Failed => Refusal::NotRunning,
            _ => Refusal::Unavailable(self.public_state()),
        }
    }

    /// The execution `seq` names, or `Stale` when it is not the active one.
    fn owned(&self, seq: i64) -> Result<&Execution, Refusal> {
        match &self.phase {
            Phase::Active(execution) if execution.seq == seq => Ok(execution),
            _ => Err(Refusal::Stale),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn next(&self, event: &Event<'_>) -> Result<(Option<Phase>, Vec<Effect>), Refusal> {
        let record = |kind: &'static str, turn_id: Option<&str>| Effect::Record {
            kind,
            turn_id: turn_id.map(str::to_owned),
        };
        let unchanged = || Ok((None, Vec::new()));
        match event {
            Event::Spawned => match self.phase {
                Phase::Starting { spawned: false } => Ok((
                    Some(Phase::Starting { spawned: true }),
                    vec![record("session.started", None)],
                )),
                _ => Err(self.refuse_for_phase()),
            },
            Event::Ready => match self.phase {
                Phase::Starting { .. } => {
                    Ok((Some(Phase::Idle), vec![record("session.ready", None)]))
                }
                _ => Err(self.refuse_for_phase()),
            },
            Event::Submit {
                turn_id,
                seq,
                acknowledged,
            } => match self.phase {
                Phase::Idle => {
                    let mut effects = vec![record("turn.submitted", Some(turn_id))];
                    if *acknowledged {
                        effects.push(record("turn.started", Some(turn_id)));
                    }
                    Ok((
                        Some(Phase::Active(Execution {
                            turn_id: (*turn_id).to_owned(),
                            seq: *seq,
                            acknowledged: *acknowledged,
                            blocked: false,
                            cancel: None,
                        })),
                        effects,
                    ))
                }
                _ => Err(self.refuse_for_phase()),
            },
            Event::Started { seq } => {
                let execution = self.owned(*seq)?;
                if execution.acknowledged {
                    return unchanged();
                }
                let turn_id = execution.turn_id.clone();
                Ok((
                    Some(Phase::Active(Execution {
                        acknowledged: true,
                        ..execution.clone()
                    })),
                    vec![record("turn.started", Some(&turn_id))],
                ))
            }
            Event::Blocked { seq } => {
                let execution = self.owned(*seq)?;
                if execution.cancel.is_some() {
                    return Err(Refusal::Unavailable(self.public_state()));
                }
                if execution.blocked {
                    return unchanged();
                }
                Ok((
                    Some(Phase::Active(Execution {
                        blocked: true,
                        ..execution.clone()
                    })),
                    vec![record("session.blocked", Some(&execution.turn_id))],
                ))
            }
            Event::Unblocked { seq } => {
                let execution = self.owned(*seq)?;
                if !execution.blocked {
                    return unchanged();
                }
                Ok((
                    Some(Phase::Active(Execution {
                        blocked: false,
                        ..execution.clone()
                    })),
                    vec![record("session.resumed", Some(&execution.turn_id))],
                ))
            }
            Event::Finished { seq, outcome } => {
                let execution = self.owned(*seq)?;
                let turn_id = execution.turn_id.as_str();
                match execution.cancel {
                    // Requests the finished execution left open can no
                    // longer be answered.
                    None => Ok((
                        Some(Phase::Idle),
                        vec![
                            Effect::CloseRequests,
                            record(outcome_kind(*outcome), Some(turn_id)),
                        ],
                    )),
                    Some(cancel) if cancel.pending_controls == 0 => Ok((
                        Some(Phase::Idle),
                        vec![record("provider.quiesced", Some(turn_id))],
                    )),
                    Some(cancel) => Ok((
                        Some(Phase::Active(Execution {
                            cancel: Some(CancelProgress {
                                provider_finished: true,
                                ..cancel
                            }),
                            ..execution.clone()
                        })),
                        Vec::new(),
                    )),
                }
            }
            Event::Cancel { seq, controls } => {
                let Phase::Active(execution) = &self.phase else {
                    return Err(Refusal::NoActiveExecution);
                };
                if execution.seq != *seq {
                    return Err(Refusal::Stale);
                }
                if execution.cancel.is_some() {
                    return unchanged();
                }
                Ok((
                    Some(Phase::Active(Execution {
                        blocked: false,
                        cancel: Some(CancelProgress {
                            pending_controls: *controls,
                            provider_finished: false,
                        }),
                        ..execution.clone()
                    })),
                    vec![
                        Effect::CloseRequests,
                        record("turn.canceled", Some(&execution.turn_id)),
                    ],
                ))
            }
            Event::ControlReserved { seq } => {
                let execution = self.owned(*seq)?;
                let Some(cancel) = execution.cancel else {
                    return Err(Refusal::NoActiveExecution);
                };
                Ok((
                    Some(Phase::Active(Execution {
                        cancel: Some(CancelProgress {
                            pending_controls: cancel.pending_controls + 1,
                            ..cancel
                        }),
                        ..execution.clone()
                    })),
                    Vec::new(),
                ))
            }
            Event::ControlDone { seq } => {
                let execution = self.owned(*seq)?;
                let Some(cancel) = execution.cancel else {
                    return Err(Refusal::NoActiveExecution);
                };
                // Every control finishes once; an extra report is stale.
                if cancel.pending_controls == 0 {
                    return Err(Refusal::Stale);
                }
                let pending_controls = cancel.pending_controls - 1;
                if pending_controls == 0 && cancel.provider_finished {
                    return Ok((
                        Some(Phase::Idle),
                        vec![record("provider.quiesced", Some(&execution.turn_id))],
                    ));
                }
                Ok((
                    Some(Phase::Active(Execution {
                        cancel: Some(CancelProgress {
                            pending_controls,
                            ..cancel
                        }),
                        ..execution.clone()
                    })),
                    Vec::new(),
                ))
            }
            Event::Fail { seq } => {
                if let (Some(seq), Some(execution)) = (seq, self.active())
                    && execution.seq != *seq
                {
                    return Err(Refusal::Stale);
                }
                if seq.is_some() && self.active().is_none() {
                    return Err(Refusal::Stale);
                }
                match &self.phase {
                    Phase::Starting { .. } | Phase::Idle | Phase::Active(_) => Ok((
                        Some(Phase::Failed),
                        self.ending(Outcome::Failed, "provider_error", "session.failed"),
                    )),
                    _ => Err(self.refuse_for_phase()),
                }
            }
            Event::Exited => match &self.phase {
                Phase::Stopping => {
                    Ok((Some(Phase::Stopped), vec![record("session.stopped", None)]))
                }
                Phase::Restarting { .. } => {
                    Ok((Some(Phase::Restarting { old_alive: false }), Vec::new()))
                }
                Phase::Starting { .. } | Phase::Idle | Phase::Active(_) => Ok((
                    Some(Phase::Failed),
                    self.ending(Outcome::Failed, "provider_exited", "session.failed"),
                )),
                Phase::Stopped | Phase::Failed => unchanged(),
            },
            Event::Stop { force } => match &self.phase {
                Phase::Starting { .. }
                | Phase::Idle
                | Phase::Active(_)
                | Phase::Restarting { .. } => Ok((
                    Some(Phase::Stopping),
                    self.ending(Outcome::Interrupted, "session_stopped", "session.stopping"),
                )),
                Phase::Stopping if *force => unchanged(),
                _ => Err(self.refuse_for_phase()),
            },
            Event::Restart => {
                let old_alive = match &self.phase {
                    Phase::Idle | Phase::Active(_) => true,
                    Phase::Stopped | Phase::Failed => false,
                    _ => return Err(self.refuse_for_phase()),
                };
                Ok((
                    Some(Phase::Restarting { old_alive }),
                    self.ending(
                        Outcome::Interrupted,
                        "session_restarted",
                        "session.restarting",
                    ),
                ))
            }
            Event::RestartLaunch => match self.phase {
                Phase::Restarting { old_alive: false } => {
                    Ok((Some(Phase::Starting { spawned: false }), Vec::new()))
                }
                _ => Err(self.refuse_for_phase()),
            },
            Event::RestartTimeout => match self.phase {
                Phase::Restarting { .. } => {
                    Ok((Some(Phase::Failed), vec![record("session.failed", None)]))
                }
                _ => Err(self.refuse_for_phase()),
            },
        }
    }

    /// Effects for leaving the live phases: the active execution, if any, is
    /// terminalized, pending Requests close, and `kind` is recorded.
    fn ending(&self, outcome: Outcome, reason: &'static str, kind: &'static str) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(execution) = self.active() {
            effects.push(Effect::EndTurn {
                turn_id: execution.turn_id.clone(),
                outcome,
                reason,
            });
            effects.push(Effect::Record {
                kind: outcome_kind(outcome),
                turn_id: Some(execution.turn_id.clone()),
            });
        }
        effects.push(Effect::CloseRequests);
        effects.push(Effect::Record {
            kind,
            turn_id: None,
        });
        effects
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

const fn outcome_kind(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "turn.completed",
        Outcome::Failed => "turn.failed",
        Outcome::Interrupted => "turn.interrupted",
    }
}

/// The public event a recorded lifecycle kind is published as, if any.
pub const fn public_event(kind: &str) -> Option<&'static str> {
    Some(match kind.as_bytes() {
        b"session.created" => "session.created",
        b"session.restarting" => "session.restarting",
        b"session.ready" => "session.ready",
        b"turn.started" => "session.busy",
        b"session.blocked" => "session.blocked",
        b"session.resumed" => "session.resumed",
        b"turn.canceled" => "session.canceling",
        b"turn.completed" | b"turn.failed" | b"turn.interrupted" | b"provider.quiesced" => {
            "session.idle"
        }
        b"session.stopping" => "session.stopping",
        b"session.stopped" => "session.stopped",
        b"session.failed" => "session.failed",
        b"provider.retrying" | b"provider.error.retrying" => "provider.retrying",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};

    use super::{
        CancelProgress, Effect, Event, Execution, Lifecycle, Outcome, Phase, Refusal, public_event,
    };
    use crate::protocol::SessionState;

    /// The public event that must accompany entering each public state.
    fn announces(state: SessionState) -> &'static [&'static str] {
        match state {
            SessionState::Idle => &["session.ready", "session.idle"],
            SessionState::Busy => &["session.busy", "session.resumed"],
            SessionState::Blocked => &["session.blocked"],
            SessionState::Quiescing => &["session.canceling"],
            SessionState::Restarting => &["session.restarting"],
            SessionState::Stopping => &["session.stopping"],
            SessionState::Stopped => &["session.stopped"],
            SessionState::Failed => &["session.failed"],
            // Launching has no public event of its own; `session.created`
            // and `session.restarting` precede it.
            SessionState::Starting | SessionState::Running => &[],
        }
    }

    fn recorded(effects: &[Effect]) -> Vec<&'static str> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Record { kind, .. } => public_event(kind),
                _ => None,
            })
            .collect()
    }

    /// Every event worth trying from `state`: from the current generation and
    /// the one before it, naming the active execution, an earlier one, and the
    /// next one. `next_seq` is the sequence the next prompt gets.
    fn alphabet(state: &Lifecycle, replaced: u64, next_seq: i64) -> Vec<(u64, Event<'static>)> {
        let generation = state.generation();
        let mut seqs = vec![next_seq - 1, next_seq - 2, next_seq];
        seqs.retain(|seq| *seq > 0);
        let mut events = Vec::new();
        for generation in [generation, replaced] {
            events.push((generation, Event::Spawned));
            events.push((generation, Event::Ready));
            for acknowledged in [false, true] {
                events.push((
                    generation,
                    Event::Submit {
                        turn_id: "t",
                        seq: next_seq,
                        acknowledged,
                    },
                ));
            }
            for &seq in &seqs {
                events.push((generation, Event::Started { seq }));
                events.push((generation, Event::Blocked { seq }));
                events.push((generation, Event::Unblocked { seq }));
                for outcome in [Outcome::Completed, Outcome::Failed, Outcome::Interrupted] {
                    events.push((generation, Event::Finished { seq, outcome }));
                }
                events.push((generation, Event::Cancel { seq, controls: 1 }));
                events.push((generation, Event::Cancel { seq, controls: 2 }));
                events.push((generation, Event::ControlReserved { seq }));
                events.push((generation, Event::ControlDone { seq }));
                events.push((generation, Event::Fail { seq: Some(seq) }));
            }
            events.push((generation, Event::Fail { seq: None }));
            events.push((generation, Event::Exited));
            events.push((generation, Event::Stop { force: false }));
            events.push((generation, Event::Stop { force: true }));
            events.push((generation, Event::Restart));
            events.push((generation, Event::RestartLaunch));
            events.push((generation, Event::RestartTimeout));
        }
        events
    }

    /// Whether an event from the current generation that names the active
    /// execution, if it names one, must be accepted. This is the transition
    /// table restated, so a change to `next` that accepts or refuses the
    /// wrong event fails here.
    fn must_accept(state: &Lifecycle, event: &Event<'_>) -> bool {
        let phase = state.phase();
        let active = state.active();
        let canceling = active.and_then(|execution| execution.cancel);
        match event {
            Event::Spawned => matches!(phase, Phase::Starting { spawned: false }),
            Event::Ready => matches!(phase, Phase::Starting { .. }),
            Event::Submit { .. } => matches!(phase, Phase::Idle),
            Event::Started { .. }
            | Event::Unblocked { .. }
            | Event::Finished { .. }
            | Event::Cancel { .. } => active.is_some(),
            Event::Blocked { .. } => active.is_some() && canceling.is_none(),
            Event::ControlReserved { .. } => canceling.is_some(),
            Event::ControlDone { .. } => {
                canceling.is_some_and(|cancel| cancel.pending_controls > 0)
            }
            Event::Fail { .. } => matches!(
                phase,
                Phase::Starting { .. } | Phase::Idle | Phase::Active(_)
            ),
            Event::Exited => true,
            Event::Stop { force } => {
                matches!(
                    phase,
                    Phase::Starting { .. }
                        | Phase::Idle
                        | Phase::Active(_)
                        | Phase::Restarting { .. }
                ) || (*force && matches!(phase, Phase::Stopping))
            }
            Event::Restart => matches!(
                phase,
                Phase::Idle | Phase::Active(_) | Phase::Stopped | Phase::Failed
            ),
            Event::RestartLaunch => matches!(phase, Phase::Restarting { old_alive: false }),
            Event::RestartTimeout => matches!(phase, Phase::Restarting { .. }),
        }
    }

    /// The execution sequence an event names, if any.
    const fn named_seq(event: &Event<'_>) -> Option<i64> {
        match event {
            Event::Started { seq }
            | Event::Blocked { seq }
            | Event::Unblocked { seq }
            | Event::Finished { seq, .. }
            | Event::Cancel { seq, .. }
            | Event::ControlReserved { seq }
            | Event::ControlDone { seq }
            | Event::Fail { seq: Some(seq) } => Some(*seq),
            _ => None,
        }
    }

    /// Whether events no caller sends reach a resting state. An execution
    /// must settle through its own provider events, canceled or not; process
    /// exit is not allowed to stand in for a broken cancel.
    fn settles(state: &Lifecycle) -> bool {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([state.clone()]);
        while let Some(state) = queue.pop_front() {
            if matches!(state.phase(), Phase::Idle | Phase::Stopped | Phase::Failed) {
                return true;
            }
            if !seen.insert(state.clone()) || seen.len() > 64 {
                continue;
            }
            let generation = state.generation();
            let provider_events = match state.active() {
                Some(execution) => {
                    // The provider reports an execution's end once; a search
                    // that could report it again would hide a cancel that
                    // fails to settle after the real report.
                    let mut events = Vec::new();
                    if !execution
                        .cancel
                        .is_some_and(|cancel| cancel.provider_finished)
                    {
                        events.push(Event::Finished {
                            seq: execution.seq,
                            outcome: Outcome::Interrupted,
                        });
                    }
                    if execution
                        .cancel
                        .is_some_and(|cancel| cancel.pending_controls > 0)
                    {
                        events.push(Event::ControlDone { seq: execution.seq });
                    }
                    events
                }
                None => vec![
                    Event::Spawned,
                    Event::Ready,
                    Event::Exited,
                    Event::RestartLaunch,
                ],
            };
            for event in provider_events {
                let mut next = state.clone();
                if next.apply(generation, &event).is_ok() {
                    queue.push_back(next);
                }
            }
        }
        false
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_interleaving_keeps_the_lifecycle_invariants() {
        let mut seen = HashSet::new();
        // A generation no live process has: events from it are stale.
        let mut queue = VecDeque::from([(Lifecycle::new(), 0_u64, 0_usize, 1_i64, 0_usize)]);
        let mut transitions = 0_usize;
        let mut sequences = HashSet::new();
        while let Some((state, stale, restarts, next_seq, depth)) = queue.pop_front() {
            if !seen.insert((state.clone(), next_seq)) {
                continue;
            }
            assert!(settles(&state), "dead end: {state:?}");
            if let Some(execution) = state.active() {
                sequences.insert(execution.seq);
            }
            if depth == 12 || restarts > 2 || next_seq > 4 {
                continue;
            }
            for (generation, event) in alphabet(&state, stale, next_seq) {
                let mut next = state.clone();
                let result = next.apply(generation, &event);
                let names_other = named_seq(&event)
                    .is_some_and(|seq| state.active().is_none_or(|active| active.seq != seq));
                if generation == state.generation() && !names_other {
                    assert_eq!(
                        result.is_ok(),
                        must_accept(&state, &event),
                        "{event:?} on {state:?} gave {result:?}"
                    );
                }
                if generation != state.generation() || names_other {
                    assert!(
                        result.is_err(),
                        "{event:?} from gen {generation} applied to {state:?}"
                    );
                }
                let Ok(effects) = result else {
                    assert_eq!(next, state, "a refused {event:?} changed {state:?}");
                    let other_active = named_seq(&event)
                        .is_some_and(|seq| state.active().is_some_and(|active| active.seq != seq));
                    if generation != state.generation() || other_active {
                        assert_eq!(result, Err(Refusal::Stale), "{event:?} on {state:?}");
                    }
                    continue;
                };
                transitions += 1;

                let before = state.public_state();
                let after = next.public_state();
                let published = recorded(&effects);
                if let (Event::Started { .. }, Some(execution)) = (&event, state.active())
                    && !execution.acknowledged
                {
                    assert_eq!(published, ["session.busy"], "{event:?} on {state:?}");
                }
                if before != after {
                    let expected = announces(after);
                    let silent_submit = matches!(
                        event,
                        Event::Submit {
                            acknowledged: false,
                            ..
                        }
                    );
                    assert!(
                        expected.is_empty()
                            || silent_submit
                            || published.iter().any(|kind| expected.contains(kind)),
                        "{before:?} -> {after:?} on {event:?} published {published:?}"
                    );
                }
                let mut unique = published.clone();
                unique.dedup();
                assert_eq!(
                    published.len(),
                    unique.len(),
                    "{event:?} published {published:?} twice"
                );

                if let Some(execution) = state.active()
                    && next
                        .active()
                        .is_none_or(|active| active.seq != execution.seq)
                {
                    let settled_by_provider =
                        matches!(event, Event::Finished { .. } | Event::ControlDone { .. });
                    let ended = effects.iter().any(|effect| {
                        matches!(effect, Effect::EndTurn { turn_id, .. } if *turn_id == execution.turn_id)
                    });
                    assert!(
                        settled_by_provider || ended,
                        "execution {execution:?} left without a result on {event:?}"
                    );
                    assert!(
                        execution.cancel.is_some() || effects.contains(&Effect::CloseRequests),
                        "execution {execution:?} left on {event:?} with its Requests open"
                    );
                }
                if let Some(execution) = next.active() {
                    assert!(
                        !(execution.blocked && execution.cancel.is_some()),
                        "blocked while canceling: {next:?}"
                    );
                }
                let next_seq = if matches!(event, Event::Submit { .. }) {
                    next_seq + 1
                } else {
                    next_seq
                };
                let (stale, restarts) = if next.generation() == state.generation() {
                    (stale, restarts)
                } else {
                    (state.generation(), restarts + 1)
                };
                queue.push_back((next, stale, restarts, next_seq, depth + 1));
            }
        }
        assert!(
            sequences.len() >= 3,
            "explored executions {sequences:?} only"
        );
        assert!(
            seen.len() > 100 && transitions > 1_000,
            "explored only {} states and {transitions} transitions",
            seen.len()
        );
    }

    #[test]
    fn a_cancel_settles_in_either_order_and_announces_idle_once() {
        for terminal_first in [true, false] {
            let mut state = Lifecycle::new();
            let generation = state.generation();
            state.apply(generation, &Event::Spawned).unwrap_or_default();
            state.apply(generation, &Event::Ready).unwrap_or_default();
            state
                .apply(
                    generation,
                    &Event::Submit {
                        turn_id: "t1",
                        seq: 1,
                        acknowledged: true,
                    },
                )
                .unwrap_or_default();
            state
                .apply(
                    generation,
                    &Event::Cancel {
                        seq: 1,
                        controls: 1,
                    },
                )
                .unwrap_or_default();
            let order = if terminal_first {
                [
                    Event::Finished {
                        seq: 1,
                        outcome: Outcome::Interrupted,
                    },
                    Event::ControlDone { seq: 1 },
                ]
            } else {
                [
                    Event::ControlDone { seq: 1 },
                    Event::Finished {
                        seq: 1,
                        outcome: Outcome::Interrupted,
                    },
                ]
            };
            let mut published = Vec::new();
            for event in &order {
                let effects = state
                    .apply(generation, event)
                    .unwrap_or_else(|refusal| panic!("{event:?} refused: {refusal:?}"));
                published.extend(recorded(&effects));
            }
            assert_eq!(state.phase(), &Phase::Idle);
            assert_eq!(
                published,
                ["session.idle"],
                "terminal_first={terminal_first}"
            );
        }
    }

    #[test]
    fn a_rejected_start_during_cancel_fails_the_session_with_a_result() {
        let mut state = Lifecycle::new();
        let generation = state.generation();
        for event in [
            Event::Spawned,
            Event::Ready,
            Event::Submit {
                turn_id: "t1",
                seq: 1,
                acknowledged: false,
            },
            Event::Cancel {
                seq: 1,
                controls: 1,
            },
        ] {
            state
                .apply(generation, &event)
                .unwrap_or_else(|refusal| panic!("{event:?}: {refusal:?}"));
        }
        let effects = state
            .apply(generation, &Event::Fail { seq: Some(1) })
            .unwrap_or_else(|refusal| panic!("{refusal:?}"));
        assert_eq!(state.phase(), &Phase::Failed);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EndTurn { turn_id, outcome: Outcome::Failed, .. } if turn_id == "t1"
        )));
    }

    #[test]
    fn stop_and_restart_from_a_replaced_generation_are_refused() {
        let mut state = Lifecycle::new();
        let old = state.generation();
        for event in [
            Event::Spawned,
            Event::Ready,
            Event::Restart,
            Event::Exited,
            Event::RestartLaunch,
        ] {
            state
                .apply(state.generation(), &event)
                .unwrap_or_else(|refusal| panic!("{event:?}: {refusal:?}"));
        }
        assert_ne!(state.generation(), old);
        for event in [
            Event::Stop { force: false },
            Event::Restart,
            Event::Exited,
            Event::Fail { seq: None },
        ] {
            let before = state.clone();
            assert_eq!(state.apply(old, &event), Err(Refusal::Stale));
            assert_eq!(state, before);
        }
        let current = state.generation();
        state.apply(current, &Event::Ready).unwrap_or_default();
        state
            .apply(current, &Event::Stop { force: false })
            .unwrap_or_else(|refusal| panic!("{refusal:?}"));
        assert!(
            state.apply(current, &Event::Stop { force: true }).is_ok(),
            "a stop can be escalated to force"
        );
        assert_eq!(
            state.apply(current, &Event::Stop { force: false }),
            Err(Refusal::Unavailable(SessionState::Stopping))
        );
    }

    #[test]
    fn restart_ends_the_active_execution_before_it_claims_the_session() {
        let mut state = Lifecycle::new();
        let generation = state.generation();
        for event in [
            Event::Spawned,
            Event::Ready,
            Event::Submit {
                turn_id: "t1",
                seq: 1,
                acknowledged: true,
            },
        ] {
            state.apply(generation, &event).unwrap_or_default();
        }
        let effects = state
            .apply(generation, &Event::Restart)
            .unwrap_or_else(|refusal| panic!("{refusal:?}"));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EndTurn { turn_id, outcome: Outcome::Interrupted, .. } if turn_id == "t1"
        )));
        assert_eq!(state.phase(), &Phase::Restarting { old_alive: true });
        let _ = (
            CancelProgress {
                pending_controls: 0,
                provider_finished: false,
            },
            Execution {
                turn_id: String::new(),
                seq: 0,
                acknowledged: false,
                blocked: false,
                cancel: None,
            },
        );
    }
}
