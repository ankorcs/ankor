---
title: Troubleshooting
description: What to do for common node failures.
slug: troubleshooting
---

Each row below starts from something that a node shows: a line in the agent's
[log](operations.md#logs), a status code or a refusal. Look up the exact text you see.

## Setup and connection

<div class="symptoms">

| Symptom                                                                                                                                    | Next step                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
|--------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Setup stops at `pulling …/ankor-agent`                                                                                                     | Check that the host can reach the image registry, then rerun setup.                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Agent log: the controller at … refused this node's token                                                                                   | The agent has stopped dialling. Restart it once the controller knows the token.                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Agent log: the controller at … uses a protocol version incompatible with this agent's …                                                    | The agent has stopped dialling. [Update it from the host](operations.md#update-the-agent-from-the-host).                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Agent log: the controller at … refused this node's hello                                                                                   | The agent has stopped dialling. Its hello failed the controller's schema ([`invalid_hello`](/reference/types/#schema-HelloRefusedReason)). [Update it from the host](operations.md#update-the-agent-from-the-host).                                                                                                                                                                                                                                                                                                       |
| Node unavailable with code [`event_storage`](/reference/types/#schema-NodeUnavailableCode), `detail` says the event spool has not answered | The disk under `/srv/ankor` hangs. Check the disk, and leave the agent running. Deleting `/srv/ankor/events/` or restarting the agent loses the records that wait to be written. The agent takes launches again once the event spool answers.                                                                                                                                                                                                                                                                             |
| Node unavailable with code [`event_storage`](/reference/types/#schema-NodeUnavailableCode), any other `detail`                             | The agent cannot open its events database, for example one that a downgrade left newer than the agent. The agent log gives the cause. [Update the agent from the host](operations.md#update-the-agent-from-the-host), or delete `/srv/ankor/events/` and restart the agent. Deleting it loses every undelivered event and server exit, including those of running servers. It also loses the record of allocations the node has launched. A running server's later events are stored again from where its plugin resumes. |
| Node unavailable with code [`disk_space`](/reference/types/#schema-NodeUnavailableCode)                                                    | The disk under `/srv/ankor` has less than [limit-size:DISK_SPACE_MIN_BYTES] free. Check it with `df -h /srv/ankor`, then free space. For example, remove [old demos](operations.md#demos), or remove unused Docker images with `docker image prune`. The agent checks every 30 seconds and accepts launches again once there is room.                                                                                                                                                                                     |

</div>

## Game servers

<div class="symptoms">

| Symptom                                        | Next step                                                                                                                        |
|------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------|
| CS2 missing after setup                        | [Install it](operations.md#update-cs2). The agent installs CS2 only when the controller requests it.                             |
| Node refuses launches after a CS2 release      | The node updates CS2 once its running servers end. After a failed automatic attempt, [update](operations.md#update-cs2) by hand. |
| Update refused for free space                  | Grow the disk. [Update CS2](operations.md#update-cs2) lists the thresholds.                                                      |
| Slot receives no launches, login token refused | Steam refused the slot's login token. [Replace it](operations.md#replace-a-slots-token).                                         |
| Missing game files or a mountpoint             | Wait until the node is idle, then update CS2.                                                                                    |
| Stop is unconfirmed                            | Check Docker on the host. The slot stays occupied until Docker reports the container removed.                                    |

</div>
