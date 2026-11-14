/**
 * The wire between a node and the controller it dials: the agent socket's messages, the
 * event-batch upload and the gameplay events it carries, the `agent.yml` shape, and the paths,
 * limits and timings. This entry holds schemas and constants only, and it imports nothing from the
 * agent runtime.
 *
 * Further entries stay outside the wire:
 *
 * - `@ankorcs/protocol/helpers`: functions both ends can share.
 * - `@ankorcs/protocol/agent-yml`: the `agent.yml` that a controller renders for a node.
 * - `@ankorcs/protocol/session`: one connected agent, from the controller's side of the socket.
 * - `@ankorcs/protocol/container`: the interface between the agent and its game containers.
 * - `@ankorcs/protocol/node-setup.sh`: the script that installs a game node.
 *
 * A controller that hosts agents serves the agent socket, the event-batch upload and, optionally,
 * the node-setup answer, each at a path that a constant in `paths.ts` gives with its rules.
 * ankor.sh serves the setup script for every controller.
 */

export * from './values.js';

export * from './game-events.js';

export * from './launch.js';

export * from './event-batch.js';

export * from './agent-socket.js';

export * from './agent-config.js';

export * from './paths.js';

export * from './limits.js';
