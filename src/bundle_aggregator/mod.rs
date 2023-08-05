//! Bundle aggregator, aggregates execution payloads into bundles, and makes them available on
//! stream for storage.
//!
//! Interesting performance trade-off is to do csv serialization and compression work as messages
//! are received, or to buffer them by slot and do the work all at once, concurrently.
mod hashmap_size;

use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, Result};
use async_nats::jetstream::message::Acker;
use chrono::{DateTime, Datelike, Timelike, Utc};
use futures::{
    channel::mpsc::{self, Receiver},
    future::try_join_all,
    FutureExt, SinkExt, StreamExt,
};
use lazy_static::lazy_static;
use object_store::path::Path;
use tokio::{
    select,
    sync::{Notify, RwLock},
};
use tracing::{debug, error, info, trace};

use crate::{message_consumer::AckablePayload, units::Slot};

type JsonValue = serde_json::Value;

const AGGREGATION_INTERVAL_DURATION: std::time::Duration = std::time::Duration::from_secs(1);
// The minimum time we allow for a bundle to come together.
const MIN_BUNDLE_AGE: Duration = Duration::from_secs(16);
// The time we allow for a bundle to come together after a slot has finished.
const BUNDLE_MAX_AGE_BUFFER: Duration = Duration::from_secs(8);

lazy_static! {
    // Maximum age of a slot before we consider a bundle complete.
    static ref BUNDLE_SLOT_AGE_LIMIT: Duration = Duration::from_secs(Slot::SECONDS_PER_SLOT.try_into().unwrap()) + BUNDLE_MAX_AGE_BUFFER;

    // Once a message has been delivered to us, anything may go wrong, we only ack messages after
    // they've been archived. If we fail to do so, we'd like NATS to redeliver the message. The age
    // limit is therefore the maximum amount of time we expect a bundle to take to come together.
    static ref REDELIVERY_AGE_LIMIT: Duration = Duration::from_secs(Slot::SECONDS_PER_SLOT.try_into().unwrap()) + BUNDLE_MAX_AGE_BUFFER;
}

// Archive bundles, bundle together all execution payloads which share a slot. In addition, they
// store when we first and last, saw an execution payload for this slot.
pub struct SlotBundle {
    // Used to Ack all messages linked to the bundle.
    ackers: Vec<Acker>,
    // Earliest is when we first saw an execution payload for the slot these payloads are for.
    earliest: DateTime<Utc>,
    execution_payloads: Vec<JsonValue>,
    pub slot: Slot,
}

impl SlotBundle {
    pub fn path(&self) -> Path {
        let slot = self.slot;
        let slot_date_time = slot.date_time();
        let year = slot_date_time.year();
        let month = slot_date_time.month();
        let day = slot_date_time.day();
        let hour = slot_date_time.hour();
        let minute = slot_date_time.minute();
        let slot = slot.to_string();
        let path_string =
            format!("{year}/{month:02}/{day:02}/{hour:02}/{minute:02}/{slot}.ndjson.gz");
        Path::from(path_string)
    }

    pub fn to_ndjson(&self) -> Result<String> {
        let mut ndjson = String::new();
        for execution_payload in self.execution_payloads.iter() {
            ndjson.push_str(&serde_json::to_string(execution_payload)?);
            ndjson.push('\n');
        }
        Ok(ndjson)
    }

    pub async fn ack(&self) -> Result<()> {
        try_join_all(self.ackers.iter().map(|acker| acker.ack()))
            .await
            .map_err(|e| anyhow!(e))?;

        Ok(())
    }
}

pub struct BundleAggregator {
    slot_bundles: RwLock<HashMap<Slot, SlotBundle>>,
    bundle_tx: mpsc::Sender<SlotBundle>,
}

impl BundleAggregator {
    pub fn new(bundle_tx: mpsc::Sender<SlotBundle>) -> Self {
        Self {
            slot_bundles: RwLock::new(HashMap::new()),
            bundle_tx,
        }
    }

    async fn add_execution_payload(&self, (acker, archive_payload): AckablePayload) -> Result<()> {
        let mut slot_bundles = self.slot_bundles.write().await;

        let slot_bundle = slot_bundles
            .entry(archive_payload.slot)
            .or_insert_with(|| SlotBundle {
                ackers: Vec::new(),
                earliest: Utc::now(),
                execution_payloads: Vec::new(),
                slot: archive_payload.slot,
            });

        slot_bundle.ackers.push(acker);
        slot_bundle.execution_payloads.push(archive_payload.payload);

        Ok(())
    }

