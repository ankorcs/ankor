/**
 * The node protocol as one AsyncAPI 3.1 document, from the controller's side: the agent socket,
 * the event upload and the gameplay events it carries, plus the plugin socket inside the node. Every
 * schema comes from this package's build, and every description from the JSDoc beside it in `src/`;
 * this file only arranges them.
 *
 * The document is read outside the docs site, so its links stand on their own. A link to a schema
 * is a reference into the document, such as `#/components/schemas/AgentHello`, and a link to a
 * guide is an absolute address on {@link DOCS_SITE}.
 */
import { readdirSync } from 'node:fs';
import path from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import * as contract from '@ankorcs/protocol';
import * as container from '@ankorcs/protocol/container';
import { z } from 'zod';
import { reachable, schemaComponents, type JsonSchema } from './components.ts';
import { readDocblocks, type DocNode, type SourceDocs } from './docblocks.ts';
import { undescribedRefinements } from './refinements.ts';
import EXAMPLES_JSON from './examples.json' with { type: 'json' };

const SOURCE_DIR = path.join(import.meta.dirname, '..', 'src');

/** Where the guides are published. Docblocks link to them from the site root, such as `/protocol/limits/`. */
export const DOCS_SITE = 'https://docs.ankor.sh';

/** A guide linked from the site root, such as `/protocol/limits/`, at its published address. */
export function absoluteGuides(text: string): string {
  return text.replace(/\]\(\/(?!\/)/g, `](${DOCS_SITE}/`);
}

/** A link target in a description that names a schema of the document. */
export const SCHEMA_LINK = '#/components/schemas/';

/** Example payloads by message name; each is checked against its schema. */
const EXAMPLES: Record<string, unknown[] | undefined> = EXAMPLES_JSON;

type Support = 'required' | 'optional';

/** What a controller must implement for a node to run games. A new message fails the build until it is listed. */
const SUPPORT: Record<string, Support> = {
  hello: 'required',
  node_status: 'required',
  slot_status: 'required',
  server_exited: 'optional',
  reply: 'required',
  shutdown: 'optional',
  log_batch: 'optional',
  log_index: 'optional',
  cs2_update_progress: 'optional',
  resource_batch: 'optional',
  event_backlog: 'optional',
  phase_change: 'optional',
  score: 'optional',
  backup: 'optional',
  welcome: 'required',
  hello_refused: 'optional',
  start_server: 'required',
  stop_server: 'optional',
  get_server_status: 'optional',
  update_cs2: 'required',
  check_cs2_build: 'optional',
  // Required only without slots in agent.yml, as its JSDoc says.
  set_config: 'optional',
  update_agent: 'optional',
  update_cs2_image: 'optional',
  demo: 'optional',
  demo_removed: 'optional',
  demo_progress: 'optional',
  delete_demo: 'optional',
  reserve_slot: 'optional',
  watch_resources: 'optional',
  watch_logs: 'optional',
  console_command: 'optional',
  restore_round: 'optional',
  start_match: 'optional',
};

/** Gameplay event pages, by event. */
const GROUPS: Record<string, string> = {
  map_start: 'match',
  going_live: 'match',
  round_backup: 'match',
  match_restored: 'match',
  map_end: 'match',
  log: 'match',
  player_connect: 'players',
  player_disconnect: 'players',
  knife_start: 'knife',
  knife_end: 'knife',
  side_choice: 'knife',
  round_start: 'rounds',
  round_freeze_end: 'rounds',
  round_buy_end: 'rounds',
  round_end: 'rounds',
  round_complete: 'rounds',
  round_mvp: 'rounds',
  round_shots: 'rounds',
  player_death: 'combat',
  player_hurt: 'combat',
  player_blind: 'combat',
  grenade_throw: 'combat',
  grenade_detonate: 'combat',
  grenade_end: 'combat',
  bomb_begin_plant: 'bomb',
  bomb_abort_plant: 'bomb',
  bomb_planted: 'bomb',
  bomb_begin_defuse: 'bomb',
  bomb_abort_defuse: 'bomb',
  bomb_defused: 'bomb',
  bomb_exploded: 'bomb',
};

