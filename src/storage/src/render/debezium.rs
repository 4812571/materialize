// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;

use differential_dataflow::{Collection, Hashable};
use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::operators::{Capability, OkErr, Operator};
use timely::dataflow::{Scope, ScopeParent, Stream};

use mz_expr::EvalError;
use mz_repr::{Datum, Diff, Row, Timestamp};
use mz_storage_client::types::errors::{DataflowError, EnvelopeError};
use mz_storage_client::types::sources::{
    DebeziumDedupProjection, DebeziumEnvelope, DebeziumSourceProjection,
    DebeziumTransactionMetadata, MzOffset,
};

use crate::source::types::DecodeResult;

pub(crate) fn render<G: Scope>(
    envelope: &DebeziumEnvelope,
    input: &Stream<G, DecodeResult>,
) -> (
    Stream<G, (Row, Timestamp, Diff)>,
    Stream<G, (DataflowError, Timestamp, Diff)>,
)
where
    G: ScopeParent<Timestamp = Timestamp>,
{
    let (before_idx, after_idx) = (envelope.before_idx, envelope.after_idx);
    // TODO(guswynn): !!! Correctly deduplicate even in the upsert case
    input
        .unary(Pipeline, "envelope-debezium", move |_, _| {
            let mut dedup_state = HashMap::new();
            let envelope = envelope.clone();
            let mut data = vec![];
            move |input, output| {
                while let Some((cap, refmut_data)) = input.next() {
                    let mut session = output.session(&cap);
                    refmut_data.swap(&mut data);
                    for result in data.drain(..) {
                        let value = match result.value {
                            Some(Ok((value, 1))) => value,
                            Some(Ok(_)) => unreachable!(
                                "Debezium should only be used with sources with no explicit diff"
                            ),
                            Some(Err(err)) => {
                                session.give((Err(err.into()), cap.time().clone(), 1));
                                continue;
                            }
                            None => continue,
                        };

                        // TODO(#11664): Dedup and process the data before combining / merging data with tx metadata
                        let partition_dedup = dedup_state
                            .entry(result.partition.clone())
                            .or_insert_with(|| DebeziumDeduplicationState::new(envelope.clone()));
                        let should_use = match partition_dedup {
                            Some(ref mut s) => {
                                let res = s.should_use_record(&value);
                                match res {
                                    Ok(b) => b,
                                    Err(err) => {
                                        session.give((
                                            Err(DataflowError::EnvelopeError(err)),
                                            cap.time().clone(),
                                            1,
                                        ));
                                        continue;
                                    }
                                }
                            }
                            None => true,
                        };

                        if should_use {
                            match value.iter().nth(before_idx).unwrap() {
                                Datum::List(l) => {
                                    session.give((Ok(Row::pack(&l)), cap.time().clone(), -1))
                                }
                                Datum::Null => {}
                                d => panic!("type error: expected record, found {:?}", d),
                            }
                            match value.iter().nth(after_idx).unwrap() {
                                Datum::List(l) => {
                                    session.give((Ok(Row::pack(&l)), cap.time().clone(), 1))
                                }
                                Datum::Null => {}
                                d => panic!("type error: expected record, found {:?}", d),
                            }
                        }
                    }
                }
            }
        })
        .ok_err(|(res, time, diff)| match res {
            Ok(v) => Ok((v, time, diff)),
            Err(e) => Err((e, time, diff)),
        })
}