    async fn find_complete_slots(&self) -> Vec<Slot> {
        let slot_bundles = self.slot_bundles.read().await;

        trace!(
            size_mb = tracing::field::display({
                let size = hashmap_size::size_of_hashmap(&slot_bundles);
                size / 1_000_000
            }),
            "scanning for complete slots in slot_bundles map"
        );

        slot_bundles
            .iter()
            .filter(|(slot, bundle)| {
                // During normal operation, the is_past_slot check should be enough. However, at times,
                // there may be a backlog. We wait at least this long from the first moment we see
                // a message for a slot, before we start archiving it, to make sure we've cleared any
                // backlog.
                let is_bundle_old_enough = Utc::now() - bundle.earliest
                    >= chrono::Duration::from_std(MIN_BUNDLE_AGE).unwrap();

                // We only want to archive slots which are at least one slot old. This is to avoid
                // archiving slots which are still being built. Normally the very first message for
                // a slot comes in at t-12. Proposer normally ask for bids around t+2. If things go
                // very wrong, perhaps t+6. At t+12 it becomes impossible to collect enough
                // attestations. Still, we may receive messages, so we allow for a very generous
                // buffer. Shipping our bundle at t + 12 + 8 = t+20.
                let is_past_slot = slot.date_time()
                    + chrono::Duration::seconds(Slot::SECONDS_PER_SLOT.into())
                    + chrono::Duration::from_std(BUNDLE_MAX_AGE_BUFFER).unwrap()
                    < Utc::now();

                is_bundle_old_enough && is_past_slot
            })
            .map(|(slot, _)| *slot)
            .collect()
    }

    async fn get_complete_bundles(&self) -> Result<Vec<(Slot, SlotBundle)>> {
        let complete_slots = self.find_complete_slots().await;
        let mut slot_bundles = self.slot_bundles.write().await;
        let mut complete_bundles = Vec::new();
        for slot in complete_slots {
            let bundle = slot_bundles.remove(&slot).unwrap();

            trace!(
                %slot,
                bundle_payload_count = bundle.execution_payloads.len(),
                bundle_size_kb = tracing::field::display({
                    let bundle_size_kb = bundle
                        .execution_payloads
                        .iter()
                        .map(|payload| { serde_json::to_string(payload).unwrap().len() })
                        .sum::<usize>() / 1000;
                    bundle_size_kb
                }),
                "completed new bundle"
            );

            complete_bundles.push((slot, bundle));
        }

        Ok(complete_bundles)
    }

    async fn run_consume_ackable_payloads_inner(
        &self,
        mut ackable_payload_rx: Receiver<AckablePayload>,
    ) -> Result<()> {
        while let Some(ackable_payload) = ackable_payload_rx.next().await {
            self.add_execution_payload(ackable_payload).await?;
        }

        Ok(())
    }

    pub async fn run_consume_ackable_payloads(
        &self,
        ackable_payload_rx: Receiver<AckablePayload>,
        shutdown_notify: &Notify,
    ) {
        select! {
            result = self.run_consume_ackable_payloads_inner(ackable_payload_rx) => {
                match result {
                    Ok(_) => info!("bundle aggregator stopped consuming ackable payloads"),
                    Err(e) => {
                        error!(%e, "bundle aggregator exited with error");
                        shutdown_notify.notify_waiters();
                    },
                }
            }
            _ = shutdown_notify.notified() => {
                info!("bundle aggregator shutting down");
            }
        }
    }

    async fn run_complete_bundle_check_inner(&self) -> Result<()> {
        debug!(
            "starting bundle aggregator, on a {:?} interval",
            AGGREGATION_INTERVAL_DURATION
        );

        let mut interval = tokio::time::interval(AGGREGATION_INTERVAL_DURATION);

        let mut tx = self.bundle_tx.clone();

        loop {
            interval.tick().await;

            let complete_bundles = self.get_complete_bundles().await?;

            if complete_bundles.is_empty() {
                trace!("no slot bundles are old enough to archive, sleeping..");
            } else {
                info!(
                    count = complete_bundles.len(),
                    "bundles are old enough with slots far enough in the past to archive",
                );
                for (slot, bundle) in complete_bundles {
                    trace!(%slot, "queueing aggregated bundle for storage");
                    tx.send(bundle).await?;
                }
            }
        }
    }

    pub async fn run_complete_bundle_check(&self, shutdown_notify: &Notify) {
        select! {
            result = self.run_complete_bundle_check_inner() => {
                match result {
                    Ok(_) => info!("bundle aggregator bundle checking stopped"),
                    Err(e) => {
                        error!(%e, "bundle aggregator exited with error");
                        shutdown_notify.notify_waiters();
                    },
                }
            }
            _ = shutdown_notify.notified().fuse() => {
                info!("bundle aggregator shutting down");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_to_ndjson() -> Result<()> {
        let slot_bundle = SlotBundle {
            ackers: vec![],
            earliest: Utc::now(),
            execution_payloads: vec![
                serde_json::json!({"foo": "bar"}),
                serde_json::json!({"baz": "qux"}),
            ],
            slot: Slot(10),
        };
        let ndjson = slot_bundle.to_ndjson()?;
        assert_eq!(ndjson, "{\"foo\":\"bar\"}\n{\"baz\":\"qux\"}\n");

        Ok(())
    }
}