/** Every group but the two support tags is a reference page; `x-title` names it. */
const TAGS = [
  { name: 'required', description: 'A controller for Ankor agents must implement this frame.' },
  { name: 'optional', description: 'A controller for Ankor agents may ignore this frame or never send it.' },
  { name: 'socket', 'x-title': 'Agent socket', description: 'Every message on the agent\'s WebSocket, both ways.' },
  { name: 'uploads', 'x-title': 'Event upload', description: 'Gameplay events uploaded over HTTP.' },
  { name: 'match', 'x-title': 'Match', description: 'The map starting, going live and ending, round backups, and plugin log lines.' },
  { name: 'players', 'x-title': 'Players', description: 'Players joining, leaving and changing sides.' },
  { name: 'knife', 'x-title': 'Knife round', description: 'The knife round and side choice.' },
  { name: 'rounds', 'x-title': 'Rounds', description: 'Round boundaries, economy, shots and results.' },
  { name: 'combat', 'x-title': 'Combat', description: 'Kills, damage, flashes and grenades.' },
  { name: 'bomb', 'x-title': 'Bomb', description: 'Plants, defuses and explosions.' },
  {
    name: 'plugin',
    'x-title': 'Plugin socket',
    description: 'Every message between a game server\'s plugin and its agent, both ways.',
  },
];

/** The contract's success reply to the request, by the `request` it names. */
function successFor(request: string) {
  const index = contract.AgentReplySuccess.options.findIndex((option) => option.shape.request.value === request);

  if (index === -1) {
    throw new Error(`AgentReplySuccess has no branch for ${request}`);
  }

  return { index, success: contract.AgentReplySuccess.options[index]! };
}

/** A request's own reply: its success branch, or a failure. */
function replyTo(request: string) {
  return z.discriminatedUnion('ok', [successFor(request).success, contract.AgentReplyFailure]);
}

/**
 * The upload as a controller reads it. The contract's `EventBatch` is what the agent checks,
 * which leaves each event opaque; here every frame's `event` is typed.
 */
const EventBatchFrameView = z.object({
  seq: contract.EventSeq,
  event: contract.PluginToControllerMessage,
});

/** The plugin socket's `event` frame, which the agent uploads without its `type`, typed the same way. */
const PluginEventFrameView = EventBatchFrameView.extend({ type: z.literal('event') });

const EventBatchView = z.object({
  allocation_id: contract.AllocationId,
  frames: z.array(EventBatchFrameView).min(1).max(contract.EVENT_BATCH_MAX_FRAMES),
});

function pascal(name: string): string {
  return name.split('_').map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join('');
}

/** A message's `type` literal; a union of messages, such as the two replies, shares one. */
function typeOf(schema: z.core.$ZodType): string {
  if (schema instanceof z.ZodDiscriminatedUnion && schema.options[0] !== undefined) {
    return typeOf(schema.options[0]);
  }

  const type = schema instanceof z.ZodObject ? schema.shape['type'] : undefined;

  if (!(type instanceof z.ZodLiteral) || typeof type.value !== 'string') {
    throw new Error('a message has a string literal `type`');
  }

  return type.value;
}

/** Every exported schema under its export name, plus the unexported ones messages reach. */
function namedSchemas(): Map<string, z.ZodType> {
  const named = new Map<string, z.ZodType>();

  for (const [name, value] of Object.entries({ ...contract, ...container })) {
    if (value instanceof z.ZodType) {
      named.set(name, value);
    }
  }

  named.set('PlayerRef', contract.PlayerConnect.shape.player);
  named.set('PlayerAt', contract.PlayerHurt.shape.attacker.unwrap());
  named.set('PlacedPlayer', contract.PlayerHurt.shape.victim);
  named.set('RoundPlayer', contract.RoundStart.shape.players.element);
  named.set('RequestId', contract.ControllerStartServer.shape.id);
  named.set('SlotLayout', contract.NodeSettings.shape.slots);
  named.set('DemoFileName', contract.ControllerDeleteDemo.shape.file_name);
  named.set('EventBatchFrame', EventBatchFrameView);
  named.set('EventBatch', EventBatchView);
  named.set('PluginEventFrameView', PluginEventFrameView);

  return named;
}

