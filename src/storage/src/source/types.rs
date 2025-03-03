// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types related to the source ingestion pipeline/framework.

// https://github.com/tokio-rs/prost/issues/237
// #![allow(missing_docs)]

use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::{Send, Sync};
use std::time::Duration;

use async_trait::async_trait;
use differential_dataflow::Hashable;
use futures::stream::LocalBoxStream;
use prometheus::core::{AtomicI64, AtomicU64};
use serde::{Deserialize, Serialize};
use timely::dataflow::channels::pact::{Exchange, ParallelizationContract};
use timely::scheduling::activate::SyncActivator;
use timely::Data;

use mz_avro::types::Value;
use mz_expr::PartitionId;
use mz_ore::metrics::{CounterVecExt, DeleteOnDropCounter, DeleteOnDropGauge, GaugeVecExt};
use mz_repr::{Diff, GlobalId, Row, Timestamp};
use mz_storage_client::types::connections::ConnectionContext;
use mz_storage_client::types::errors::{DecodeError, SourceErrorDetails};
use mz_storage_client::types::sources::encoding::SourceDataEncoding;
use mz_storage_client::types::sources::MzOffset;

use crate::source::metrics::SourceBaseMetrics;
use crate::source::source_reader_pipeline::HealthStatus;

/// Extension trait to the SourceConnection trait that defines how to intantiate a particular
/// connetion into a reader and offset committer
pub trait SourceConnectionBuilder {
    type Reader: SourceReader + 'static;
    type OffsetCommitter: OffsetCommitter + Send + Sync + 'static;

    /// Turn this connection into a new source reader.
    ///
    /// This function returns the source reader and its corresponding offset committed.
    fn into_reader(
        self,
        source_name: String,
        source_id: GlobalId,
        worker_id: usize,
        worker_count: usize,
        consumer_activator: SyncActivator,
        restored_offsets: Vec<(PartitionId, Option<MzOffset>)>,
        encoding: SourceDataEncoding,
        metrics: crate::source::metrics::SourceBaseMetrics,
        connection_context: ConnectionContext,
    ) -> Result<(Self::Reader, Self::OffsetCommitter), anyhow::Error>;
}

/// This trait defines the interface between Materialize and external sources,
/// and must be implemented for every new kind of source.
///
/// ## Contract between [`SourceReader`] and the ingestion framework
///
/// A source reader uses updates emitted from
/// [`SourceReader::next`]/[`SourceReader::get_next_message`] to update the
/// ingestion framework about new updates retrieved from the external system and
/// about its internal state.
///
/// The framework will spawn a [`SourceReader`] on each timely worker. It is the
/// responsibility of the reader to figure out which of the partitions (if any)
/// it is responsible for reading using [`crate::source::responsible_for`].
///
/// The reader implicitly is given a capability for emitting updates for each
/// new partition (identified by a [`PartitionId`]) that it discovers. It must
/// downgrade those capabilities by either emitting updates for those partitions
/// that it is responsible for or by emitting a
/// [`SourceMessageType::DropPartitionCapabilities`] for those partitions which
/// it is not responsible for.
//
// TODO: this trait is still a little too Kafka-centric, specifically the concept of
// a "partition" is baked into this trait and introduces some cognitive overhead as
// we are forced to treat things like file sources as "single-partition"
#[async_trait(?Send)]
pub trait SourceReader {
    type Key: timely::Data + MaybeLength;
    type Value: timely::Data + MaybeLength;
    type Diff: timely::Data;

    /// Returns the next message available from the source.
    ///
    /// Note that implementers are required to present messages in strictly ascending offset order
    /// within each partition.
    async fn next(
        &mut self,
        timestamp_granularity: Duration,
    ) -> Option<SourceMessageType<Self::Key, Self::Value, Self::Diff>> {
        // Compatiblity implementation that delegates to the deprecated [Self::get_next_method]
        // call. Once all source implementations have been transitioned to implement
        // [SourceReader::next] directly this provided implementation should be removed and the
        // method should become a required method.
        loop {
            match self.get_next_message() {
                NextMessage::Ready(msg) => return Some(msg),
                // There was a temporary hiccup in getting messages, check again asap.
                NextMessage::TransientDelay => tokio::time::sleep(Duration::from_millis(1)).await,
                // There were no new messages, check again after a delay
                NextMessage::Pending => tokio::time::sleep(timestamp_granularity).await,
                NextMessage::Finished => return None,
            }
        }
    }

