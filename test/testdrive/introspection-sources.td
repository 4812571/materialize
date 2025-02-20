# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test for a subset of the information returned by introspection sources.
# The test focuses on computing answers that are independent of particular
# timing measurement data, even if these stable answers may take some time
# to appear in the results of queries to introspection sources.

# Note that we count on the retry behavior of testdrive in this test
# since introspection sources may take some time to catch up.

# The contents of the introspection tables depend on the replica size
$ skip-if
SELECT '${arg.default-replica-size}' != '4-4'

# In case the environment has other replicas
> SET cluster_replica = r1

> CREATE TABLE t (a int)

> CREATE MATERIALIZED VIEW mv AS SELECT * FROM t

> SELECT count_all > 0
  FROM (SELECT SUM(count) as count_all
        FROM
            mz_materialized_views AS views,
            mz_internal.mz_compute_exports as compute_exports,
            mz_internal.mz_raw_worker_compute_delays AS delays
        WHERE
            views.name = 'mv' AND
            views.id = compute_exports.export_id AND
            compute_exports.export_id = delays.export_id)
true

> SELECT COUNT(*)
  FROM
    mz_materialized_views AS views,
    mz_internal.mz_compute_exports AS compute_exports,
    mz_internal.mz_compute_import_frontiers AS import_frontiers
  WHERE
    views.name = 'mv' AND
    views.id = compute_exports.export_id AND
    compute_exports.export_id = import_frontiers.export_id AND
    time > 0
16

> CREATE VIEW vv AS SELECT * FROM t

> SELECT COUNT(*)
  FROM (SELECT DISTINCT delays.export_id, delays.import_id
        FROM
            mz_internal.mz_raw_worker_compute_delays AS delays)
1

> SELECT COUNT(*)
  FROM (SELECT DISTINCT import_frontiers.export_id, import_frontiers.import_id
        FROM
            mz_internal.mz_worker_compute_import_frontiers AS import_frontiers)
1

> CREATE DEFAULT INDEX ON vv

> SELECT COUNT(*)
  FROM (SELECT DISTINCT delays.export_id, delays.import_id
        FROM
            mz_internal.mz_raw_worker_compute_delays AS delays)
2

> SELECT COUNT(*)
  FROM (SELECT DISTINCT import_frontiers.export_id, import_frontiers.import_id
        FROM
            mz_internal.mz_worker_compute_import_frontiers AS import_frontiers)
2

> SELECT count_all > 0
  FROM (SELECT SUM(count) as count_all
        FROM
            mz_views AS views,
            mz_indexes AS indexes,
            mz_internal.mz_compute_exports AS compute_exports,
            mz_internal.mz_raw_worker_compute_delays AS delays
        WHERE
            views.name = 'vv' AND
            views.id = indexes.on_id AND
            indexes.id = compute_exports.export_id AND
            compute_exports.export_id = delays.export_id)
true

> SELECT COUNT(*)
  FROM
    mz_views AS views,
    mz_indexes AS indexes,
   mz_internal. mz_compute_exports compute_exports,
    mz_internal.mz_compute_import_frontiers AS import_frontiers
  WHERE
    views.name = 'vv' AND
    views.id = indexes.on_id AND
    indexes.id = compute_exports.export_id AND
    compute_exports.export_id = import_frontiers.export_id AND
    time > 0
16

> DROP INDEX vv_primary_idx

> SELECT COUNT(*)
  FROM (SELECT DISTINCT delays.export_id, delays.import_id
        FROM
            mz_internal.mz_raw_worker_compute_delays AS delays)
1

> SELECT COUNT(*)
  FROM (SELECT DISTINCT import_frontiers.export_id, import_frontiers.import_id
        FROM
            mz_internal.mz_worker_compute_import_frontiers AS import_frontiers)
1

> DROP MATERIALIZED VIEW mv

> SELECT COUNT(*)
  FROM (SELECT DISTINCT delays.export_id, delays.import_id
        FROM
            mz_internal.mz_raw_worker_compute_delays AS delays)
0

> SELECT COUNT(*)
  FROM (SELECT DISTINCT import_frontiers.export_id, import_frontiers.import_id
        FROM
            mz_internal.mz_worker_compute_import_frontiers AS import_frontiers)
0

# Test that frontiers of introspection sources advance at all.

! SELECT * FROM mz_internal.mz_active_peeks AS OF 0
contains: Timestamp (0) is not valid for all inputs
