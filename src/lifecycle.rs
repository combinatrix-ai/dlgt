//! The Session lifecycle as one pure transition function.
//!
//! Every change to a Session's phase goes through [`Lifecycle::apply`]. An
//! event names the provider generation it comes from, and execution events
//! also name their execution sequence, so an event from a replaced process or
//! a finished execution is refused here instead of being checked, or not, at
//! each call site. `apply` returns the effects the store must carry out and
//! the lifecycle events it must record; it never touches anything itself.

// Wired into the store in the next change.
#![allow(dead_code)]

use crate::protocol::SessionState;

pub type Generation = u64;

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

impl Lifecycle {
    pub const fn new() -> Self {
        Self {
            generation: 1,
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
                self.generation += 1;
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
                    None => Ok((
                        Some(Phase::Idle),
                        vec![record(outcome_kind(*outcome), Some(turn_id))],
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
                let pending_controls = cancel.pending_controls.saturating_sub(1);
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

    /// Every event worth trying from `state`, from the current generation and
    /// the one before it, naming the active execution and the one before it.
    fn alphabet(state: &Lifecycle) -> Vec<(u64, Event<'static>)> {
        let generation = state.generation();
        let seq = state.active().map_or(0, |execution| execution.seq);
        let mut events = Vec::new();
        for generation in [generation, generation.saturating_sub(1)] {
            events.push((generation, Event::Spawned));
            events.push((generation, Event::Ready));
            events.push((
                generation,
                Event::Submit {
                    turn_id: "t",
                    seq: seq + 1,
                    acknowledged: false,
                },
            ));
            events.push((
                generation,
                Event::Submit {
                    turn_id: "t",
                    seq: seq + 1,
                    acknowledged: true,
                },
            ));
            for seq in [seq, seq - 1] {
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

    /// Events the provider and the daemon produce on their own, without a
    /// caller: enough to show that no state is a dead end.
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
            let seq = state.active().map_or(0, |execution| execution.seq);
            for event in [
                Event::Spawned,
                Event::Ready,
                Event::Finished {
                    seq,
                    outcome: Outcome::Completed,
                },
                Event::ControlDone { seq },
                Event::Exited,
                Event::RestartLaunch,
            ] {
                let mut next = state.clone();
                if next.apply(generation, &event).is_ok() {
                    queue.push_back(next);
                }
            }
        }
        false
    }

    #[test]
    fn every_interleaving_keeps_the_lifecycle_invariants() {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([(Lifecycle::new(), 0_usize)]);
        let mut transitions = 0_usize;
        while let Some((state, depth)) = queue.pop_front() {
            if !seen.insert(state.clone()) {
                continue;
            }
            assert!(settles(&state), "dead end: {state:?}");
            if depth == 9 || state.generation() > 3 {
                continue;
            }
            for (generation, event) in alphabet(&state) {
                let mut next = state.clone();
                let result = next.apply(generation, &event);
                let Ok(effects) = result else {
                    assert_eq!(next, state, "a refused {event:?} changed {state:?}");
                    if generation != state.generation() {
                        assert_eq!(result, Err(Refusal::Stale));
                    }
                    continue;
                };
                transitions += 1;
                assert_eq!(
                    generation,
                    state.generation(),
                    "{event:?} from an old generation applied"
                );

                let before = state.public_state();
                let after = next.public_state();
                if before != after {
                    let expected = announces(after);
                    let published = recorded(&effects);
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
                let published = recorded(&effects);
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
                }
                if let Some(execution) = next.active() {
                    assert!(
                        !(execution.blocked && execution.cancel.is_some()),
                        "blocked while canceling: {next:?}"
                    );
                }
                queue.push_back((next, depth + 1));
            }
        }
        assert!(
            seen.len() > 50 && transitions > 500,
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
        assert_eq!(state.generation(), old + 1);
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