interface MessageSpec {
  name: string;
  /** The message's own name where `name` has to be unique across channels. */
  title?: string;
  schema: string;
  tags: string[];
  correlation?: 'request' | 'reply';
  examples: unknown[];
  bindings?: Record<string, unknown>;
}

/** The first sentence, on one line however the docblock wrapped it. */
function summary(description: string | undefined): string | undefined {
  const match = description?.match(/^.*?[.!?](?=\s|$)/s);

  return (match?.[0] ?? description)?.replace(/\s*\n\s*/g, ' ');
}

/** The paths at which `parsed` differs from `example`, such as a key the schema stripped. */
function differences(example: unknown, parsed: unknown, at = '$'): string[] {
  if (example === null || parsed === null || typeof example !== 'object' || typeof parsed !== 'object') {
    return isDeepStrictEqual(example, parsed) ? [] : [at];
  }

  const keys = new Set([...Object.keys(example), ...Object.keys(parsed)]);

  return [...keys].flatMap((key) => differences((example as Record<string, unknown>)[key], (parsed as Record<string, unknown>)[key], `${at}.${key}`));
}

/** The keys whose docblock carries `@internal`, which examples leave out like the reference does. */
function internalKeys(declarations: ReadonlyMap<string, DocNode>): Set<string> {
  const keys = new Set<string>();
  const seen = new Set<DocNode>();
  const visit = (doc: DocNode | undefined): void => {
    if (doc === undefined || seen.has(doc)) {
      return;
    }

    seen.add(doc);

    for (const [key, property] of Object.entries(doc.properties ?? {})) {
      if (property.internal === true) {
        keys.add(key);
      }

      visit(property);
    }

    [doc.items, ...doc.options ?? [], ...doc.allOf ?? []].forEach(visit);
  };

  declarations.forEach(visit);

  return keys;
}

function withoutKeys(value: unknown, keys: ReadonlySet<string>): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => withoutKeys(item, keys));
  }

  if (value === null || typeof value !== 'object') {
    return value;
  }

  return Object.fromEntries(Object.entries(value).filter(([key]) => !keys.has(key)).map(([key, item]) => [key, withoutKeys(item, keys)]));
}

function examplesFor(name: string, schema: z.ZodType, internal: ReadonlySet<string>): unknown[] {
  const examples = EXAMPLES[name];

  if (examples === undefined || examples.length === 0) {
    throw new Error(`reference/examples.json has no example for ${name}`);
  }

  for (const example of examples) {
    const parsed = schema.safeParse(example);

    if (!parsed.success) {
      throw new Error(`the ${name} example does not match its schema:\n${z.prettifyError(parsed.error)}`);
    }

    // zod strips keys a schema does not know, so a stale field would pass the parse alone. An internal
    // key's default is left out of the comparison, as the example leaves the key out.
    const data = withoutKeys(parsed.data, internal);

    if (!isDeepStrictEqual(data, example)) {
      throw new Error(`the ${name} example has fields its schema drops or rewrites: ${differences(example, data).join(', ')}`);
    }
  }

  return examples;
}

function supportTag(name: string): Support {
  const support = SUPPORT[name];

  if (support === undefined) {
    throw new Error(`reference/asyncapi.ts: add ${name} to SUPPORT`);
  }

  return support;
}

function groupTag(name: string): string {
  const group = GROUPS[name];

  if (group === undefined) {
    throw new Error(`reference/asyncapi.ts: add ${name} to GROUPS`);
  }

  return group;
}

const ref = (target: string) => ({ $ref: target });

const DURATION_UNITS: [number, string][] = [[86_400_000, 'day'], [3_600_000, 'hour'], [60_000, 'minute']];

