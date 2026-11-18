import type { NodeSettings } from '@ankorcs/protocol';

// #region example
import { createHash } from 'node:crypto';
import { WebSocketServer, type WebSocket } from 'ws';
import { AGENT_HELLO_TIMEOUT_MS, PROTOCOL_HEADER } from '@ankorcs/protocol';
import { bearerNodeToken, isCompatibleProtocol, protocolVersionOf, upgradeRefusal } from '@ankorcs/protocol/helpers';
import { createAgentSession } from '@ankorcs/protocol/session';

const sockets = new WebSocketServer({ noServer: true });

server.on('upgrade', (request, socket, head) => {
  const token = bearerNodeToken(request.headers.authorization);
  // Your code: find the node's in-memory record by the SHA-256 of its token, or return null.
  const node = token === null ? null : findNodeByTokenHash(createHash('sha256').update(token).digest('hex'));

  if (node === null) {
    socket.end(upgradeRefusal(401));

    return;
  }

  const protocol = request.headers[PROTOCOL_HEADER.toLowerCase()];

  if (!isCompatibleProtocol(protocol)) {
    socket.end(upgradeRefusal(426));

    return;
  }

  // The node's self-reported version, to offer it only the features it has.
  node.protocol = protocolVersionOf(protocol);
  sockets.handleUpgrade(request, socket, head, (ws) => run(node, ws));
});

function run(node: NodeRecord, ws: WebSocket): void {
  const session = createAgentSession({ send: (text) => ws.send(text) });
  let welcomed = false;
  // A socket that sends no valid hello in time is closed, as step 2 describes.
  const helloTimer = setTimeout(() => ws.close(1008, 'hello_timeout'), AGENT_HELLO_TIMEOUT_MS);

  ws.on('message', (raw) => {
    // `ws` hands each text frame over as a Buffer.
    const frame = session.receive(raw.toString());

    switch (frame.kind) {
      case 'report':
        // Every connection starts with a `hello`. Answer it before step 2's deadline.
        if (frame.message.type === 'hello') {
          if (welcomed) {
            // A second `hello` on the same socket breaks the protocol.
            ws.close(1008, 'duplicate_hello');

            break;
          }

          clearTimeout(helloTimer);
          // Your code: store what the `hello` reports: the node's settings, slots, servers and CS2.
          saveHello(node, frame.message);
          session.notify({ type: 'welcome' });
          welcomed = true;
        } else {
          // Your code: handle every other report, as the other tabs show.
          handleReport(node, frame.message);
        }

        break;
      case 'reply':
        // The session has already passed this reply to the waiting `request(…)`. Only a reply
        // to a request that timed out arrives here with no one waiting.
        // Your code: `log` is your logger.
        if (!frame.answered) {
          log.info(`node ${node.id} answered ${frame.message.request} after it timed out`);
        }

        break;
      case 'unsupported':
        // A message type from a newer agent. Log it and skip it.
        log.info(`node ${node.id} sent ${frame.type}, which this protocol version does not have`);

        break;
      case 'invalid':
        log.warn(`node ${node.id} sent a frame that breaks the contract: ${frame.detail}`);

        // Before the welcome, this is the `hello` failing its schema. Refuse it, as step 2
        // describes.
        if (!welcomed) {
          clearTimeout(helloTimer);
          session.notify({ type: 'hello_refused', reason: 'invalid_hello', detail: frame.detail.slice(0, 512) });
          ws.close(1008, 'hello_refused');
        }

        break;
    }
  });
  ws.on('close', () => {
    clearTimeout(helloTimer);
    session.closed();
  });

  node.session = session;
}
// #endregion

type SlotLayout = NodeSettings['slots'];

export async function addSlots(node: NodeRecord, slots: SlotLayout): Promise<void> {
  // #region example
  // Anywhere else, once the node is welcomed: send the node its slots.
  const result = await node.session.request({ type: 'set_config', config: { slots } });

  if (result.status === 'ok') {
    // Your code: store the node's settings, which now include the new slots.
    saveSettings(node, result.data);
  }
  // #endregion
}

