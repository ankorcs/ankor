---
title: Demo uploads
description: How nodes post finished demos to the configured target.
slug: protocol/demos
---

A match can record a demo, and the node uploads each finished demo over HTTP to the upload target
that you set. The target can
be any endpoint that accepts a `POST`, such as your controller or a storage proxy. If you operate
a node, [Demos](../operations.md#demos) explains the target and its upload rate cap instead.

Uploads are optional. Without a target, demos stay on the node until [agent:delete_demo] removes
them.

## Recording and the target

A match records a demo when its [`MatchConfig`](/reference/types/#schema-MatchConfig) has
`record_demo` set to `true`. The server starts recording `tv_delay` seconds after the match goes
live. GOTV shows play that much later, so the demo opens on the first live round. If the launch's
`gotv` is null, the delay is 0 and recording starts as the match goes live.

Set the target per node, as [config:demo_upload] in `agent.yml` or as `config.demo_upload` in
[agent:set_config]. The node uploads a demo once its server has finished recording it. The node
sends one demo at a time, oldest first. If you change the target, the upload in progress continues
to the old target, and the next upload goes to the new one.

Your controller sees the demos on the node through these messages:

- The `demos` array in [agent:hello] lists the newest [limit:MAX_REPORTED_DEMOS] demos.
- A [agent:demo] arrives when a demo appears or changes, for example when its upload starts or
  stops.
- A [agent:demo_progress] reports how far an upload is. The messages of one upload arrive at least
  [limit-time:DEMO_PROGRESS_INTERVAL_MS] apart.
- A [agent:demo_removed] arrives when a demo is no longer on the node, because it was uploaded or
  deleted.

## Request

The node sends `POST <url>` with the raw `.dem` file as the body. It sets
`Content-Type: application/octet-stream`, `Content-Length` and these headers:

| Header                | Value                                                                                                                                            |
|-----------------------|--------------------------------------------------------------------------------------------------------------------------------------------------|
| `Ankor-Protocol`      | The [protocol version](index.md#versions).                                                                                                       |
| `Ankor-File-Name`     | The file name, `<yyyyMMdd-HHmmss>_<matchId>_<map>.dem`. The time is the recording's start in UTC. The match id is empty for a match without one. |
| `Ankor-Match-Id`      | The match id. The header is absent for a match launched without one.                                                                             |
| `Ankor-Allocation-Id` | The `allocation_id` of the server that recorded the demo.                                                                                        |
| configured header     | The `header` of the target, if set. It can have any name that your target expects, such as `Authorization: Basic …`.                             |

The configured header is the only credential. A URL that carries a user name or a password is
rejected, and the node never sends its node token to a demo target. The configured header cannot
take a name that the agent sets itself or that frames the request. `RESERVED_DEMO_HEADERS` in
`@ankorcs/protocol` lists those names. If the configured header has the name of a header that the
agent sets, the agent sends its own value.

The node reports the value of the configured header only as `value_sha256`. When you send the
header back in [agent:set_config] without `value`, the node keeps the value it has. Send `value`
only to change it.

## Answers

The node reads only the status code of the answer and ignores the body. It does not follow
redirects, so a `3xx` counts as any other status:

| Status        | The node                                                                                                                                                                                                                                                                                                                                                                                      |
|---------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `2xx`         | Deletes the file. If it cannot delete the file, it logs the error and keeps the file. It does not upload that file again until the target changes or the agent restarts.                                                                                                                                                                                                                      |
| `401`, `403`  | Keeps every waiting file and stops uploading until a new demo is recorded, the target changes or the agent restarts.                                                                                                                                                                                                                                                                          |
| `422`         | Keeps the file and skips it until the target changes or the agent restarts. Continues with the next file.                                                                                                                                                                                                                                                                                     |
| anything else | Keeps every waiting demo and retries oldest first after [limit-time:DEMO_RETRY_MIN_MS], doubling to [limit-time:DEMO_RETRY_MAX_MS]. After [limit-time:DEMO_OUTAGE_LIMIT_MS] of failures, keeps them until another demo is recorded, the target changes or the agent restarts. Treats a network failure, or an upload that sends nothing for [limit-time:DEMO_STALL_TIMEOUT_MS], the same way. |

When a demo waits after a failed upload, its `upload_error` gives the reason.

Your target must accept bodies of a few hundred megabytes, the typical size of a demo of a full
match. If a storage service needs signed requests, such as S3 with presigned URLs, put a small proxy
in front of it that adds the signatures.