    /// Returns the next message available from the source.
    ///
    /// Note that implementers are required to present messages in strictly ascending offset order
    /// within each partition.
    ///
    /// # Deprecated
    ///
    /// Source implementation should implement the async [SourceReader::next] method instead.
    fn get_next_message(&mut self) -> NextMessage<Self::Key, Self::Value, Self::Diff> {
        NextMessage::Pending
    }

    /// Returns an adapter that treats the source as a stream.
    ///
    /// The stream produces the messages that would be produced by repeated calls to `next`.
    fn into_stream<'a>(
        mut self,
        timestamp_granularity: Duration,
    ) -> LocalBoxStream<'a, SourceMessageType<Self::Key, Self::Value, Self::Diff>>
    where
        Self: Sized + 'a,
    {
        Box::pin(async_stream::stream!({
            while let Some(msg) = self.next(timestamp_granularity).await {
                yield msg;
            }
        }))
    }
}

/// A sibling trait to `SourceReader` that represents a source's
/// ability to _commit offsets_ that have been guaranteed
/// to be written into persist
#[async_trait]
pub trait OffsetCommitter {
    /// Commit the given partition-offset pairs upstream.
    /// A specific `SourceReader`-`OffsetCommiter` pair
    /// is guaranteed to only receive offsets for partitions
    /// they are owners for.
    async fn commit_offsets(
        &self,
        offsets: HashMap<PartitionId, MzOffset>,
    ) -> Result<(), anyhow::Error>;
}

pub enum NextMessage<Key, Value, Diff> {
    Ready(SourceMessageType<Key, Value, Diff>),
    Pending,
    TransientDelay,
    Finished,
}

/// A wrapper around [`SourceMessage`] that allows [`SourceReader`]'s to
/// communicate additional "maintenance" messages.
#[derive(Debug)]
pub enum SourceMessageType<Key, Value, Diff> {
    /// Communicate that this [`SourceMessage`] is the final
    /// message its its offset.
    Finalized(
        Result<SourceMessage<Key, Value>, SourceReaderError>,
        (PartitionId, MzOffset),
        Diff,
    ),
    /// Communicate that more [`SourceMessage`]'s
    /// will come later at the same offset as this one.
    InProgress(
        Result<SourceMessage<Key, Value>, SourceReaderError>,
        (PartitionId, MzOffset),
        Diff,
    ),
    /// Information about the source status
    SourceStatus(HealthStatus),
    /// Signals that this [`SourceReader`] instance will never emit
    /// messages/updates for a given partition anymore. This is similar enough
    /// to a timely operator dropping a capability, hence the naming.
    ///
    /// We need these to compute a "global" source upper, when determining
    /// completeness of a timestamp.
    DropPartitionCapabilities(Vec<PartitionId>),
}

/// Source-agnostic wrapper for messages. Each source must implement a
/// conversion to Message.
#[derive(Debug, Clone)]
pub struct SourceMessage<Key, Value> {
    /// The output stream this message belongs to. Later in the pipeline the stream is partitioned
    /// based on this value and is fed to the appropriate source exports
    pub output: usize,
    /// The time that an external system first observed the message
    ///
    /// Milliseconds since the unix epoch
    pub upstream_time_millis: Option<i64>,
    /// The message key
    pub key: Key,
    /// The message value
    pub value: Value,
    /// Headers, if the source is configured to pass them along. If it is, but there are none, it
    /// passes `Some([])`
    pub headers: Option<Vec<(String, Option<Vec<u8>>)>>,
}

