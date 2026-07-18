//! v7.39 (round 222) — LISTEN / NOTIFY delivery. Was accept-and-drop
//! since v7.37.17; now notifications are real: LISTEN subscribes the
//! session, NOTIFY queues on the transaction and delivers at COMMIT
//! (PG semantics: transactional, deduplicated within the tx, dropped
//! at ROLLBACK; immediate under autocommit), and the wire layer drains
//! [`Engine::take_notifications`] into 'A' NotificationResponse
//! messages after each statement. Delivery is same-process (the
//! engine's session-state architecture wall, like `session_params`):
//! every pgwire connection shares the engine, so a NOTIFY from one
//! connection reaches a LISTEN from another at the next statement
//! boundary. Idle-connection push (no statement in flight) is not
//! implemented — psycopg2 / libpq poll patterns receive on their next
//! interaction.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{EngineError, QueryResult};

impl crate::Engine {
    /// Drain committed notifications (channel, payload). Wire layers emit
    /// each as a NotificationResponse; embedded callers consume directly.
    pub fn take_notifications(&mut self) -> Vec<(String, String)> {
        core::mem::take(&mut self.delivered_notifies)
    }

    /// COMMIT boundary — release the transaction's pending notifications
    /// to the delivery queue (only channels someone LISTENs on).
    pub(crate) fn notifies_on_commit(&mut self) {
        let pending = core::mem::take(&mut self.tx_pending_notifies);
        for (ch, payload) in pending {
            if self.listen_channels.contains(&ch) {
                self.delivered_notifies.push((ch, payload));
            }
        }
    }

    /// ROLLBACK boundary — the aborted transaction's notifications vanish.
    pub(crate) fn notifies_on_rollback(&mut self) {
        self.tx_pending_notifies.clear();
    }

    pub(crate) fn exec_listen(&mut self, channel: String) -> Result<QueryResult, EngineError> {
        self.listen_channels.insert(channel);
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_unlisten(
        &mut self,
        channel: Option<String>,
    ) -> Result<QueryResult, EngineError> {
        match channel {
            Some(c) => {
                self.listen_channels.remove(&c);
            }
            None => self.listen_channels.clear(),
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    pub(crate) fn exec_notify(
        &mut self,
        channel: String,
        payload: Option<String>,
    ) -> Result<QueryResult, EngineError> {
        let payload = payload.unwrap_or_default();
        if self.in_transaction() {
            // PG deduplicates identical (channel, payload) pairs within one
            // transaction.
            if !self
                .tx_pending_notifies
                .iter()
                .any(|(c, p)| *c == channel && *p == payload)
            {
                self.tx_pending_notifies.push((channel, payload));
            }
        } else if self.listen_channels.contains(&channel) {
            // Autocommit: immediate delivery.
            self.delivered_notifies.push((channel, payload));
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }
}
