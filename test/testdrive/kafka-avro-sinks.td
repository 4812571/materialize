# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test Avro Sinks in general. This tests things that are not specific to an
# envelope. Mostly that we can correctly encode various data types and how we
# determine field names. This uses ENVELOPE DEBEZIUM implicitly but the tested
# behavior is not specific to DEBEZIUM sinks.

# Test that we invent field names for unnamed columns.

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}');

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

# Test interval type.

> CREATE MATERIALIZED VIEW interval_data (interval) AS VALUES
  (INTERVAL '0s'),
  (INTERVAL '1month 1day 1us'),
  (INTERVAL '-1month -1day -1us'),
  (INTERVAL '-178000000 years'),
  (INTERVAL '178000000 years')

> CREATE SINK interval_data_sink FROM interval_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-interval-data-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.interval_data_sink sort-messages=true
{"before": null, "after": {"row": {"interval": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]}}}
{"before": null, "after": {"row": {"interval": [0, 198, 80, 127, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]}}}
{"before": null, "after": {"row": {"interval": [0, 58, 175, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]}}}
{"before": null, "after": {"row": {"interval": [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]}}}
{"before": null, "after": {"row": {"interval": [255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]}}}

# See #9723
#> CREATE MATERIALIZED VIEW unnamed_cols AS SELECT 1, 2 AS b, 3;
#
#> CREATE SINK unnamed_cols_sink FROM unnamed_cols
#  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-unnamed-cols-sink-${testdrive.seed}')
#  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
#
#$ kafka-verify-data format=avro sink=materialize.public.unnamed_cols_sink
#{"before": null, "after": {"row": {"column1": 1, "b": 2, "column3": 3}}}

# Test that invented field names do not clash with named columns.

# See #9723
#> CREATE MATERIALIZED VIEW clashing_cols AS SELECT 1, 2 AS column1, 3 as b, 4 as b2, 5 as b3;
#
#> CREATE SINK clashing_cols_sink FROM clashing_cols
#  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-clashing-cols-sink-${testdrive.seed}')
#  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
#
#$ kafka-verify-data format=avro sink=materialize.public.clashing_cols_sink
#{"before": null, "after": {"row": {"column1": 1, "column1_1": 2, "b": 3, "b2": 4, "b3": 5}}}

# Test date/time types.



> CREATE MATERIALIZED VIEW datetime_data (date, ts, ts_tz) AS VALUES
  (DATE '2000-01-01', TIMESTAMP '2000-01-01 10:10:10.111', TIMESTAMPTZ '2000-01-01 10:10:10.111+02'),
  (DATE '2000-02-01', TIMESTAMP '2000-02-01 10:10:10.111', TIMESTAMPTZ '2000-02-01 10:10:10.111+02'),
  (('0001-01-01'::DATE - '1721389days'::INTERVAL)::DATE, ('0001-01-01'::DATE - '1721389days'::INTERVAL)::TIMESTAMP, ('0001-01-01'::DATE - '1721389days'::INTERVAL)::TIMESTAMPTZ),
  (('0001-01-01'::DATE + '262142years 11months 30days'::INTERVAL)::DATE, ('0001-01-01'::DATE + '262142years 11months 30days'::INTERVAL)::TIMESTAMP, ('0001-01-01'::DATE + '262142years 11months 30days'::INTERVAL)::TIMESTAMPTZ)

> CREATE SINK datetime_data_sink FROM datetime_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-datetime-data-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.datetime_data_sink sort-messages=true
{"before": null, "after": {"row": {"date": -2440551, "ts": -210863606400000000, "ts_tz": -210863606400000000}}}
{"before": null, "after": {"row": {"date": 10957, "ts": 946721410111000, "ts_tz": 946714210111000}}}
{"before": null, "after": {"row": {"date": 10988, "ts": 949399810111000, "ts_tz": 949392610111000}}}
{"before": null, "after": {"row": {"date": 95026601, "ts": 8210298326400000000, "ts_tz": 8210298326400000000}}}

> CREATE MATERIALIZED VIEW time_data (time) AS VALUES (TIME '01:02:03'), (TIME '01:02:04'), (TIME '00:00:00'), (TIME '23:59:59')

> CREATE SINK time_data_sink FROM time_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-time-data-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.time_data_sink sort-messages=true
{"before": null, "after": {"row": {"time": 0}}}
{"before": null, "after": {"row": {"time": 3723000000}}}
{"before": null, "after": {"row": {"time": 3724000000}}}
{"before": null, "after": {"row": {"time": 86399000000}}}

# Test jsonb

> CREATE MATERIALIZED VIEW json_data (a, b) AS VALUES ('{"a":1, "b":2}'::jsonb, 2)