pub(crate) fn render_tx<G: Scope>(
    envelope: &DebeziumEnvelope,
    input: &Stream<G, DecodeResult>,
    tx_ok: Collection<G, Row, Diff>,
) -> (
    Stream<G, (Row, Timestamp, Diff)>,
    Stream<G, (DataflowError, Timestamp, Diff)>,
)
where
    G: ScopeParent<Timestamp = Timestamp>,
{
    let (before_idx, after_idx) = (envelope.before_idx, envelope.after_idx);

    let tx_metadata_description = envelope
        .dedup
        .tx_metadata
        .clone()
        .expect("render_tx should only be called when there's a transaction");

    let DebeziumTransactionMetadata {
        tx_data_collections_idx,
        tx_data_collections_data_collection_idx,
        tx_data_collection_name,
        tx_data_collections_event_count_idx,
        tx_status_idx,
        tx_transaction_id_idx,
        data_transaction_idx,
        data_transaction_id_idx,
        tx_metadata_global_id: _,
    } = tx_metadata_description;

    let tx_dist = move |&(ref row, time, _diff): &(Row, Timestamp, Diff)| match row
        .iter()
        .nth(tx_transaction_id_idx)
    {
        Some(Datum::String(s)) => s.hashed(),
        // If there's no transaction_id then we won't be able to use this row so aim to distribute evenly
        _ => time.hashed(),
    };

    let data_dist = move |result: &DecodeResult| {
        let value = match &result.value {
            Some(Ok((v, 1))) => Some(Ok(v)),
            Some(Ok(_)) => {
                unreachable!("Debezium should only be used with sources with no explicit diff")
            }
            Some(Err(e)) => Some(Err(e)),
            None => None,
        };
        // If we can't pull out the transaction_id, it doesn't matter which worker we end up on.  Use
        // value as that's how we distribute decoding dbz messages.
        let default_hash = value.hashed();

        // The logic below mirrors inline decoding of the data.  The shape of the value is validated
        // when constructing the source.
        let value = match value {
            Some(Ok(v)) => v,
            _ => return default_hash,
        };
        let transaction = match value.iter().nth(data_transaction_idx) {
            Some(Datum::List(l)) => l,
            Some(Datum::Null) => return default_hash,
            _ => panic!("Previously validated to be nullable list"),
        };
        transaction
            .iter()
            .nth(data_transaction_id_idx)
            .unwrap()
            .unwrap_str()
            .hashed()
    };

    tx_ok
        .inner
        .binary_frontier(
            input,
            Exchange::new(tx_dist),
            Exchange::new(data_dist),
            "envelope-debezium-tx",
            {
                let envelope = envelope.clone();
                let mut tx_data = vec![];
                let mut data = vec![];
                let mut data_buffer = VecDeque::new();
                let mut dedup_state = HashMap::new();

                // Keep mapping of `transaction_id`s to the timestamp at which that transaction END record was read from
                // the transaction metadata stream.  That stored timestamp will be the timestamp for all data records
                // with the corresponding `transaction_id`.  We keep this mapping around after we've matched `event_count`
                // number of data rows so that we're able to process duplicates.  Otherwise, we're not able to tell the
                // difference between "we've recieved the tx metadata and processed everything" and "we're still waiting
                // on the tx metadata".
                let mut tx_mapping: HashMap<String, Timestamp> = HashMap::new();
                // Hold onto a capability for each `transaction_id`.  This represents the time at which we'll emit
                // matched data rows.  This will be dropped when we've matched the indicated number of events.
                let mut tx_cap_event_count: HashMap<String, (Capability<_>, i64)> = HashMap::new();
                move |_, _| {
                    // TODO(#11669) Revisit error handling strategy to do something optimized than just emitting
                    // everything we can and holding back the frontier to the first data error.
                    move |tx_metadata_input, data_input, output| {
                        while let Some((tx_metadata_cap, refmut_data)) = tx_metadata_input.next() {
                            refmut_data.swap(&mut tx_data);
                            let tx_metadata_cap = tx_metadata_cap.retain();
                            for (row, time, diff) in tx_data.drain(..) {
                                if diff != 1 {
                                    output.session(&tx_metadata_cap).give((
                                        Err(DataflowError::EvalError(EvalError::Internal(
                                            format!("Transaction metadata supplied diff value {:?}", diff),
                                        ))),
                                        time,
                                        1,
                                    ));
                                }

                                let status = row.iter().nth(tx_status_idx).unwrap().unwrap_str();
                                if status != "END" {
                                    continue;
                                }

                                let tx_id = row.iter().nth(tx_transaction_id_idx).unwrap().unwrap_str().to_owned();

                                let event_count = match row.iter().nth(tx_data_collections_idx).unwrap() {
                                    Datum::List(dl) => dl,
                                    Datum::Array(dl) => dl.elements(),
                                    _ => panic!("data_collections previously validated to be array or list"),
                                }.iter()
                                .find(|datum| datum.unwrap_list().iter().nth(tx_data_collections_data_collection_idx).unwrap().unwrap_str() == &tx_data_collection_name)
                                .map(|datum| match datum.unwrap_list().iter().nth(tx_data_collections_event_count_idx).unwrap() {
                                    Datum::Int16(i) => i.into(),
                                    Datum::Int32(i) => i.into(),
                                    Datum::Int64(i) => i,
                                    Datum::Null => 0,
                                    d => panic!("event_count field previously validated to be integer type.  Found {:?}", d),
                                }).unwrap_or(0);

                                // It's okay for event_count to equal zero here!  This occurs when there is transaction
                                // metadata for other collections but not this particular one.  If that happens, let's
                                // just move on with our lives.
                                //
                                // We do have panics / unwraps above though because we still ought to validate that the
                                // shape of the data is correct -- or else we might be unintentionally missing data!
                                if event_count == 0 {
                                    continue;
                                }

                                match tx_mapping.insert(tx_id.clone(), time) {
                                    None => {
                                        tx_cap_event_count
                                            .insert(tx_id.clone(), (tx_metadata_cap.clone(), event_count));
                                    }
                                    Some(val) if val == time => {},
                                    Some(val) => panic!("unexpected mismatch in duplicate END record for {:?}: {:?} vs {:?}", tx_id, time, val),
                                }
                            }
                        }

                        while let Some((data_cap, refmut_data)) = data_input.next() {
                            // RefOrMut doesn't let us drain directly into iterator
                            refmut_data.swap(&mut data);
                            let data_cap = data_cap.retain();
                            data_buffer.extend(data.drain(..).map(|r| (r, data_cap.clone())));
                        }
                        while let Some((result, data_cap)) = data_buffer.pop_front() {
                            let value = match result.value.clone() {
                                Some(Ok((value, 1))) => value,
                                Some(Ok(_)) => unreachable!(
                                    "Debezium should only be used with sources with no explicit diff"
                                ),
                                Some(Err(err)) => {
                                    output.session(&data_cap).give((
                                        Err(err.into()),
                                        *data_cap.time(),
                                        1,
                                    ));
                                    continue;
                                }
                                None => continue,
                            };

                            let tx_id_and_time = match value.iter().nth(data_transaction_idx).unwrap() {
                                Datum::List(l) => {
                                    let tx_id = l.iter().nth(data_transaction_id_idx).unwrap().unwrap_str().to_owned();
                                    let tx_time: Timestamp = match tx_mapping.get(&tx_id) {
                                        Some(time) => *time,
                                        None => {
                                            data_buffer.push_front((result, data_cap));
                                            break;
                                        },
                                    };
                                    Some((tx_id, tx_time))
                                },
                                Datum::Null => None,
                                _ => panic!("Previously validated to be nullable list"),
                            };

                            let partition_dedup = dedup_state
                                .entry(result.partition.clone())
                                .or_insert_with(|| {
                                    DebeziumDeduplicationState::new(envelope.clone())
                                });
                            let should_use = match partition_dedup {
                                Some(ref mut s) => {
                                    let res = s.should_use_record(
                                        &value,
                                    );
                                    match res {
                                        Ok(b) => b,
                                        Err(err) => {
                                            // We could theoretically use tx_cap_map.get(tx_id) here but for sake of
                                            // consistency, we output all errors at the data_cap time.
                                            output.session(&data_cap).give((
                                                Err(DataflowError::EnvelopeError(err)),
                                                *data_cap.time(),
                                                1,
                                            ));
                                            continue;
                                        }
                                    }
                                }
                                None => true,
                            };

                            match (should_use, tx_id_and_time) {
                                (true, Some((tx_id, tx_time))) => {
                                    let mut tx_cap_event_count_entry = match tx_cap_event_count.entry(tx_id.clone()) {
                                        Entry::Occupied(e) => e,
                                        Entry::Vacant(_) => panic!("Must have cap if using record"),
                                    };

                                    let mut session = output.session(&tx_cap_event_count_entry.get().0);
                                    match value.iter().nth(before_idx).unwrap() {
                                        Datum::List(l) => {
                                            session.give((Ok(Row::pack(&l)), tx_time, -1));
                                        }
                                        Datum::Null => {}
                                        d => {
                                            panic!("type error: expected record, found {:?}", d)
                                        }
                                    }
                                    match value.iter().nth(after_idx).unwrap() {
                                        Datum::List(l) => {
                                            session.give((Ok(Row::pack(&l)), tx_time, 1));
                                        }
                                        Datum::Null => {}
                                        d => {
                                            panic!("type error: expected record, found {:?}", d)
                                        }
                                    }

                                    let (_, ref mut count) = tx_cap_event_count_entry.get_mut();
                                    *count -= 1;
                                    if *count == 0 {
                                        // Must drop the capability to allow the output frontier to progress
                                        let _ = tx_cap_event_count_entry.remove_entry();
                                    }
                                },
                                // The data row has no "transaction" field so it was created before transaction metadata
                                // production was turned on.  Importantly, it is _not_ that the tx_id on the data could
                                // not be matched to a tx metadata row.
                                (true, None) => {
                                    let data_time = *data_cap.time();
                                    let mut session = output.session(&data_cap);
                                    match value.iter().nth(before_idx).unwrap() {
                                        Datum::List(l) => {
                                            session.give((Ok(Row::pack(&l)), data_time, -1));
                                        }
                                        Datum::Null => {}
                                        d => {
                                            panic!("type error: expected record, found {:?}", d)
                                        }
                                    }
                                    match value.iter().nth(after_idx).unwrap() {
                                        Datum::List(l) => {
                                            session.give((Ok(Row::pack(&l)), data_time, 1));
                                        }
                                        Datum::Null => {}
                                        d => {
                                            panic!("type error: expected record, found {:?}", d)
                                        }
                                    }
                                },
                                (false, _) => (),
                            }
                        }
                    }
                }
            },
        )
        .ok_err(|(res, time, diff)| match res {
            Ok(v) => Ok((v, time, diff)),
            Err(e) => Err((e, time, diff)),
        })
}

