//! Routing to the clients attached to one session.
//!
//! There used to be exactly one. The wire server wrote every outbound frame
//! into a single `Queue<Value>` drained by a single writer task, so "the
//! client", "the connection" and "the output" were the same thing. Once
//! several frontends can sit on one live agent, outbound traffic splits in
//! two kinds:
//!
//! - **Events and reverse-RPC requests are session facts.** A turn began; a
//!   tool wants approval. Everyone attached gets them.
//! - **A JSON-RPC response is not a session fact.** Its id was minted by one
//!   client's `next_id` and is unique only within that connection, so it goes
//!   back to the connection that asked and nowhere else. This is why ids
//!   never need a `(connection, id)` key: nothing routes *by* a client id,
//!   the handler that answers already knows whose question it was.
//!
//! A client that has gone away is pruned rather than reported: one frontend
//! closing its window must not silence the others.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tracing::debug;

use crate::utils::Queue;

/// Which attached client. Ids are minted once per process and never reused,
/// so an id held past a detach resolves to nothing rather than to whoever
/// attached next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(u64);

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "client#{}", self.0)
    }
}

struct Attachment {
    queue: Queue<Value>,
    /// Set while this connection is catching up on the session's past. Live
    /// traffic collects here instead of going out, so that a replay of what
    /// happened and a report of what is happening cannot interleave in the
    /// client's transcript. Flushed, in order, when the catch-up ends.
    staged: Option<Vec<Value>>,
}

/// Every client attached to one session, and the routing over them.
pub struct Fanout {
    attachments: Mutex<HashMap<ConnId, Attachment>>,
    next_id: AtomicU64,
}