# Sinks with JSON columns should not crash - see https://github.com/MaterializeInc/materialize/issues/4722
> CREATE SINK json_data_sink FROM json_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-json-data-sink-${testdrive.seed}')
  FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

# Test map

> CREATE MATERIALIZED VIEW map_data (map) AS SELECT '{a => 1, b => 2}'::map[text=>int];

> CREATE SINK map_sink FROM map_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-map-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.map_sink sort-messages=true
{"before": null, "after": {"row": {"map": {"a": {"int": 1}, "b": {"int": 2}}}}}

> CREATE MATERIALIZED VIEW list_data (list) AS SELECT LIST[1, 2];

> CREATE SINK list_sink FROM list_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-list-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.list_sink sort-messages=true
{"before": null, "after": {"row": {"list": [{"int": 1}, {"int": 2}]}}}

# Test optional namespace for auto-generated value schema
> CREATE MATERIALIZED VIEW namespace_value_data (namespace) AS SELECT 1;

> CREATE SINK namespace_value_sink FROM namespace_value_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-namespace-value-sink-${testdrive.seed}')
  FORMAT AVRO USING
    CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO VALUE FULLNAME = 'abc.def.ghi')
  ENVELOPE DEBEZIUM

$ schema-registry-verify schema-type=avro subject=testdrive-namespace-value-sink-${testdrive.seed}-value
{"type":"record","name":"ghi","namespace":"abc.def","fields":[{"name":"before","type":["null",{"type":"record","name":"row","fields":[{"name":"namespace","type":"int"}]}]},{"name":"after","type":["null","row"]}]}

$ kafka-verify-data format=avro sink=materialize.public.namespace_value_sink sort-messages=true
{"before": null, "after": {"row": {"namespace": 1}}}

# Test optional namespaces for autogenerated key and value schemas
> CREATE MATERIALIZED VIEW namespace_key_value_data (a, b) AS SELECT * FROM (VALUES (1, 2));

> CREATE SINK namespace_key_value_sink FROM namespace_key_value_data
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-namespace-key-value-sink-${testdrive.seed}')
  KEY (b)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO KEY FULLNAME = 'some.neat.class.foo', AVRO VALUE FULLNAME = 'some.neat.class.bar')
  ENVELOPE DEBEZIUM

$ schema-registry-verify schema-type=avro subject=testdrive-namespace-key-value-sink-${testdrive.seed}-key
{"type":"record","name":"foo","namespace":"some.neat.class","fields":[{"name":"b","type":"int"}]}

$ schema-registry-verify schema-type=avro subject=testdrive-namespace-key-value-sink-${testdrive.seed}-value
{"type":"record","name":"bar","namespace":"some.neat.class","fields":[{"name":"before","type":["null",{"type":"record","name":"row","fields":[{"name":"a","type":"int"},{"name":"b","type":"int"}]}]},{"name":"after","type":["null","row"]}]}

$ kafka-verify-data format=avro sink=materialize.public.namespace_key_value_sink sort-messages=true
{"b": 2} {"before": null, "after": {"row": {"a": 1, "b": 2}}}

# Bad Sinks

> CREATE MATERIALIZED VIEW input (a, b) AS SELECT * FROM (VALUES (1, 2))

! CREATE SINK bad_sink FROM input
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-input-sink-${testdrive.seed}') KEY (a, a)
  FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
contains:Repeated column name in sink key: a

! CREATE SINK bad_sink FROM input
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-input-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO VALUE FULLNAME = 'some.neat.class.foo', AVRO KEY FULLNAME = 'some.neat.class.bar')
  ENVELOPE DEBEZIUM
contains:Cannot specify AVRO KEY FULLNAME without a corresponding KEY field

! CREATE SINK bad_sink FROM input
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-input-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO KEY FULLNAME = 'some.neat.class.bar')
  ENVELOPE DEBEZIUM
contains:Cannot specify AVRO KEY FULLNAME without a corresponding KEY field

! CREATE SINK bad_sink FROM input
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-input-sink-${testdrive.seed}') KEY (a)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO KEY FULLNAME = 'some.neat.class.bar')
  ENVELOPE DEBEZIUM
contains:Must specify both AVRO KEY FULLNAME and AVRO VALUE FULLNAME when specifying generated schema names

! CREATE SINK bad_sink FROM input
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-input-sink-${testdrive.seed}') KEY (a)
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn (AVRO VALUE FULLNAME = 'some.neat.class.bar')
  ENVELOPE DEBEZIUM
contains:Must specify both AVRO KEY FULLNAME and AVRO VALUE FULLNAME when specifying generated schema names
