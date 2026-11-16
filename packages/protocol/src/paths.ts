import type { ControllerSetConfig } from './agent-socket.js';
import type { EventBatch, EventBatchReceipt, EventBatchRejection } from './event-batch.js';

/**
 * The wire's version: the `major.minor` of this package. Every request a node makes to the
 * controller, its demo uploads, the node-setup request and the plugin's connection to the agent
 * carry it in {@link PROTOCOL_HEADER}. A peer whose version is not compatible answers
 * `426 Upgrade Required`, and no frame is exchanged. [Versioning](/protocol/versioning/) says
 * which versions are.
 */
export const PROTOCOL_VERSION = '0.10';

/**
 * The header that carries {@link PROTOCOL_VERSION} on every request a node, its plugin or the setup
 * script makes.
 */
export const PROTOCOL_HEADER = 'Ankor-Protocol';


/**
 * The agent socket's default path, which the agent dials with `Authorization: Bearer <node token>`.
 * `controller.socket_path` in `agent.yml` overrides it.
 */
export const DEFAULT_SOCKET_PATH = '/ws/agent';

/**
 * The default path the agent posts an {@link EventBatch} to, with its bearer token.
 *
 * The controller answers `200` with an {@link EventBatchReceipt} once every frame is stored or
 * quarantined, or `409` with an {@link EventBatchRejection}. Any other answer keeps the frames on
 * the node for a retry. `controller.events_path` in `agent.yml` overrides the path.
 */
export const DEFAULT_EVENTS_PATH = '/api/node/events';

/**
 * The name of a demo file, `<yyyyMMdd-HHmmss>_<matchId>_<map>.dem`. `<matchId>` is empty for a
 * match without a `match_id`.
 */
export const DEMO_NAME = /^(\d{8}-\d{6})_([A-Za-z0-9-]*)_([A-Za-z0-9_-]+)\.dem$/;

/**
 * The headers a demo upload carries besides {@link PROTOCOL_HEADER} and the target's configured
 * header.
 */
export const DEMO_HEADERS = {
  fileName: 'Ankor-File-Name',
  /** The demo's `match_id`. Omitted for a match launched without one. */
  matchId: 'Ankor-Match-Id',
  /** The `allocation_id` of the server that recorded the demo. */
  allocationId: 'Ankor-Allocation-Id',
} as const;

/**
 * The path the setup script `POST`s to for a node's `agent.yml`, with
 * `Authorization: Bearer <node token>`, {@link PROTOCOL_HEADER} and no body.
 *
 * The answer is that node's `agent.yml` as `text/yaml`, and errors stay JSON. An unknown token is
 * refused with `401`. The file's `slots` may be empty, for slots that
 * {@link ControllerSetConfig | set_config} adds once the node connects.
 *
 * The path is the setup script's default, `DEFAULT_SETUP_PATH` in the script. Operators can set
 * another with `--setup-path`.
 */
export const NODE_SETUP_PATH = '/api/node/setup';

/**
 * The path of the node-setup script, which a `GET` returns as the file. ankor.sh serves it for
 * every controller, which operators name with `--controller`.
 */
export const SETUP_SCRIPT_PATH = '/node-setup.sh';

/**
 * The controller the setup script fetches `agent.yml` from when `--controller` is absent. It is
 * `DEFAULT_CONTROLLER_URL` in the script.
 */
export const DEFAULT_CONTROLLER_URL = 'https://ankor.sh';

/**
 * Where the setup script downloads itself from to re-run as root. It is `SETUP_SCRIPT_URL` in the
 * script.
 */
export const SETUP_SCRIPT_URL = `${DEFAULT_CONTROLLER_URL}${SETUP_SCRIPT_PATH}`;

/**
 * The environment variable the setup script reads the node token from instead of prompting for it.
 */
export const NODE_TOKEN_VARIABLE = 'ANKOR_NODE_TOKEN';
