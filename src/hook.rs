//! The rig hook that drives gaff during a run.
//!
//! The decision logic is factored into pure functions, so it is tested
//! without a model or a rig `HookContext` (which the crate keeps private).
//! The `AgentHook` methods are thin: they call gaff, update the run-scoped
//! counters in the scratchpad, and map the result to a rig action.

use std::collections::HashSet;

use rig::agent::hook::{
    AgentHook, HookContext, ModelTurnAction, ModelTurnFinished, ToolCall, ToolCallAction,
    ToolResultAction, ToolResultEvent,
};
use rig::completion::message::AssistantContent;
use rig::one_or_many::OneOrMany;

use crate::gaff::{Gaff, Outcome};

/// The most consecutive refusals of one tool call before the run stops.
/// Without a cap, a model that keeps retrying a refused call never ends.
const MAX_REFUSALS: u32 = 3;

/// The most times a held stop is refused before the run stops cleanly.
/// Without a cap, a permanent hold burns the turn budget to a max-turns
/// error instead of a legible stop.
const MAX_STOP_RETRIES: u32 = 3;

/// The rig hook. It carries the run's gaff context and holds no mutable
/// state of its own; the counters live in the scratchpad.
pub struct GaffHook {
    gaff: Gaff,
}

impl GaffHook {
    #[must_use]
    pub fn new(gaff: Gaff) -> Self {
        Self { gaff }
    }
}

/// The streak of refusals of one tool call, in the scratchpad.
#[derive(Clone, Default)]
struct RefusalStreak {
    key: Option<String>,
    count: u32,
}

/// The count of refused stops, in the scratchpad.
#[derive(Clone, Default)]
struct StopRetries(u32);

/// The rig call ids of tool calls that were skipped, so their post-call
/// count is suppressed.
#[derive(Clone, Default)]
struct SkippedCalls(HashSet<String>);

/// Whether a finished turn ran no tool. A retry is valid only for such a
/// turn; a retry on a tool-bearing turn fails the whole run.
#[must_use]
pub fn is_tool_free(content: &OneOrMany<AssistantContent>) -> bool {
    !content
        .iter()
        .any(|item| matches!(item, AssistantContent::ToolCall(_)))
}

/// The rig action for a gaff outcome on a pre-tool call.
///
/// A refusal or a failure both skip: a failure fails closed, because a
/// call gaff could not check must not run.
#[must_use]
pub fn tool_action(outcome: Outcome) -> ToolCallAction {
    match outcome {
        Outcome::Allow(_) => ToolCallAction::Run,
        Outcome::Refuse(reason) => ToolCallAction::Skip(reason),
        Outcome::Failed(message) => ToolCallAction::Skip(format!(
            "gaff could not check this call, so it is refused: {message}"
        )),
    }
}

/// Advance the refusal streak for `key`. A run resets it; a refusal of the
/// same key advances it. Returns true when the run should stop.
fn advance_streak(streak: &mut RefusalStreak, key: &str, refused: bool) -> bool {
    if !refused {
        streak.key = None;
        streak.count = 0;
        return false;
    }
    if streak.key.as_deref() == Some(key) {
        streak.count += 1;
    } else {
        streak.key = Some(key.to_owned());
        streak.count = 1;
    }
    streak.count >= MAX_REFUSALS
}

/// The rig action for a finished turn.
///
/// A turn that ran a tool is accepted; the stop rule applies only to a
/// text-only turn. A refused stop retries with the hold text until the
/// cap, then stops cleanly. A gaff failure at a stop lets the stop
/// through, so a broken gaff cannot trap the agent forever.
#[must_use]
pub fn stop_action(tool_free: bool, outcome: Outcome, retries: &mut u32) -> ModelTurnAction {
    if !tool_free {
        return ModelTurnAction::Continue;
    }
    match outcome {
        Outcome::Allow(_) | Outcome::Failed(_) => ModelTurnAction::Continue,
        Outcome::Refuse(reason) => {
            *retries += 1;
            if *retries > MAX_STOP_RETRIES {
                ModelTurnAction::stop(format!("held, and the hold did not clear: {reason}"))
            } else {
                ModelTurnAction::retry_with_feedback(reason)
            }
        }
    }
}

