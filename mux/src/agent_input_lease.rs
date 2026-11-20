//! One prompt at a time into each agent's input box.
//!
//! A program sends an agent a prompt by checking that the input box is
//! empty, typing the prompt and pressing Enter, and the box is shared: two
//! senders that check it at the same moment both find it empty, both type,
//! and the agent takes both texts as one input. Each sender, whether prompt
//! admission or a client typing through `PrepareAgentInputBox` and
//! `SubmitTypedInput`, holds the pane's lease from that check until its
//! prompt is submitted, and others wait for it.

use crate::pane::PaneId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Who holds a lease: one per server-side sender or client connection.
pub type Holder = u64;

/// A lease nobody releases, such as a client that checked the box and
/// went away, lapses after this.
const LEASE_TTL: Duration = Duration::from_secs(30);
/// How long a sender waits for another's prompt before it is told the box
/// is busy, which senders retry.
const ACQUIRE_WAIT: Duration = Duration::from_secs(20);

static NEXT_HOLDER: AtomicU64 = AtomicU64::new(1);
static LEASES: Mutex<Option<HashMap<PaneId, (Holder, Instant)>>> = Mutex::new(None);

/// A new holder identity.
pub fn new_holder() -> Holder {
    NEXT_HOLDER.fetch_add(1, Ordering::Relaxed)
}

fn try_take(pane_id: PaneId, holder: Holder) -> bool {
    let mut leases = LEASES.lock();
    let leases = leases.get_or_insert_with(HashMap::new);
    match leases.get(&pane_id) {
        Some((other, expires)) if *other != holder && *expires > Instant::now() => false,
        _ => {
            leases.insert(pane_id, (holder, Instant::now() + LEASE_TTL));
            true
        }
    }
}

/// Takes the lease on `pane_id` for `holder`, waiting while another holds
/// it. A holder may take a lease it holds again, which extends it.
pub async fn acquire(pane_id: PaneId, holder: Holder) -> Result<(), String> {
    let started = Instant::now();
    while !try_take(pane_id, holder) {
        if started.elapsed() >= ACQUIRE_WAIT {
            return Err("another prompt is being written to this agent".to_string());
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Gives up `holder`'s lease on `pane_id`, if it holds it.
pub fn release(pane_id: PaneId, holder: Holder) {
    if let Some(leases) = LEASES.lock().as_mut() {
        if leases
            .get(&pane_id)
            .is_some_and(|(owner, _)| *owner == holder)
        {
            leases.remove(&pane_id);
        }
    }
}

/// Gives up every lease `holder` has, as when its client disconnects.
pub fn release_all(holder: Holder) {
    if let Some(leases) = LEASES.lock().as_mut() {
        leases.retain(|_, (owner, _)| *owner != holder);
    }
}

/// A lease held until dropped, for a sender on the server.
pub struct Held {
    pane_id: PaneId,
    holder: Holder,
}

impl Drop for Held {
    fn drop(&mut self) {
        release(self.pane_id, self.holder);
    }
}

/// Takes the lease on `pane_id` until the returned value is dropped.
pub async fn hold(pane_id: PaneId) -> Result<Held, String> {
    let holder = new_holder();
    acquire(pane_id, holder).await?;
    Ok(Held { pane_id, holder })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn one_sender_writes_into_a_pane_at_a_time() {
        let pane = 7_000_001;
        let (first, second) = (new_holder(), new_holder());
        assert!(try_take(pane, first));
        // Another sender waits; the holder may take it again.
        assert!(!try_take(pane, second));
        assert!(try_take(pane, first));
        // Another pane is free.
        assert!(try_take(pane + 1, second));
        release(pane, second);
        assert!(!try_take(pane, second));
        release(pane, first);
        assert!(try_take(pane, second));
        release_all(second);
        assert!(try_take(pane, first) && try_take(pane + 1, first));
        release_all(first);

        // A held lease is released when dropped.
        let held = smol::block_on(hold(pane)).unwrap();
        assert!(!try_take(pane, second));
        drop(held);
        assert!(try_take(pane, second));
        release(pane, second);
    }
}