/** A contract constant as a reader wants it: durations in the largest unit that keeps a count above one, sizes in KiB or MiB, jitter in percent. */
function constantText(name: string, value: number): string {
  if (name.endsWith('_SECONDS')) {
    return constantText(`${name.slice(0, -'_SECONDS'.length)}_MS`, value * 1000);
  }

  if (name.endsWith('_JITTER')) {
    return `${value * 100}%`;
  }

  if (name.endsWith('_MS') && value % 1000 === 0) {
    const [unit, unitName] = DURATION_UNITS.find(([unit]) => value % unit === 0 && value > unit) ?? [1000, 'second'];

    return `${value / unit} ${unitName}${value === unit ? '' : 's'}`;
  }

  if (name.endsWith('_BYTES') && value % 1024 === 0) {
    return value % (1024 * 1024) === 0 ? `${value / (1024 * 1024)} MiB` : `${value / 1024} KiB`;
  }

  return String(value);
}

/**
 * Where a link to a declaration the document does not show goes: the schema built from it, or a
 * guide's path for one that has no schema.
 */
const LINK_ALIASES: Record<string, string> = {
  AGENT_ERRORS: 'AgentErrorCode',
  NODE_UNAVAILABLE: 'NodeUnavailableCode',
  // ServerExitReason spreads its values.
  AgentStopReason: 'ServerExitReason',
  // Close codes and reasons travel in close frames, not JSON; the socket page lists them.
  CLOSE_CODES: '/reference/socket/#closing',
  CloseReason: '/reference/socket/#closing',
  RawAgentConfigSchema: '/configuration/#agentyml',
  // The reference shows the frame's view, whose `event` is the gameplay event union.
  PluginEventFrame: 'PluginEventFrameView',
};

/**
 * `{@link X}`, or `{@link X | label}`, as the value of a numeric or string contract constant, or of a
 * list of strings, else as a link to the schema `X` in the document. generate.ts fails the build on
 * a schema the document lacks.
 */
