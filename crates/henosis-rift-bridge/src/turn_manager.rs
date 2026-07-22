//! Turn discipline: identity-derived compose slots and a single compose floor.
//!
//! Two mechanisms have distinct jobs:
//!
//! - **Compose slots** pace responders into deterministic, non-overlapping
//!   windows derived from each agent's stable roster slot index. Jitter is
//!   drawn *inside* the agent's own window, never from a range shared with
//!   other agents. Distinct cascading windows replace shared jitter ranges,
//!   under which collisions are expected.
//! - **The compose floor** is a single-permit semaphore enforcing the
//!   invariant "no two agents compose against the same room state". Today the
//!   room drives generation sequentially, so the floor is uncontended; it
//!   exists so the invariant survives any future refactor that spawns
//!   generation concurrently.
//!
//! There is intentionally no queue here. A strict speaking-token queue is
//! deferred until execution mode needs strict ordering.

use rand::Rng;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::types::AgentId;

/// Manages turn interleaving, per-agent compose slots, and the compose floor.
pub struct TurnManager {
    /// Who posted last (None = human or no one yet).
    last_poster: Option<AgentId>,
    /// Width of each agent's compose window in milliseconds.
    slot_width_ms: u64,
    /// Maximum jitter drawn inside an agent's own window, in milliseconds.
    /// Strictly less than `slot_width_ms` so windows never overlap.
    slot_jitter_ms: u64,
    /// Single-permit floor: whoever holds it is the only agent composing.
    floor: Arc<Semaphore>,
}

/// Implements slot geometry, interleaving rules, and floor acquisition.
impl TurnManager {
    /// Create a turn manager with the given slot geometry. A jitter equal to
    /// or wider than the slot width would re-create overlapping windows (the
    /// exact bug this module replaces), so it is clamped below the width.
    pub fn new(slot_width_ms: u64, slot_jitter_ms: u64) -> Self {
        let slot_width_ms = slot_width_ms.max(1);
        let slot_jitter_ms = if slot_jitter_ms >= slot_width_ms {
            tracing::warn!(
                slot_width_ms,
                slot_jitter_ms,
                "slot jitter >= slot width would overlap compose windows; clamping"
            );
            slot_width_ms - 1
        } else {
            slot_jitter_ms
        };
        Self {
            last_poster: None,
            slot_width_ms,
            slot_jitter_ms,
            floor: Arc::new(Semaphore::new(1)),
        }
    }

    /// Check if an agent is allowed to post next (interleaving rule).
    /// An agent cannot post twice in a row.
    pub fn can_post_next(&self, agent_id: AgentId) -> bool {
        self.last_poster != Some(agent_id)
    }

    /// Record that an agent posted (updates last_poster).
    pub fn record_post(&mut self, agent_id: AgentId) {
        self.last_poster = Some(agent_id);
    }

    /// Record that a human posted (resets interleaving so any agent can respond).
    pub fn record_human_post(&mut self) {
        self.last_poster = None;
    }

    /// Compose delay for the agent occupying `slot_index`, measured from the
    /// start of the current round: the window
    /// `[index * width, index * width + jitter]`. Distinct indices always get
    /// disjoint windows because jitter < width.
    pub fn slot_delay(&self, slot_index: usize) -> Duration {
        let base = self.slot_width_ms.saturating_mul(slot_index as u64);
        let jitter = if self.slot_jitter_ms == 0 {
            0
        } else {
            rand::rng().random_range(0..=self.slot_jitter_ms)
        };
        Duration::from_millis(base.saturating_add(jitter))
    }

    /// Acquire the compose floor. The returned permit MUST be held for the
    /// whole compose sequence (context build, generation, post) so no other
    /// agent can compose against the same room state concurrently.
    pub async fn acquire_floor(&self) -> OwnedSemaphorePermit {
        self.floor
            .clone()
            .acquire_owned()
            .await
            .expect("compose floor semaphore is never closed")
    }
}

/// Unit tests for slot geometry, interleaving, and floor exclusivity.
#[cfg(test)]
mod tests {
    use super::TurnManager;
    use crate::types::AgentId;
    use uuid::Uuid;

    /// Verifies distinct slot indices always produce non-overlapping windows:
    /// the maximum delay of slot k stays strictly below the minimum of k+1.
    #[test]
    fn test_slot_windows_do_not_overlap() {
        let tm = TurnManager::new(6000, 4000);
        for k in 0..8u64 {
            let slot_min = k * 6000;
            let slot_max = k * 6000 + 4000;
            let next_min = (k + 1) * 6000;
            assert!(slot_max < next_min);
            for _ in 0..64 {
                let d = tm.slot_delay(k as usize).as_millis() as u64;
                assert!(d >= slot_min && d <= slot_max);
            }
        }
    }

    /// Verifies jitter wider than the slot is clamped so windows stay disjoint.
    #[test]
    fn test_excessive_jitter_is_clamped_below_width() {
        let tm = TurnManager::new(2000, 9000);
        for _ in 0..64 {
            assert!(tm.slot_delay(0).as_millis() < 2000);
        }
    }

    /// Verifies the interleaving rule: the last poster cannot post again
    /// until someone else (or a human) posts.
    #[test]
    fn test_can_post_next_blocks_consecutive_posts_only() {
        let a = AgentId(Uuid::new_v4());
        let b = AgentId(Uuid::new_v4());
        let mut tm = TurnManager::new(6000, 4000);

        assert!(tm.can_post_next(a));
        tm.record_post(a);
        assert!(!tm.can_post_next(a));
        assert!(tm.can_post_next(b));

        tm.record_human_post();
        assert!(tm.can_post_next(a));
    }

    /// Verifies the compose floor is exclusive: while one permit is held no
    /// second compose may start, and releasing it frees the floor.
    #[tokio::test]
    async fn test_floor_is_mutually_exclusive() {
        let tm = TurnManager::new(6000, 4000);
        let held = tm.acquire_floor().await;
        assert!(tm.floor.try_acquire().is_err());
        drop(held);
        assert!(tm.floor.try_acquire().is_ok());
    }
}