/// A record produced by a source
#[derive(Clone, Serialize, Debug, Deserialize)]
pub struct SourceOutput<K, V, D>
where
    K: Data,
    V: Data,
{
    /// The record's key (or some empty/default value for sources without the concept of key)
    pub key: K,
    /// The record's value
    pub value: V,
    /// The position in the partition described by the `partition` in the source
    /// (e.g., Kafka offset, file line number, monotonic increasing
    /// number, etc.)
    pub position: MzOffset,
    /// The time the record was created in the upstream system, as milliseconds since the epoch
    pub upstream_time_millis: Option<i64>,
    /// The partition of this message, present iff the partition comes from Kafka
    pub partition: PartitionId,
    /// Headers, if the source is configured to pass them along. If it is, but there are none, it
    /// passes `Some([])`
    pub headers: Option<Vec<(String, Option<Vec<u8>>)>>,

    /// Indicator for what the differential `diff` value
    /// for this decoded message should be
    pub diff: D,
}

impl<K, V, D> SourceOutput<K, V, D>
where
    K: Data,
    V: Data,
{
    /// Build a new SourceOutput
    pub fn new(
        key: K,
        value: V,
        position: MzOffset,
        upstream_time_millis: Option<i64>,
        partition: PartitionId,
        headers: Option<Vec<(String, Option<Vec<u8>>)>>,
        diff: D,
    ) -> SourceOutput<K, V, D> {
        SourceOutput {
            key,
            value,
            position,
            upstream_time_millis,
            partition,
            headers,
            diff,
        }
    }
}

impl<K, V, D> SourceOutput<K, V, D>
where
    K: Data + Serialize + for<'a> Deserialize<'a> + Send + Sync,
    V: Data + Serialize + for<'a> Deserialize<'a> + Send + Sync,
    D: Data + Serialize + for<'a> Deserialize<'a> + Send + Sync,
{
    /// A parallelization contract that hashes by positions (if available)
    /// and otherwise falls back to hashing by value. Values can be just as
    /// skewed as keys, whereas positions are generally known to be unique or
    /// close to unique in a source. For example, Kafka offsets are unique per-partition.
    /// Most decode logic should use this instead of `key_contract`.
    pub fn position_value_contract() -> impl ParallelizationContract<Timestamp, Self>
    where
        V: Hashable<Output = u64>,
    {
        Exchange::new(|x: &Self| x.position.hashed())
    }
}

/// The output of the decoding operator
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct DecodeResult {
    /// The decoded key
    pub key: Option<Result<Row, DecodeError>>,
    /// The decoded value, as well as the the
    /// differential `diff` value for this value, if the value
    /// is present and not and error.
    pub value: Option<Result<(Row, Diff), DecodeError>>,
    /// The index of the decoded value in the stream
    pub position: MzOffset,
    /// The time the record was created in the upstream system, as milliseconds since the epoch
    pub upstream_time_millis: Option<i64>,
    /// The partition this record came from
    pub partition: PartitionId,
    /// If this is a Kafka stream, the appropriate metadata
    // TODO(bwm): This should probably be statically different for different streams, or we should
    // propagate whether metadata is requested into the decoder
    pub metadata: Row,
}

/// A structured error for `SourceReader::get_next_message` implementors.
#[derive(Debug, Clone)]
pub struct SourceReaderError {
    pub inner: SourceErrorDetails,
}

impl SourceReaderError {
    /// This is an unclassified but definite error. This is typically only appropriate
    /// when the error is permanently fatal for the source... some critical invariant
    /// is violated or data is corrupted, for example.
    pub fn other_definite(e: anyhow::Error) -> SourceReaderError {
        SourceReaderError {
            inner: SourceErrorDetails::Other(format!("{}", e)),
        }
    }
}

/// Source-specific metrics in the persist sink
pub struct SourcePersistSinkMetrics {
    pub(crate) progress: DeleteOnDropGauge<'static, AtomicI64, Vec<String>>,
    pub(crate) row_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) row_retractions: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) error_inserts: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) error_retractions: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
    pub(crate) processed_batches: DeleteOnDropCounter<'static, AtomicU64, Vec<String>>,
}

