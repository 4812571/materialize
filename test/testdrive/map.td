# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

> SELECT * FROM mz_map_types;

> SHOW TYPES

> CREATE TYPE bool AS MAP (KEY TYPE = text, VALUE TYPE = int4)

> CREATE TYPE custom AS MAP (KEY TYPE = text, VALUE TYPE = bool)

$ set-regex match=^s\d*$ replacement=<GID>
# Without qualifiers, should default to builtin bool.
> SELECT value_id
  FROM mz_map_types JOIN mz_types ON mz_map_types.id = mz_types.id
  WHERE name = 'custom'
<GID>

> CREATE TYPE another_custom AS MAP (KEY TYPE = text, VALUE TYPE = public.bool)

$ set-regex match=^u\d*$ replacement=<GID>
# Qualified name should point to user-defined bool.
> SELECT value_id
  FROM mz_map_types JOIN mz_types ON mz_map_types.id = mz_types.id
  WHERE name = 'another_custom'
<GID>

> CREATE SCHEMA test_schema

> CREATE TYPE test_schema.bool AS MAP (KEY TYPE = text, VALUE TYPE = float4)

> SHOW TYPES
name
--------------
bool
custom
another_custom

> SHOW TYPES FROM test_schema
name
----
bool

! DROP TYPE bool
contains:cannot drop item pg_catalog.bool because it is required by the database system

! DROP TYPE public.bool
contains:cannot drop materialize.public.bool: still depended upon by catalog item 'materialize.public.another_custom'

> DROP TYPE another_custom

> DROP TYPE public.bool
