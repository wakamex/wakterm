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

#[derive(Default)]
struct Leases {
    /// Each held lease's holder, and when it lapses, if it does.
    held: HashMap<PaneId, (Holder, Option<Instant>)>,
    /// Holders that may take no lease again, such as a client that went
    /// away while a request of its waited for one.
    revoked: std::collections::HashSet<Holder>,
}

static LEASES: Mutex<Option<Leases>> = Mutex::new(None);

/// A new holder identity.
pub fn new_holder() -> Holder {
    NEXT_HOLDER.fetch_add(1, Ordering::Relaxed)
}

/// Whether `holder` took the lease, and whether it held it already.
#[derive(Debug, PartialEq, Eq)]
enum Take {
    Taken,
    AlreadyHeld,
    Busy,
}

fn try_take(pane_id: PaneId, holder: Holder, lapses: bool) -> Take {
    let mut leases = LEASES.lock();
    let leases = leases.get_or_insert_with(Leases::default);
    if leases.revoked.contains(&holder) {
        return Take::Busy;
    }
    let expires = lapses.then(|| Instant::now() + LEASE_TTL);
    match leases.held.get(&pane_id) {
        Some((other, expiry))
            if *other != holder && expiry.map_or(true, |at| at > Instant::now()) =>
        {
            Take::Busy
        }
        Some((other, _)) if *other == holder => {
            leases.held.insert(pane_id, (holder, expires));
            Take::AlreadyHeld
        }
        _ => {
            leases.held.insert(pane_id, (holder, expires));
            Take::Taken
        }
    }
}

/// Takes the lease on `pane_id` for a client connection `holder`, waiting
/// while another holds it. Returns whether it took it now, rather than
/// holding it already, which extends it. A connection's lease lapses after
/// `LEASE_TTL`, in case the client never submits.
pub async fn acquire(pane_id: PaneId, holder: Holder) -> Result<bool, String> {
    let started = Instant::now();
    loop {
        match try_take(pane_id, holder, true) {
            Take::Taken => return Ok(true),
            Take::AlreadyHeld => return Ok(false),
            Take::Busy if started.elapsed() >= ACQUIRE_WAIT => {
                return Err("another prompt is being written to this agent".to_string())
            }
            Take::Busy => smol::Timer::after(Duration::from_millis(50)).await,
        };
    }
}

/// Gives up `holder`'s lease on `pane_id`, if it holds it.
pub fn release(pane_id: PaneId, holder: Holder) {
    if let Some(leases) = LEASES.lock().as_mut() {
        if leases
            .held
            .get(&pane_id)
            .is_some_and(|(owner, _)| *owner == holder)
        {
            leases.held.remove(&pane_id);
        }
    }
}

/// Gives up every lease `holder` has and keeps it from taking another, as
/// when its client disconnects while a request of its may still be waiting.
pub fn revoke(holder: Holder) {
    let mut leases = LEASES.lock();
    let leases = leases.get_or_insert_with(Leases::default);
    leases.held.retain(|_, (owner, _)| *owner != holder);
    leases.revoked.insert(holder);
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

/// Takes the lease on `pane_id` until the returned value is dropped. It
/// does not lapse, since dropping it, on any return or unwinding, ends it:
/// a sender on the server that runs long keeps the box to itself.
pub async fn hold(pane_id: PaneId) -> Result<Held, String> {
    let holder = new_holder();
    let started = Instant::now();
    while try_take(pane_id, holder, false) == Take::Busy {
        if started.elapsed() >= ACQUIRE_WAIT {
            return Err("another prompt is being written to this agent".to_string());
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
    Ok(Held { pane_id, holder })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn one_sender_writes_into_a_pane_at_a_time() {
        let pane = 7_000_001;
        let (first, second) = (new_holder(), new_holder());
        assert_eq!(try_take(pane, first, true), Take::Taken);
        // Another sender waits; the holder may take it again.
        assert_eq!(try_take(pane, second, true), Take::Busy);
        assert_eq!(try_take(pane, first, true), Take::AlreadyHeld);
        // Another pane is free.
        assert_eq!(try_take(pane + 1, second, true), Take::Taken);
        release(pane, second);
        assert_eq!(try_take(pane, second, true), Take::Busy);
        release(pane, first);
        assert_eq!(try_take(pane, second, true), Take::Taken);
        release(pane, second);
        release(pane + 1, second);

        // A server-side lease is released when dropped, and does not lapse.
        let held = smol::block_on(hold(pane)).unwrap();
        assert_eq!(try_take(pane, second, true), Take::Busy);
        LEASES
            .lock()
            .as_mut()
            .unwrap()
            .held
            .get_mut(&pane)
            .map(|lease| assert!(lease.1.is_none()));
        drop(held);
        assert_eq!(try_take(pane, second, true), Take::Taken);
        release(pane, second);
    }

    #[test]
    fn a_disconnected_client_takes_no_lease_and_frees_its_own() {
        let pane = 7_000_101;
        let (client, other) = (new_holder(), new_holder());
        assert_eq!(try_take(pane, client, true), Take::Taken);
        revoke(client);
        // Its lease is gone, and a request of its still waiting gets none.
        assert_eq!(try_take(pane, other, true), Take::Taken);
        release(pane, other);
        assert_eq!(try_take(pane, client, true), Take::Busy);
    }
}