impl SourcePersistSinkMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(
        base: &SourceBaseMetrics,
        shard_id: &mz_persist_client::ShardId,
        source_id: GlobalId,
        output_index: usize,
    ) -> SourcePersistSinkMetrics {
        let shard = shard_id.to_string();
        SourcePersistSinkMetrics {
            progress: base.source_specific.progress.get_delete_on_drop_gauge(vec![
                source_id.to_string(),
                output_index.to_string(),
                shard.clone(),
            ]),
            row_inserts: base
                .source_specific
                .row_inserts
                .get_delete_on_drop_counter(vec![
                    source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                ]),
            row_retractions: base
                .source_specific
                .row_retractions
                .get_delete_on_drop_counter(vec![
                    source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                ]),
            error_inserts: base
                .source_specific
                .error_inserts
                .get_delete_on_drop_counter(vec![
                    source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                ]),
            error_retractions: base
                .source_specific
                .error_retractions
                .get_delete_on_drop_counter(vec![
                    source_id.to_string(),
                    output_index.to_string(),
                    shard.clone(),
                ]),
            processed_batches: base
                .source_specific
                .persist_sink_processed_batches
                .get_delete_on_drop_counter(vec![
                    source_id.to_string(),
                    output_index.to_string(),
                    shard,
                ]),
        }
    }
}

/// Source-specific Prometheus metrics
pub struct SourceMetrics {
    /// Value of the capability associated with this source
    pub(crate) capability: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// The resume_upper for a source.
    pub(crate) resume_upper: DeleteOnDropGauge<'static, AtomicI64, Vec<String>>,
    /// Per-partition Prometheus metrics.
    pub(crate) partition_metrics: HashMap<PartitionId, PartitionMetrics>,
    source_name: String,
    source_id: GlobalId,
    base_metrics: SourceBaseMetrics,
}

impl SourceMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(
        base: &SourceBaseMetrics,
        source_name: &str,
        source_id: GlobalId,
        worker_id: &str,
    ) -> SourceMetrics {
        let labels = &[
            source_name.to_string(),
            source_id.to_string(),
            worker_id.to_string(),
        ];
        SourceMetrics {
            capability: base
                .source_specific
                .capability
                .get_delete_on_drop_gauge(labels.to_vec()),
            resume_upper: base
                .source_specific
                .resume_upper
                .get_delete_on_drop_gauge(vec![source_id.to_string()]),
            partition_metrics: Default::default(),
            source_name: source_name.to_string(),
            source_id,
            base_metrics: base.clone(),
        }
    }

    /// Log updates to which offsets / timestamps read up to.
    pub fn record_partition_offsets(
        &mut self,
        offsets: HashMap<PartitionId, (MzOffset, Timestamp, i64)>,
    ) {
        for (partition, (offset, timestamp, count)) in offsets {
            let metric = self
                .partition_metrics
                .entry(partition.clone())
                .or_insert_with(|| {
                    PartitionMetrics::new(
                        &self.base_metrics,
                        &self.source_name,
                        self.source_id,
                        &partition,
                    )
                });

            metric.messages_ingested.inc_by(count);

            metric.record_offset(
                &self.source_name,
                self.source_id,
                &partition,
                offset.offset,
                u64::from(timestamp) as i64,
            );
        }
    }
}

/// Partition-specific metrics, recorded to both Prometheus and a system table
pub struct PartitionMetrics {
    /// Highest offset that has been received by the source and timestamped
    pub(crate) offset_ingested: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Highest offset that has been received by the source
    pub(crate) offset_received: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Value of the highest timestamp that is closed (for which all messages have been ingested)
    pub(crate) closed_ts: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
    /// Total number of messages that have been received by the source and timestamped
    pub(crate) messages_ingested: DeleteOnDropCounter<'static, AtomicI64, Vec<String>>,
    pub(crate) last_offset: u64,
    pub(crate) last_timestamp: i64,
}