impl AgentHook for GaffHook {
    async fn on_tool_call(&self, ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let key = format!("{}\u{1f}{}", event.tool_name, event.args);
        let outcome = self
            .gaff
            .hook("pre_tool_call", Some((event.tool_name, event.args)))
            .await;
        let refused = !matches!(outcome, Outcome::Allow(_));
        let action = tool_action(outcome);

        let over_streak = ctx
            .scratchpad()
            .update::<RefusalStreak, _>(|streak| advance_streak(streak, &key, refused));

        if matches!(action, ToolCallAction::Skip(_)) {
            let id = event.internal_call_id.to_owned();
            ctx.scratchpad().update::<SkippedCalls, _>(|s| {
                s.0.insert(id);
            });
        }

        if over_streak {
            return ToolCallAction::stop(format!(
                "the tool `{}` was refused {MAX_REFUSALS} times in a row",
                event.tool_name
            ));
        }
        action
    }

    async fn on_tool_result(
        &self,
        ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        // Do not count a call whose body did not run. This hook fires for
        // a framework-skipped call too.
        let skipped = ctx
            .scratchpad()
            .update::<SkippedCalls, _>(|s| s.0.remove(event.internal_call_id));
        if !skipped {
            let _ = self.gaff.hook("tool_call", None).await;
        }
        ToolResultAction::Keep
    }

    async fn on_model_turn_finished(
        &self,
        ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if !is_tool_free(event.content) {
            return ModelTurnAction::Continue;
        }
        let outcome = self.gaff.hook("stop", None).await;
        ctx.scratchpad()
            .update::<StopRetries, _>(|retries| stop_action(true, outcome.clone(), &mut retries.0))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_streak_stops_only_on_the_same_call() {
        let mut streak = RefusalStreak::default();
        assert!(!advance_streak(&mut streak, "a", true)); // 1
        assert!(!advance_streak(&mut streak, "a", true)); // 2
        assert!(advance_streak(&mut streak, "a", true)); // 3 -> stop
    }

    #[test]
    fn a_different_call_resets_the_streak() {
        let mut streak = RefusalStreak::default();
        advance_streak(&mut streak, "a", true);
        advance_streak(&mut streak, "a", true);
        assert!(
            !advance_streak(&mut streak, "b", true),
            "a new call restarts the count"
        );
    }

    #[test]
    fn an_allowed_call_resets_the_streak() {
        let mut streak = RefusalStreak::default();
        advance_streak(&mut streak, "a", true);
        advance_streak(&mut streak, "a", false);
        assert_eq!(streak.count, 0);
    }

    #[test]
    fn a_tool_bearing_turn_is_always_accepted() {
        let mut retries: u32 = 0;
        let action = stop_action(false, Outcome::Refuse("held".into()), &mut retries);
        assert!(matches!(action, ModelTurnAction::Continue));
    }

    #[test]
    fn a_refused_stop_retries_then_stops_cleanly() {
        let mut retries: u32 = 0;
        for _ in 0..MAX_STOP_RETRIES {
            let action = stop_action(true, Outcome::Refuse("finish first".into()), &mut retries);
            assert!(
                matches!(action, ModelTurnAction::Retry(_)),
                "still retrying"
            );
        }
        let action = stop_action(true, Outcome::Refuse("finish first".into()), &mut retries);
        assert!(
            matches!(action, ModelTurnAction::Stop(_)),
            "the cap converts to a clean stop"
        );
    }

    #[test]
    fn an_allowed_stop_continues() {
        let mut retries: u32 = 0;
        assert!(matches!(
            stop_action(true, Outcome::Allow(String::new()), &mut retries),
            ModelTurnAction::Continue
        ));
    }

    #[test]
    fn a_gaff_failure_at_a_stop_lets_the_agent_stop() {
        let mut retries: u32 = 0;
        assert!(matches!(
            stop_action(true, Outcome::Failed("boom".into()), &mut retries),
            ModelTurnAction::Continue
        ));
    }

    #[test]
    fn a_failed_pre_tool_check_fails_closed() {
        assert!(matches!(
            tool_action(Outcome::Failed("boom".into())),
            ToolCallAction::Skip(_)
        ));
    }
}
