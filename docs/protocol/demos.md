---
title: Demo uploads
description: How nodes post finished demos to the configured target.
slug: protocol/demos
---

When a match records a demo, the node can upload it to any HTTP endpoint that accepts a `POST`.
The endpoint can be your controller, a storage proxy or anything else. The target is set per node,
as [config:demo_upload] in `agent.yml` or `config.demo_upload` in [agent:set_config]. Changing the
target does not interrupt an upload in progress.

Uploads are optional. Without a target, demos stay on the node until [agent:delete_demo] removes
them.

Recording starts `tv_delay` seconds after the match goes live. GOTV shows play that delay late, so
the demo opens on the first live round. With `gotv` null, the delay is 0 and recording starts as
the match goes live.

## Request

`POST <url>` with the raw `.dem` file as the body, `Content-Type: application/octet-stream` and
`Content-Length`.

| Header                | Value                                                                                                                                                                |
|-----------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `Ankor-Protocol`      | The [protocol version](index.md#versions).                                                                                                                           |
| `Ankor-File-Name`     | The file name, `<yyyyMMdd-HHmmss>_<matchId>_<map>.dem`. The time is the recording's start in UTC. The match id is empty for a match launched without one.            |
| `Ankor-Match-Id`      | The match id. Absent for a match launched without one.                                                                                                               |
| `Ankor-Allocation-Id` | The `allocation_id` of the server that recorded it.                                                                                                                  |
| configured header     | The target's `header`, if set. It can have any name that your target expects, such as `Authorization: Basic …`. It is the only credential, and the URL carries none. |

The configured header cannot take a name that the agent sets itself or that frames the request.
`RESERVED_DEMO_HEADERS` in `@ankorcs/protocol` lists those names. The agent's own headers are the
ones sent. The node token is never sent to a demo target.

The node reports the header's value only as `value_sha256`. When you send a header back in
[agent:set_config] without `value`, the node keeps the value it has. Send `value` only to change
it.

## Answers

| Status        | The node                                                                                                                                                                                                                                                                                                                                                                                      |
|---------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `2xx`         | Deletes the file. A file it cannot delete stays, with the error in the node log, and is not uploaded again until the target changes or the agent restarts.                                                                                                                                                                                                                                    |
| `401`, `403`  | Keeps every waiting file and stops uploading until a new demo is recorded, the target changes or the agent restarts.                                                                                                                                                                                                                                                                          |
| `422`         | Keeps the file and skips it until the target changes or the agent restarts. Continues with the next file.                                                                                                                                                                                                                                                                                     |
| anything else | Keeps every waiting demo and retries oldest first after [limit-time:DEMO_RETRY_MIN_MS], doubling to [limit-time:DEMO_RETRY_MAX_MS]. After [limit-time:DEMO_OUTAGE_LIMIT_MS] of failures, keeps them until another demo is recorded, the target changes or the agent restarts. Treats a network failure, or an upload that sends nothing for [limit-time:DEMO_STALL_TIMEOUT_MS], the same way. |

The node ignores the response body. A demo that waits after a failed upload carries the reason in
its `upload_error`.

The target must accept bodies of a few hundred megabytes, the typical size of a full match's demo.
If a storage service needs signed requests, such as S3 with presigned URLs, put a small proxy in
front of it that adds the signatures.