impl PartitionMetrics {
    /// Record the latest offset ingested high-water mark
    fn record_offset(
        &mut self,
        _source_name: &str,
        _source_id: GlobalId,
        _partition_id: &PartitionId,
        offset: u64,
        timestamp: i64,
    ) {
        self.offset_received.set(offset);
        self.offset_ingested.set(offset);
        self.last_offset = offset;
        self.last_timestamp = timestamp;
    }

    /// Initialises partition metrics for a given (source_id, partition_id)
    pub fn new(
        base_metrics: &SourceBaseMetrics,
        source_name: &str,
        source_id: GlobalId,
        partition_id: &PartitionId,
    ) -> PartitionMetrics {
        let labels = &[
            source_name.to_string(),
            source_id.to_string(),
            partition_id.to_string(),
        ];
        let base = &base_metrics.partition_specific;
        PartitionMetrics {
            offset_ingested: base
                .offset_ingested
                .get_delete_on_drop_gauge(labels.to_vec()),
            offset_received: base
                .offset_received
                .get_delete_on_drop_gauge(labels.to_vec()),
            closed_ts: base.closed_ts.get_delete_on_drop_gauge(labels.to_vec()),
            messages_ingested: base
                .messages_ingested
                .get_delete_on_drop_counter(labels.to_vec()),
            last_offset: 0,
            last_timestamp: 0,
        }
    }
}

/// Source reader operator specific Prometheus metrics
pub struct SourceReaderMetrics {
    /// Per-partition Prometheus metrics.
    pub(crate) partition_metrics: HashMap<PartitionId, SourceReaderPartitionMetrics>,
    source_id: GlobalId,
    base_metrics: SourceBaseMetrics,
}

impl SourceReaderMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(base: &SourceBaseMetrics, source_id: GlobalId) -> SourceReaderMetrics {
        SourceReaderMetrics {
            partition_metrics: Default::default(),
            source_id,
            base_metrics: base.clone(),
        }
    }

    /// Log updates to which offsets / timestamps read up to.
    pub fn metrics_for_partition(&mut self, pid: &PartitionId) -> &SourceReaderPartitionMetrics {
        self.partition_metrics
            .entry(pid.clone())
            .or_insert_with(|| {
                SourceReaderPartitionMetrics::new(&self.base_metrics, self.source_id, pid)
            })
    }
}

/// Partition-specific metrics, recorded to both Prometheus and a system table
pub struct SourceReaderPartitionMetrics {
    /// The offset-domain resume_upper for a source.
    pub(crate) source_resume_upper: DeleteOnDropGauge<'static, AtomicU64, Vec<String>>,
}

impl SourceReaderPartitionMetrics {
    /// Initialises partition metrics for a given (source_id, partition_id)
    pub fn new(
        base_metrics: &SourceBaseMetrics,
        source_id: GlobalId,
        partition_id: &PartitionId,
    ) -> SourceReaderPartitionMetrics {
        let base = &base_metrics.partition_specific;
        SourceReaderPartitionMetrics {
            source_resume_upper: base
                .source_resume_upper
                .get_delete_on_drop_gauge(vec![source_id.to_string(), partition_id.to_string()]),
        }
    }
}

/// Types that implement this trait expose a length function
pub trait MaybeLength {
    /// Returns the size of the object
    fn len(&self) -> Option<usize>;
}

impl MaybeLength for () {
    fn len(&self) -> Option<usize> {
        None
    }
}

impl MaybeLength for Vec<u8> {
    fn len(&self) -> Option<usize> {
        Some(self.len())
    }
}

impl MaybeLength for mz_repr::Row {
    fn len(&self) -> Option<usize> {
        Some(self.data().len())
    }
}

impl MaybeLength for Value {
    // Not possible to compute a size in bytes without recursively traversing the entire tree.
    fn len(&self) -> Option<usize> {
        None
    }
}

impl<T: MaybeLength> MaybeLength for Option<T> {
    fn len(&self) -> Option<usize> {
        self.as_ref().and_then(|v| v.len())
    }
}
