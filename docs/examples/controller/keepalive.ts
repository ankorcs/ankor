import type { WebSocket } from 'ws';
// #region example
import { AGENT_PING_TIMEOUT_MS } from '@ankorcs/protocol';
// #endregion

export function keepAlive(ws: WebSocket): void {
  // #region example

  // Inside run(node, ws). The agent pings regularly, and `ws` answers each ping by itself. Close
  // the socket when the agent has been silent for too long.
  let heardAt = Date.now();
  const heard = () => {
    heardAt = Date.now();
  };

  ws.on('ping', heard);
  ws.on('message', heard);

  const silence = setInterval(() => {
    if (Date.now() - heardAt > AGENT_PING_TIMEOUT_MS) {
      // The connection is most likely gone, so drop it without a closing handshake.
      ws.terminate();
    }
  }, 5_000);

  ws.on('close', () => clearInterval(silence));
  // #endregion
}
