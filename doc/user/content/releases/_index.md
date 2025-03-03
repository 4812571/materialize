---
title: "Releases"
description: "How Materialize is released"
disable_list: true
menu:
  main:
    parent: 'about'
    weight: 10
---

We are continually improving Materialize with new features and bug fixes. We
periodically release these improvements to your Materialize account. This page
describes the changes in each release and the process by which they are
deployed.

## Release notes

{{< version-list >}}

For versions that predate cloud-native Materialize, see our
historical [release notes](https://materialize.com/docs/lts/release-notes/)
and [documentation](https://materialize.com/docs/lts/).

## Schedule

We release a new version of Materialize approximately every week. Each
version includes both new features and bug fixes.

On weeks with a scheduled release, we deploy the release to Materialize accounts
during the scheduled maintenance window on Wednesday from 4-6pm ET.

We may occasionally deploy unscheduled releases to fix urgent bugs during
unplanned maintenance windows. Due to the unexpected nature of these bugs, we
cannot provide advance notice of these releases.

The deployment of a new release of Materialize causes an interruption in
service. The length of the service interruption is proportional to the size of
your sources, sinks, and indexes. We plan to support zero-downtime deployments
in a future release of Materialize.

We announce both planned and unplanned maintenance windows on our [status
page](https://status.materialize.com).

You can also use our [status page API](https://status.materialize.com/api) to
programmatically access the information on our status page.

## Versioning

Each release is associated with an internal version number. You can determine
what release your Materialize region is running by executing:

```
SELECT mz_version();
```

Scheduled weekly releases increase the middle component of the version number
and reset the final component to zero (e.g., v0.26.2 -> v0.27.0). Unscheduled
releases increase the final component of the version number (e.g., v0.27.0 -> v0.27.1).

## Backwards compatibility

Materialize maintains backwards compatibility whenever possible. You can expect
applications that work with the current version of Materialize to work with all
future versions of Materialize with only minor changes to the application's
code.

Very occasionally, a bug fix may require breaking backwards compatibility. These
changes are approved only after weighing the severity of the bug against the
number of users that will be affected by the backwards-incompatible change.
Backwards-incompatible changes are always clearly marked as such in the [release
notes](#release-notes).

There are several aspects of the product that are not considered part of
Materialize's stable interface:

  * Features that are in alpha (labeled as such in the documentation)
  * The [`EXPLAIN`](/sql/explain) statement
  * Objects in the [`mz_internal` schema](/sql/system-catalog/mz_internal)
  * Any undocumented features or behavior

These unstable interfaces are not subject to the backwards-compatibility policy.
If you choose to use these unstable interfaces, you should be aware of the risk
of backwards-incompatible changes. Backwards-incompatible changes may be made to
these unstable interfaces at any time and without mention in the release notes.