impl Fanout {
    pub fn new() -> Self {
        Self {
            attachments: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a new client. The returned queue is its outbound half; a
    /// writer task drains it, and `detach` shuts it down to stop that task.
    pub fn attach(&self) -> (ConnId, Queue<Value>) {
        let id = ConnId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let queue = Queue::new();
        self.attachments.lock().unwrap().insert(
            id,
            Attachment {
                queue: queue.clone(),
                staged: None,
            },
        );
        (id, queue)
    }

    pub fn detach(&self, id: ConnId) {
        let attachment = self.attachments.lock().unwrap().remove(&id);
        if let Some(attachment) = attachment {
            attachment.queue.shutdown(false);
        }
    }

    pub fn len(&self) -> usize {
        self.attachments.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A session fact: everyone attached sees it. A connection that is
    /// catching up gets it staged rather than sent.
    pub fn broadcast(&self, msg: Value) {
        let mut attachments = self.attachments.lock().unwrap();
        attachments.retain(|id, attachment| {
            if let Some(staged) = attachment.staged.as_mut() {
                staged.push(msg.clone());
                return true;
            }
            if attachment.queue.put_nowait(msg.clone()).is_err() {
                debug!("{id} is gone; dropping it from the fan-out");
                return false;
            }
            true
        });
    }

    /// An answer to one client's question, or its own catch-up output.
    /// Bypasses staging: this *is* what the connection is waiting for.
    pub fn send_to(&self, id: ConnId, msg: Value) {
        let mut attachments = self.attachments.lock().unwrap();
        let Some(attachment) = attachments.get(&id) else {
            debug!("{id} is no longer attached; dropping a message meant for it");
            return;
        };
        if attachment.queue.put_nowait(msg).is_err() {
            debug!("{id} is gone; dropping it from the fan-out");
            attachments.remove(&id);
        }
    }

    /// Start staging live traffic for one connection. Callers must pair this
    /// with `end_catch_up`, including on the error paths, or the connection
    /// goes deaf.
    pub fn begin_catch_up(&self, id: ConnId) {
        let mut attachments = self.attachments.lock().unwrap();
        if let Some(attachment) = attachments.get_mut(&id) {
            attachment.staged = Some(Vec::new());
        }
    }

    /// Release what was staged, in arrival order, and go live again. The
    /// flush happens under the lock so a message published mid-flush cannot
    /// overtake the staged ones it came after.
    pub fn end_catch_up(&self, id: ConnId) {
        let mut attachments = self.attachments.lock().unwrap();
        let Some(attachment) = attachments.get_mut(&id) else {
            return;
        };
        let Some(staged) = attachment.staged.take() else {
            return;
        };
        for msg in staged {
            if attachment.queue.put_nowait(msg).is_err() {
                debug!("{id} is gone; dropping it from the fan-out");
                attachments.remove(&id);
                return;
            }
        }
    }

    pub fn shutdown(&self) {
        let mut attachments = self.attachments.lock().unwrap();
        for attachment in attachments.values() {
            attachment.queue.shutdown(false);
        }
        attachments.clear();
    }
}

impl Default for Fanout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drain(queue: &Queue<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(msg) = queue.get_nowait() {
            out.push(msg);
        }
        out
    }

    #[test]
    fn a_broadcast_reaches_every_attached_client() {
        let fanout = Fanout::new();
        let (_, first) = fanout.attach();
        let (_, second) = fanout.attach();

        fanout.broadcast(json!({"method": "event"}));

        assert_eq!(drain(&first), vec![json!({"method": "event"})]);
        assert_eq!(drain(&second), vec![json!({"method": "event"})]);
    }

    #[test]
    fn a_response_reaches_only_the_client_that_asked() {
        let fanout = Fanout::new();
        let (asker, asker_queue) = fanout.attach();
        let (_, bystander) = fanout.attach();

        fanout.send_to(asker, json!({"id": "1", "result": {}}));

        assert_eq!(drain(&asker_queue), vec![json!({"id": "1", "result": {}})]);
        assert!(drain(&bystander).is_empty());
    }

    #[test]
    fn ids_are_never_reused_after_a_detach() {
        let fanout = Fanout::new();
        let (first, _) = fanout.attach();
        fanout.detach(first);
        let (second, queue) = fanout.attach();

        assert_ne!(first, second);
        // The stale id must not resolve to the newcomer.
        fanout.send_to(first, json!({"stale": true}));
        assert!(drain(&queue).is_empty());
    }

    #[test]
    fn a_client_that_went_away_does_not_silence_the_others() {
        let fanout = Fanout::new();
        let (_, gone) = fanout.attach();
        let (_, alive) = fanout.attach();
        gone.shutdown(false);

        fanout.broadcast(json!({"method": "event"}));

        assert_eq!(drain(&alive), vec![json!({"method": "event"})]);
        assert_eq!(fanout.len(), 1, "the dead client should have been pruned");
    }

    #[test]
    fn a_catching_up_client_sees_its_own_output_before_the_live_stream() {
        let fanout = Fanout::new();
        let (joiner, queue) = fanout.attach();

        fanout.begin_catch_up(joiner);
        fanout.broadcast(json!({"live": 1}));
        fanout.send_to(joiner, json!({"replayed": 1}));
        fanout.broadcast(json!({"live": 2}));
        fanout.send_to(joiner, json!({"replayed": 2}));
        fanout.end_catch_up(joiner);

        assert_eq!(
            drain(&queue),
            vec![
                json!({"replayed": 1}),
                json!({"replayed": 2}),
                json!({"live": 1}),
                json!({"live": 2}),
            ],
            "replay output first, then the live traffic that arrived during it"
        );
    }

    #[test]
    fn staging_one_client_does_not_stage_the_others() {
        let fanout = Fanout::new();
        let (joiner, joiner_queue) = fanout.attach();
        let (_, settled) = fanout.attach();

        fanout.begin_catch_up(joiner);
        fanout.broadcast(json!({"live": 1}));

        assert!(drain(&joiner_queue).is_empty());
        assert_eq!(drain(&settled), vec![json!({"live": 1})]);
    }
}