/// Track whether or not we should skip a specific debezium message
///
/// The goal of deduplication is to omit sending true duplicates -- the exact
/// same record being sent into materialize twice. That means that we create
/// one deduplicator per timely worker and use use timely key sharding
/// normally. But it also means that no single deduplicator knows the
/// highest-ever seen binlog offset.
#[derive(Debug)]
struct DebeziumDeduplicationState {
    /// Last recorded binlog position.
    ///
    /// [`DebeziumEnvelope`] determines whether messages that are not ahead
    /// of the last recorded position will be skipped.
    last_position: Option<RowCoordinates>,
    messages_processed: MzOffset,
    // TODO(petrosagg): This is only used when unpacking MySQL row coordinates. The logic was
    // transferred as-is from the previous avro-debezium code. Find a better place to put this or
    // avoid it completely.
    filenames_to_indices: HashMap<String, i64>,
    projection: DebeziumDedupProjection,
}

/// See <https://rusanu.com/2012/01/17/what-is-an-lsn-log-sequence-number/>
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct SqlServerLsn {
    file_seq_num: u32,
    log_block_offset: u32,
    slot_num: u16,
}

impl FromStr for SqlServerLsn {
    type Err = EnvelopeError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let make_err = || EnvelopeError::Debezium(format!("invalid lsn: {}", input));
        // SQL Server change LSNs are 10-byte integers. Debezium
        // encodes them as hex, in the following format: xxxxxxxx:xxxxxxxx:xxxx
        if input.len() != 22 {
            return Err(make_err());
        }
        if input.as_bytes()[8] != b':' || input.as_bytes()[17] != b':' {
            return Err(make_err());
        }
        let file_seq_num = u32::from_str_radix(&input[0..8], 16).or_else(|_| Err(make_err()))?;
        let log_block_offset =
            u32::from_str_radix(&input[9..17], 16).or_else(|_| Err(make_err()))?;
        let slot_num = u16::from_str_radix(&input[18..22], 16).or_else(|_| Err(make_err()))?;