function constantLinks(text: string): string {
  return text.replace(/\{@link\s+([\w.]+)(?:\s*\|\s*|\s+)?([^}]*)\}/g, (_, name: string, label: string) => {
    const value = name.split('.').reduce<unknown>((target, key) => (target as Record<string, unknown> | undefined)?.[key], { ...contract, ...container });
    const text = label.trim();

    if (typeof value === 'number' && text === '') {
      return constantText(name, value);
    }

    if (typeof value === 'string' && text === '') {
      return `\`${value}\``;
    }

    if (Array.isArray(value) && value.length > 1 && value.every((item) => typeof item === 'string') && text === '') {
      const items = value.map((item) => `\`${item}\``);

      return `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;
    }

    const target = LINK_ALIASES[name] ?? name;

    return `[\`${text === '' ? name : text}\`](${target.startsWith('/') ? target : `${SCHEMA_LINK}${target}`})`;
  });
}

/** Fails the build on refinements whose descriptions do not state them, as refinements.ts says. */
function requireRuleSentences(paths: string[]): void {
  if (paths.length > 0) {
    throw new Error(`refinements whose description has no "Rejected when …" sentence for their rule:\n${paths.join('\n')}`);
  }
}

/** The docblocks of every contract source file. */
export function contractDocs(): SourceDocs {
  const files = readdirSync(SOURCE_DIR).filter((file) => file.endsWith('.ts')).map((file) => path.join(SOURCE_DIR, file));

  return readDocblocks(files, constantLinks);
}

/** `agent.yml` as one JSON Schema with its docblocks, every key inline, for the configuration page. */
export function agentYmlSchema(docs: SourceDocs): JsonSchema {
  const doc = docs.declarations.get('AgentConfigSchema');
  const schema = schemaComponents(new Map([['AgentYml', contract.AgentConfigSchema]]), new Map(doc === undefined ? [] : [['AgentYml', doc]]))['AgentYml'];

  if (schema === undefined) {
    throw new Error('agent.yml has no JSON Schema');
  }

  requireRuleSentences(undescribedRefinements(contract.AgentConfigSchema, schema, 'AgentYml'));

  return schema;
}

export function nodeProtocolDocument(docs: SourceDocs) {
  const named = namedSchemas();
  const names = new Map<z.core.$ZodType, string>([...named].map(([name, schema]) => [schema, name]));
  const declarations = new Map<string, DocNode>(docs.declarations);
  const internal = internalKeys(docs.declarations);

  // The views read as the contract schemas they stand for.
  const batch = declarations.get('EventBatch');

  const batchFrame = declarations.get('EventBatchFrame') ?? {};

  declarations.set('PluginEventFrameView', {
    ...declarations.get('PluginEventFrame'),
    properties: { ...batchFrame.properties, ...declarations.get('PluginEventFrame')?.properties },
  });
  declarations.set('EventBatch', {
    ...batch,
    properties: { ...batch?.properties, frames: { ...batch?.properties?.['frames'], items: batchFrame } },
  });

  const schemaName = (schema: z.core.$ZodType, label: string): string => {
    const name = names.get(schema);

    if (name === undefined) {
      throw new Error(`the ${label} schema is not exported from the contract`);
    }

    return name;
  };

  const socket: MessageSpec[] = [];
  const socketOperations: Record<string, unknown> = {};
  const replySchemas = new Map<string, z.ZodType>();

  for (const option of contract.AgentToControllerMessage.options) {
    const name = typeOf(option);

    socket.push({
      name,
      schema: schemaName(option, name),
      tags: [supportTag(name), 'socket'],
      ...name === 'reply' ? { correlation: 'reply' as const } : {},
      examples: examplesFor(name, option, internal),
    });

    if (name !== 'reply') {
      socketOperations[name] = {
        action: 'receive',
        channel: ref('#/channels/agentSocket'),
        messages: [ref(`#/channels/agentSocket/messages/${name}`)],
        tags: [{ name: 'socket' }],
      };
    }
  }

  for (const option of contract.ControllerToAgentMessage.options) {
    const name = typeOf(option);
    const isRequest = 'id' in option.shape;

    socket.push({
      name,
      schema: schemaName(option, name),
      tags: [supportTag(name), 'socket'],
      ...isRequest ? { correlation: 'request' as const } : {},
      examples: examplesFor(name, option, internal),
    });

    let reply: unknown;

    if (isRequest) {
      const replySchema = replyTo(name);
      const replyName = `${name}_reply`;

      replySchemas.set(replyName, replySchema);
      named.set(`${pascal(name)}Result`, replySchema);
      names.set(replySchema, `${pascal(name)}Result`);
      socket.push({
        name: replyName,
        schema: `${pascal(name)}Result`,
        tags: [supportTag(name), 'socket'],
        correlation: 'reply',
        examples: examplesFor(replyName, replySchema, internal),
      });
      reply = { channel: ref('#/channels/agentSocket'), messages: [ref(`#/channels/agentSocket/messages/${replyName}`)] };
    }

    socketOperations[name] = {
      action: 'send',
      channel: ref('#/channels/agentSocket'),
      messages: [ref(`#/channels/agentSocket/messages/${name}`)],
      tags: [{ name: 'socket' }],
      ...reply === undefined ? {} : { reply },
    };
  }

  const uploads: MessageSpec[] = [
    { name: 'event_batch', schema: 'EventBatch', tags: ['uploads'], examples: examplesFor('event_batch', EventBatchView, internal), bindings: { http: {} } },
    {
      name: 'event_batch_receipt', schema: 'EventBatchReceipt', tags: ['uploads'],
      examples: examplesFor('event_batch_receipt', contract.EventBatchReceipt, internal), bindings: { http: { statusCode: 200 } },
    },
    {
      name: 'event_batch_rejection', schema: 'EventBatchRejection', tags: ['uploads'],
      examples: examplesFor('event_batch_rejection', contract.EventBatchRejection, internal), bindings: { http: { statusCode: 409 } },
    },
  ];

  const gameplay: MessageSpec[] = contract.PluginToControllerMessage.options.map((option) => {
    const name = typeOf(option);

    return { name, schema: schemaName(option, name), tags: [groupTag(name)], examples: examplesFor(name, contract.PluginToControllerMessage, internal) };
  });

  const plugin: MessageSpec[] = [];
  const pluginOperations: Record<string, unknown> = {};

  for (const [options, action] of [[container.PluginToAgentMessage.options, 'receive'], [container.AgentToPluginMessage.options, 'send']] as const) {
    for (const option of options) {
      const title = typeOf(option);
      const name = `plugin_${title}`;

      // The contract leaves the event opaque; the view types it.
      const schema = option === container.PluginEventFrame ? 'PluginEventFrameView' : schemaName(option, title);

      plugin.push({ name, title, schema, tags: ['plugin'], examples: examplesFor(name, option, internal) });
      pluginOperations[name] = {
        action,
        channel: ref('#/channels/pluginSocket'),
        messages: [ref(`#/channels/pluginSocket/messages/${name}`)],
        tags: [{ name: 'plugin' }],
      };
    }
  }

  const all = [...socket, ...uploads, ...gameplay, ...plugin];
  const components = schemaComponents(named, declarations);

  // A request's own reply is built here, so it reads its descriptions off the contract's branch for it.
  for (const request of contract.RequestType.options) {
    const result = components[`${pascal(request)}Result`];
    const success = result?.oneOf?.[0];
    const { index, success: schema } = successFor(request);
    const branch = components['AgentReplySuccess']?.oneOf?.[index];

    if (result === undefined || success?.properties === undefined) {
      throw new Error(`the ${request} reply has no success branch`);
    }

    result.description = `The reply to \`${request}\`: its data on success, or why it failed.`;

    for (const [key, property] of Object.entries(success.properties)) {
      property.description ??= branch?.properties?.[key]?.description;
    }

    const data = names.get(schema.shape.data);

    // The data's own schema says what it holds.
    if (data !== undefined) {
      success.properties['data']!.description = components[data]?.description;
    }
  }

  // A value set that no message field names, but readers branch on.
  const used = reachable(components, [...all.map((message) => message.schema), 'StartRefusal']);
  const schemas: Record<string, JsonSchema> = Object.fromEntries(used.map((id) => [id, components[id]!]));

  requireRuleSentences(used.flatMap((id) => undescribedRefinements(named.get(id)!, schemas[id], id)));

  const message = (spec: MessageSpec) => {
    const description = schemas[spec.schema]?.description;
    const brief = summary(description);

    return {
      name: spec.name,
      title: spec.title ?? spec.name,
      ...brief === undefined ? {} : { summary: brief },
      ...description === undefined || description === brief ? {} : { description },
      tags: spec.tags.map((name) => ({ name })),
      ...spec.correlation === undefined ? {} : { correlationId: ref(`#/components/correlationIds/${spec.correlation}`) },
      ...spec.bindings === undefined ? {} : { bindings: spec.bindings },
      payload: ref(`#/components/schemas/${spec.schema}`),
      examples: spec.examples.map((payload) => ({ payload })),
    };
  };

  const channelMessages = (specs: MessageSpec[]) =>
    Object.fromEntries(specs.map((spec) => [spec.name, ref(`#/components/messages/${spec.name}`)]));

  return {
    asyncapi: '3.1.0',
    info: {
      title: 'Ankor node protocol',
      version: String(contract.PROTOCOL_VERSION),
      description: [
        'This reference lists every message that passes between an Ankor node and a controller, with',
        'its fields and an example. For how the connection works and where to start, read the',
        '[protocol overview](/protocol/) first.',
      ].join(' '),
      tags: TAGS,
    },
    defaultContentType: 'application/json',
    servers: {
      socket: {
        host: '{controllerHost}',
        protocol: 'wss',
        description: 'The agent dials `ws://` instead when `controller.tls` is false in its agent.yml.',
        variables: { controllerHost: { description: '`controller.host` from the node\'s agent.yml.' } },
        security: [ref('#/components/securitySchemes/nodeToken')],
      },
      http: {
        host: '{controllerHost}',
        protocol: 'https',
        description: 'The agent posts over `http://` instead when `controller.tls` is false in its agent.yml.',
        variables: { controllerHost: { description: '`controller.host` from the node\'s agent.yml.' } },
        security: [ref('#/components/securitySchemes/nodeToken')],
      },
      plugin: {
        host: `127.0.0.1:${contract.EVENT_RELAY_PORT}`,
        protocol: 'ws',
        description: 'The agent\'s port for plugins, on the node\'s loopback.',
      },
    },
    channels: {
      agentSocket: {
        address: contract.DEFAULT_SOCKET_PATH,
        title: 'Agent socket',
        description: 'The default path. `controller.socket_path` in agent.yml sets another. One socket per node. Requests carry an `id`, and the agent answers each with one reply whose `reply_to` repeats it and whose `request` names the request. '
          + `The controller reads messages up to ${contract.AGENT_FRAME_MAX_BYTES} bytes and closes with \`${contract.RATE_LIMITED_CLOSE_CODE}\` \`rate_limited\` past a node's budgets; see [Limits](/protocol/limits/).`,
        servers: [ref('#/servers/socket')],
        'x-directions': [
          { from: 'agent', to: 'controller', about: declarations.get('AgentToControllerMessage')?.description },
          { from: 'controller', to: 'agent', about: declarations.get('ControllerToAgentMessage')?.description },
        ],
        'x-closes': { description: declarations.get('SOCKET_CLOSES')?.description, closes: contract.SOCKET_CLOSES },
        messages: channelMessages(socket),
        bindings: {
          ws: {
            headers: {
              type: 'object',
              properties: {
                [contract.PROTOCOL_HEADER]: { type: 'string', const: String(contract.PROTOCOL_VERSION) },
              },
              required: [contract.PROTOCOL_HEADER],
            },
          },
        },
      },
      eventUpload: {
        address: contract.DEFAULT_EVENTS_PATH,
        title: 'Event upload',
        description: `The default path. \`controller.events_path\` in agent.yml sets another. \`POST\` with \`${contract.PROTOCOL_HEADER}: ${contract.PROTOCOL_VERSION}\`. Any 2xx carries a receipt. On any other status, or no answer within 15 seconds, the agent backs off and retries.`,
        servers: [ref('#/servers/http')],
        messages: channelMessages(uploads),
      },
      gameplayEvents: {
        address: null,
        title: 'Gameplay events',
        description: 'The `event` of each frame in an event batch.',
        messages: channelMessages(gameplay),
      },
      pluginSocket: {
        address: container.PLUGIN_WS_PATH,
        title: 'Plugin socket',
        description: 'One socket per game server, on the node\'s loopback.',
        servers: [ref('#/servers/plugin')],
        bindings: {
          ws: {
            query: {
              type: 'object',
              properties: {
                slot_id: { type: 'string', description: 'The slot the game runs in.' },
                key: { type: 'string', description: 'The launch\'s relay key: 8 random bytes in hex, which the agent makes for each launch.' },
              },
              required: ['slot_id', 'key'],
            },
            headers: {
              type: 'object',
              properties: { [contract.PROTOCOL_HEADER]: { type: 'string', const: String(contract.PROTOCOL_VERSION) } },
              required: [contract.PROTOCOL_HEADER],
            },
          },
        },
        'x-directions': [
          { from: 'plugin', to: 'agent', about: declarations.get('PluginToAgentMessage')?.description },
          { from: 'agent', to: 'plugin', about: declarations.get('AgentToPluginMessage')?.description },
        ],
        messages: channelMessages(plugin),
      },
    },
    operations: {
      ...socketOperations,
      event_batch: {
        action: 'receive',
        channel: ref('#/channels/eventUpload'),
        messages: [ref('#/channels/eventUpload/messages/event_batch')],
        tags: [{ name: 'uploads' }],
        bindings: { http: { method: 'POST' } },
        reply: {
          channel: ref('#/channels/eventUpload'),
          messages: [ref('#/channels/eventUpload/messages/event_batch_receipt'), ref('#/channels/eventUpload/messages/event_batch_rejection')],
        },
      },
      gameplay_events: {
        action: 'receive',
        channel: ref('#/channels/gameplayEvents'),
        messages: gameplay.map((spec) => ref(`#/channels/gameplayEvents/messages/${spec.name}`)),
        tags: [{ name: 'uploads' }],
      },
      ...pluginOperations,
    },
    components: {
      messages: Object.fromEntries(all.map((spec) => [spec.name, message(spec)])),
      schemas,
      correlationIds: {
        request: { description: 'The request\'s id, which its reply repeats.', location: '$message.payload#/id' },
        reply: { description: 'The id of the request this answers.', location: '$message.payload#/reply_to' },
      },
      securitySchemes: {
        nodeToken: { type: 'http', scheme: 'bearer', description: '`token` from the node\'s agent.yml.' },
      },
    },
  };
}
