import { createHash } from 'node:crypto';
import type { IncomingMessage } from 'node:http';
import type { Duplex } from 'node:stream';
import type { AgentHello } from '@ankorcs/protocol';
import { bearerNodeToken } from '@ankorcs/protocol/helpers';
// #region example
import { WebSocket } from 'ws';
// #endregion
import { stopServer } from './stop.ts';

export function upgrade(request: IncomingMessage, socket: Duplex, head: Buffer): void {
  const token = bearerNodeToken(request.headers.authorization);
  const node = token === null ? null : findNodeByTokenHash(createHash('sha256').update(token).digest('hex'));

  if (node === null) {
    return;
  }

  // #region example

  // In the upgrade handler from Accept a node, replacing its last line. Keep one socket per node:
  // close a second one while the first is open. The agent retries until it gets through.
  sockets.handleUpgrade(request, socket, head, (ws) => {
    if (node.ws?.readyState === WebSocket.OPEN) {
      ws.close(1008, 'node_connected');

      return;
    }

    node.ws = ws;
    ws.on('close', () => {
      if (node.ws === ws) {
        node.ws = undefined;
      }
    });
    run(node, ws);
  });
  // #endregion
}

export function afterWelcome(node: NodeRecord, hello: AgentHello): void {
  // #region example

  // Inside run(node, ws), right after the welcome. Requests can be sent from here on.
  reconcile(node, hello);
  // #endregion
}

// #region example

// After a reconnect, compare the `hello` with your records, as Disconnects and restarts in
// A minimal controller explains.
function reconcile(node: NodeRecord, hello: AgentHello): void {
  const held = new Map(hello.slot_status.flatMap((slot) => (slot.allocation_id === null ? [] : [[slot.allocation_id, slot] as const])));

  for (const exit of hello.server_exits) {
    // Your code: record that the server ended, and why.
    endLaunch(exit.allocation_id, exit.reason);
  }

  for (const [allocationId, slot] of held) {
    // Your code: check whether you stored this launch for this node.
    if (!hasLaunch(node, allocationId)) {
      // A server you have no record of. Stop it.
      void stopServer(node, allocationId);
    } else if (slot.state === 'running') {
      // This includes a launch whose reply never came. No `slot_status` repeats what the `hello`
      // already says.
      markRunning(allocationId, slot.slot_id);
    }
  }

  // Your code: list the launches you stored for this node that have not ended.
  for (const allocationId of openLaunches(node)) {
    if (!held.has(allocationId)) {
      // No longer on the node. The `hello` lists only the oldest exits, so a `server_exited` after
      // the welcome may still give the reason.
      endLaunch(allocationId, null);
    }
  }
}
// #endregion
