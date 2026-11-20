//! Shared request-scoped handle for adopted audit-outbox mutations (#706).
//!
//! Every adopted gRPC service (enrollment, reconcile, lifecycle) receives an
//! [`AdoptedAudit`] from its adapter. It bundles the pre-minted outbox row the
//! service enqueues inside its mutation transaction with the per-request
//! committed flag the adapter's check-or-insert consults afterwards.

use chv_controlplane_store::AuditOutboxInsert;

/// Handler-side handle to one adopted mutation's audit evidence (#706).
///
/// Mirrors the BFF's `AuditOutboxTicket`: [`Self::insert`] is the
/// pre-minted outbox row the service enqueues inside its final mutation
/// transaction, and the committed flag is flipped by the service
/// immediately after that store call returns `Ok` (i.e. the enqueue
/// committed). The gRPC adapter's check-or-insert reads the flag to tell
/// "nothing was enqueued" from "the evidence is durable even though the
/// final check failed", so a check error can never produce a duplicate
/// row.
pub struct AdoptedAudit<'a> {
    insert: &'a AuditOutboxInsert,
    committed: &'a std::sync::atomic::AtomicBool,
}

impl<'a> AdoptedAudit<'a> {
    pub fn new(
        insert: &'a AuditOutboxInsert,
        committed: &'a std::sync::atomic::AtomicBool,
    ) -> Self {
        Self { insert, committed }
    }

    /// The row to enqueue in the mutation's final transaction.
    pub fn insert(&self) -> &AuditOutboxInsert {
        self.insert
    }

    /// Record that this request's enqueue committed with its mutation.
    /// Called right after the adopted store call returns `Ok`; every
    /// adopted store method enqueues as its last fallible statement, so
    /// `Ok` there means the evidence is durable.
    pub fn mark_committed(&self) {
        self.committed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn was_committed(&self) -> bool {
        self.committed.load(std::sync::atomic::Ordering::SeqCst)
    }
}