        Ok(Self {
            file_seq_num,
            log_block_offset,
            slot_num,
        })
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum RowCoordinates {
    MySql {
        file: i64,
        pos: i64,
        row: i32,
    },
    Postgres {
        last_commit_lsn: Option<u64>,
        lsn: i64,
        op_type: PostgresOpCoordinates,
    },
    SqlServer {
        change_lsn: SqlServerLsn,
        event_serial_no: i64,
    },
}

/// The coordinates of a PostgreSQL operation type.
///
/// Updates to the primary key of a PostgreSQL table will result in a delete
/// operation followed by a create operation at the same sequence number, rather
/// than a single update operation. PostgreSQL row coordinates therefore need
/// to encode that expected order by ensuring delete operations sort before
/// create operations at the same sequence number.
///
/// See: <https://debezium.io/documentation/reference/stable/connectors/postgresql.html#postgresql-primary-key-updates>
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum PostgresOpCoordinates {
    // The order here is load bearing. See documentation comment above!
    Delete,
    Create,
    Other,
}

impl DebeziumDeduplicationState {
    fn new(envelope: DebeziumEnvelope) -> Option<Self> {
        Some(DebeziumDeduplicationState {
            last_position: None,
            messages_processed: 0.into(),
            filenames_to_indices: HashMap::new(),
            projection: envelope.dedup,
        })
    }

