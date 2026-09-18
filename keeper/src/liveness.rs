//! A follower's view of the primary keeper, from public chain state only: committer()'s confirmed transaction nonce
//! and the work this node can see waiting. There is no heartbeat, lease or shared service between the two keepers.
use alloy_primitives::Address;

/// What a follower has observed of committer()'s confirmed nonce while sendable work was waiting, in chain seconds.
#[derive(Debug, Default)]
pub struct PrimaryLiveness {
    observed: Option<Observation>,
}
#[derive(Clone, Copy, Debug)]
struct Observation {
    committer: Address,
    nonce: u64,
    /// When this committer was first observed: by this process, so a restart observes afresh.
    since: u64,
    /// When the nonce was last seen to advance.
    advanced_at: Option<u64>,
    /// When work this node could send started waiting without the nonce advancing; cleared by an advance or an
    /// empty queue, so an idle primary with nothing to do is never called dead.
    waiting_since: Option<u64>,
}
impl PrimaryLiveness {
    /// Record committer()'s confirmed nonce at chain time `now`, and whether this node can see work the primary
    /// has not taken. A different committer starts a new observation; a lower nonce, as from a lagging RPC node,
    /// is ignored.
    pub fn observe(&mut self, committer: Address, nonce: u64, now: u64, work_waiting: bool) {
        match &mut self.observed {
            Some(observed) if observed.committer == committer => {
                if nonce > observed.nonce {
                    observed.nonce = nonce;
                    observed.advanced_at = Some(now);
                    // The primary just acted: whatever is still queued starts waiting from now.
                    observed.waiting_since = work_waiting.then_some(now);
                } else if work_waiting {
                    observed.waiting_since.get_or_insert(now);
                } else {
                    observed.waiting_since = None;
                }
            }
            _ => {
                self.observed = Some(Observation {
                    committer,
                    nonce,
                    since: now,
                    advanced_at: None,
                    waiting_since: work_waiting.then_some(now),
                })
            }
        }
    }
    /// Whether the primary counts as dead at chain time `now`: work this node can send has been waiting for a whole
    /// `window` and committer()'s confirmed nonce has not advanced in that time. A primary with nothing to do, or
    /// one that is working through a queue, is never dead, and a process that has not yet observed the chain for a
    /// whole window never declares death, so a restarted follower cannot take over early.
    pub fn dead(&self, now: u64, window: u64) -> bool {
        self.observed.is_some_and(|observed| {
            now.saturating_sub(observed.since) >= window
                && observed
                    .waiting_since
                    .is_some_and(|since| now.saturating_sub(since) >= window)
                && observed
                    .advanced_at
                    .is_none_or(|at| now.saturating_sub(at) >= window)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const WINDOW: u64 = 10;
    #[test]
    fn a_primary_is_dead_only_when_work_waits_while_its_nonce_stands_still() {
        let primary = Address::repeat_byte(1);
        let mut liveness = PrimaryLiveness::default();
        // Nothing observed yet, and nothing waiting: never dead.
        assert!(!liveness.dead(1000, WINDOW));
        liveness.observe(primary, 7, 1000, false);
        assert!(
            !liveness.dead(1100, WINDOW),
            "an idle primary with no work is not dead"
        );
        // Work appears and the nonce stands still: dead once it has waited a whole window.
        liveness.observe(primary, 7, 1100, true);
        assert!(!liveness.dead(1109, WINDOW));
        liveness.observe(primary, 7, 1109, true);
        assert!(!liveness.dead(1109, WINDOW));
        liveness.observe(primary, 7, 1110, true);
        assert!(liveness.dead(1110, WINDOW));
        // A single confirmed transaction restarts the wait: a primary working through a burst is never dead.
        liveness.observe(primary, 8, 1111, true);
        assert!(!liveness.dead(1111, WINDOW));
        assert!(!liveness.dead(1120, WINDOW));
        liveness.observe(primary, 8, 1121, true);
        assert!(liveness.dead(1121, WINDOW));
        // The queue empties while the primary is still silent: no work, no death.
        liveness.observe(primary, 8, 1122, false);
        assert!(!liveness.dead(1200, WINDOW));
        // A lagging node's lower nonce is not an advance.
        liveness.observe(primary, 8, 1200, true);
        liveness.observe(primary, 6, 1210, true);
        assert!(liveness.dead(1210, WINDOW));
    }
    #[test]
    fn a_new_committer_or_a_restarted_follower_observes_afresh() {
        let (primary, rotated) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let mut liveness = PrimaryLiveness::default();
        liveness.observe(primary, 3, 100, true);
        assert!(liveness.dead(110, WINDOW));
        liveness.observe(rotated, 50, 200, true);
        assert!(
            !liveness.dead(209, WINDOW),
            "a rotated committer is observed for a whole window first"
        );
        assert!(liveness.dead(210, WINDOW));
        // A restart loses the observation: the new process cannot declare death before it has watched a window,
        // however long the work in front of it has actually been waiting.
        let mut restarted = PrimaryLiveness::default();
        restarted.observe(rotated, 50, 300, true);
        assert!(!restarted.dead(309, WINDOW));
        assert!(restarted.dead(310, WINDOW));
    }
}