    fn extract_binlog_position(
        &mut self,
        value: &Row,
    ) -> Result<Option<RowCoordinates>, EnvelopeError> {
        match value.iter().nth(self.projection.source_idx).unwrap() {
            Datum::List(source) => {
                // While reading a snapshot the row coordinates are useless, so early return None
                match source.iter().nth(self.projection.snapshot_idx).unwrap() {
                    Datum::String(s) if s != "false" => return Ok(None),
                    Datum::True => return Ok(None),
                    _ => {}
                }

                let coords = match self.projection.source_projection {
                    DebeziumSourceProjection::MySql { file, pos, row } => {
                        let filename = match source.iter().nth(file).unwrap() {
                            Datum::String(s) => s,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected text, found {:?}", d),
                        };

                        let file = match self.filenames_to_indices.get(filename) {
                            Some(idx) => *idx,
                            None => {
                                let next_idx = self.filenames_to_indices.len() as i64;
                                self.filenames_to_indices
                                    .insert(filename.to_owned(), next_idx);
                                next_idx
                            }
                        };
                        let pos = match source.iter().nth(pos).unwrap() {
                            Datum::Int64(s) => s,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected bigint, found {:?}", d),
                        };
                        let row = match source.iter().nth(row).unwrap() {
                            Datum::Int32(s) => s,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected int, found {:?}", d),
                        };

                        RowCoordinates::MySql { file, pos, row }
                    }
                    DebeziumSourceProjection::Postgres { sequence, lsn } => {
                        let last_commit_lsn = {
                            let sequence = match source.iter().nth(sequence).unwrap() {
                                Datum::String(s) => s,
                                Datum::Null => return Ok(None),
                                d => panic!("type error: expected text, found {:?}", d),
                            };
                            let make_err = || {
                                EnvelopeError::Debezium(format!("invalid sequence: {:?}", sequence))
                            };
                            let sequence: Vec<Option<&str>> =
                                serde_json::from_str(sequence).or_else(|_| Err(make_err()))?;

                            match sequence.first().ok_or_else(make_err)? {
                                Some(s) => Some(u64::from_str(s).or_else(|_| Err(make_err()))?),
                                None => None,
                            }
                        };
                        // NOTE: The second entry of the `sequence` field is
                        // meant to be the LSN, but due to a bug in Debezium
                        // 1.5-1.7 it is actually the same value as
                        // `last_commit_lsn` [0]. The top-level `lsn` field is
                        // correct even in Debezium 1.5, however, so we use
                        // that.
                        //
                        // [0]: https://github.com/debezium/debezium/pull/2563
                        let lsn = match source.iter().nth(lsn).unwrap() {
                            Datum::Int64(s) => s,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected bigint, found {:?}", d),
                        };
                        let op_type = match value
                            .iter()
                            .nth(self.projection.op_idx)
                            .unwrap()
                            .unwrap_str()
                        {
                            "d" => PostgresOpCoordinates::Delete,
                            "c" => PostgresOpCoordinates::Create,
                            _ => PostgresOpCoordinates::Other,
                        };
                        RowCoordinates::Postgres {
                            last_commit_lsn,
                            lsn,
                            op_type,
                        }
                    }
                    DebeziumSourceProjection::SqlServer {
                        change_lsn,
                        event_serial_no,
                    } => {
                        let change_lsn = match source.iter().nth(change_lsn).unwrap() {
                            Datum::String(s) => s.parse::<SqlServerLsn>()?,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected text, found {:?}", d),
                        };
                        let event_serial_no = match source.iter().nth(event_serial_no).unwrap() {
                            Datum::Int64(s) => s,
                            Datum::Null => return Ok(None),
                            d => panic!("type error: expected bigint, found {:?}", d),
                        };

                        RowCoordinates::SqlServer {
                            change_lsn,
                            event_serial_no,
                        }
                    }
                };
                Ok(Some(coords))
            }
            Datum::Null => Ok(None),
            d => panic!("type error: expected record, found {:?}", d),
        }
    }

    fn should_use_record(&mut self, value: &Row) -> Result<bool, EnvelopeError> {
        let binlog_position = self.extract_binlog_position(value)?;

        self.messages_processed += 1;

        // If in the initial snapshot, binlog position is meaningless for detecting
        // duplicates, since it is always the same.
        match &binlog_position {
            None => Ok(true),
            Some(position) => match &mut self.last_position {
                Some(old_position) => {
                    if position > old_position {
                        *old_position = position.clone();
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
                None => {
                    self.last_position = Some(position.clone());
                    Ok(true)
                }
            },
        }
    }
}
