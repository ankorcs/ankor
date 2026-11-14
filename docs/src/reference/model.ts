/**
 * The generated AsyncAPI document as reference pages: one per group of messages, and one for the
 * shared types they reach. Everything here reads the document; nothing restates the contract.
 */
import type { SocketClose } from '@ankorcs/protocol';
import { parse } from 'yaml';
import { sitePath } from '../site.ts';

export interface JsonSchema {
  $ref?: string;
  type?: string | string[];
  description?: string;
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  anyOf?: JsonSchema[];
  oneOf?: JsonSchema[];
  allOf?: JsonSchema[];
  enum?: unknown[];
  const?: unknown;
  format?: string;
  pattern?: string;
  /** Each enum value's docblock, where it has one. */
  'x-enumDescriptions'?: Record<string, string>;
  [keyword: string]: unknown;
}

interface Ref {
  $ref: string;
}

interface Tag {
  name: string;
  description?: string;
  'x-title'?: string;
}

interface Message {
  name: string;
  /** The name shown, where `name` is made unique across channels. */
  title?: string;
  summary?: string;
  description?: string;
  tags: { name: string }[];
  payload: Ref;
  examples?: { payload: unknown }[];
  bindings?: { http?: { statusCode?: number } };
}

interface Operation {
  action: 'send' | 'receive';
  channel: Ref;
  messages: Ref[];
  tags?: { name: string }[];
  reply?: { messages: Ref[] };
}

interface Document {
  info: { title: string; version: string; description?: string; tags: Tag[] };
  channels: Record<string, {
    address: string | null;
    title?: string;
    description?: string;
    /** On a socket: what each side sends, from the contract's two unions. */
    'x-directions'?: (Direction & { about?: string })[];
    /** On the agent socket: every close either side makes on purpose. */
    'x-closes'?: { description?: string; closes: SocketClose[] };
    messages: Record<string, Ref>;
  }>;
  operations: Record<string, Operation>;
  components: { messages: Record<string, Message>; schemas: Record<string, JsonSchema> };
}

export type Kind = 'report' | 'request' | 'notice' | 'reply' | 'upload' | 'answer' | 'event';

export type Party = 'agent' | 'controller' | 'plugin';

export interface Direction {
  from: Party;
  to: Party;
}

/** How a direction reads everywhere: `agent → controller`. */
export function directionText(direction: Direction): string {
  return `${direction.from} → ${direction.to}`;
}

export function sameDirection(a: Direction, b: Direction): boolean {
  return a.from === b.from && a.to === b.to;
}

export interface MessageView {
  /** Its key among the document's messages, unique across channels. */
  key: string;
  name: string;
  anchor: string;
  support: 'required' | 'optional' | null;
  /** Null for gameplay events, which all go plugin → controller inside an event batch. */
  direction: Direction | null;
  /** How an HTTP message travels, after its direction: `HTTP POST`, `HTTP 200`. */
  transport: string | null;
  kind: Kind;
  summary: string | undefined;
  description: string | undefined;
  schema: string;
  /** For requests: the message that answers them. */
  reply: MessageView | null;
  statusCode: number | null;
  examples: unknown[];
}

/** A nested section's messages on its parent's page, such as one kind of gameplay event. */
export interface ReferencePart {
  /** Its heading's `id` on the parent's page. */
  slug: string;
  title: string;
  description: string;
  messages: MessageView[];
}

export interface ReferencePage {
  slug: string;
  /** The channel its messages are on. */
  channel: string;
  title: string;
  description: string;
  messages: MessageView[];
  /** The groups of nested sections, shown after its own messages. */
  parts: ReferencePart[];
}

/** A group of reference pages, by the channel their messages are on. */
export interface ReferenceSection {
  /** The channel its pages' messages are on. */
  channel: string;
  label: string;
  /** Its heading's `id` on the contents page, and the outline's link to it. */
  slug: string;
  /** Markdown under its heading on the contents page. */
  intro: string;
  pages: ReferencePage[];
}

export interface Reference {
  document: Document;
  /** In page order. */
  sections: ReferenceSection[];
  pages: ReferencePage[];
  /** Components the generator kept that are not themselves a message's payload. */
  types: string[];
  /** Where each component is described: its message on a group page, or the types page. */
  hrefs: Map<string, string>;
  /** Message payload components, with the message each one carries. */
  payloads: Map<string, string>;
  /** Where each frame is described, by the tag guides write it as: `agent:hello`, `event:map_end`. */
  frames: Map<string, string>;
}


/** The prefix of a frame tag, by channel. */
const FRAME_PREFIXES: Record<string, string> = {
  agentSocket: 'agent',
  eventUpload: 'upload',
  gameplayEvents: 'event',
  pluginSocket: 'plugin',
};

const SUPPORT = new Set(['required', 'optional']);

export const REFERENCE_BASE = sitePath('/reference');

export const TYPES_SLUG = 'types';

/** Each section of the reference, by channel: the one place its name, slug and intro are set. */
/**
 * Channels whose groups are shown on another channel's page instead of pages of their own: each
 * kind of gameplay event joins the event upload page that carries it.
 */
const JOINS: Record<string, string> = {
  gameplayEvents: 'eventUpload',
};

const SECTIONS: Record<string, Omit<ReferenceSection, 'channel' | 'pages'>> = {
  agentSocket: {
    label: 'Agent socket',
    slug: 'agent-socket',
    intro: 'Everything sent over the WebSocket each node keeps open: what the agent reports, the requests your controller sends, and their replies.',
  },
  eventUpload: {
    label: 'Event upload',
    slug: 'event-upload',
    intro: 'The HTTP request that delivers gameplay events in batches, the answers your controller gives, and every gameplay event a batch can carry, grouped by what happens in the game.',
  },
  pluginSocket: {
    label: 'Plugin socket',
    slug: 'plugin-socket',
    intro: "The local socket inside a node between each game server's plugin and the agent.",
  },
};

export function refName(ref: string): string {
  return ref.slice(ref.lastIndexOf('/') + 1);
}

export function messageAnchor(name: string): string {
  return `message-${name}`;
}

export function schemaAnchor(name: string): string {
  return `schema-${name}`;
}

function channelOf(document: Document, message: string): string {
  for (const [id, channel] of Object.entries(document.channels)) {
    if (message in channel.messages) {
      return id;
    }
  }

  throw new Error(`message ${message} is on no channel`);
}

function describe(document: Document, name: string, replies: Map<string, string>, requests: Set<string>, sent: Set<string>, pluginSent: Set<string>): Omit<MessageView, 'reply'> {
  const message = document.components.messages[name];

  if (message === undefined) {
    throw new Error(`no message named ${name}`);
  }

  const channel = channelOf(document, name);
  const support = message.tags.find((tag) => SUPPORT.has(tag.name))?.name as MessageView['support'] | undefined;
  const statusCode = message.bindings?.http?.statusCode ?? null;
  let kind: Kind;
  let direction: Direction | null;
  let transport: string | null = null;

  if (channel === 'pluginSocket') {
    kind = pluginSent.has(name) ? 'notice' : 'report';
    direction = pluginSent.has(name) ? { from: 'agent', to: 'plugin' } : { from: 'plugin', to: 'agent' };
  } else if (channel === 'gameplayEvents') {
    kind = 'event';
    direction = null;
  } else if (channel === 'eventUpload') {
    kind = statusCode === null ? 'upload' : 'answer';
    direction = statusCode === null ? { from: 'agent', to: 'controller' } : { from: 'controller', to: 'agent' };
    transport = statusCode === null ? 'HTTP POST' : `HTTP ${statusCode}`;
  } else if (replies.has(name) || name === 'reply') {
    kind = 'reply';
    direction = { from: 'agent', to: 'controller' };
  } else if (sent.has(name)) {
    kind = requests.has(name) ? 'request' : 'notice';
    direction = { from: 'controller', to: 'agent' };
  } else {
    kind = 'report';
    direction = { from: 'agent', to: 'controller' };
  }

  return {
    key: name,
    name: message.title ?? name,
    anchor: messageAnchor(name),
    support: support ?? null,
    direction,
    transport,
    kind,
    summary: message.summary,
    description: message.description,
    schema: refName(message.payload.$ref),
    statusCode,
    examples: (message.examples ?? []).map((example) => example.payload),
  };
}

export function loadReference(yaml: string): Reference {
  const document = parse(yaml) as Document;
  const replies = new Map<string, string>();
  const requests = new Set<string>();
  const sent = new Set<string>();
  const pluginSent = new Set<string>();

  for (const operation of Object.values(document.operations)) {
    const [message] = operation.messages.map((ref) => refName(ref.$ref));

    if (message !== undefined && operation.channel.$ref === '#/channels/pluginSocket' && operation.action === 'send') {
      pluginSent.add(message);
    }

    if (message === undefined || operation.channel.$ref !== '#/channels/agentSocket') {
      continue;
    }

    if (operation.action === 'send') {
      sent.add(message);
    }

    const reply = operation.reply?.messages[0];

    if (reply !== undefined) {
      requests.add(message);
      replies.set(refName(reply.$ref), message);
    }
  }

  const views = new Map<string, MessageView>();

  for (const name of Object.keys(document.components.messages)) {
    views.set(name, { ...describe(document, name, replies, requests, sent, pluginSent), reply: null });
  }

  for (const operation of Object.values(document.operations)) {
    const [message] = operation.messages.map((ref) => refName(ref.$ref));
    const reply = operation.reply?.messages[0];
    const view = message === undefined ? undefined : views.get(message);

    if (view !== undefined && reply !== undefined && operation.channel.$ref === '#/channels/agentSocket') {
      view.reply = views.get(refName(reply.$ref)) ?? null;
    }
  }

  const pages: ReferencePage[] = [];

  for (const tag of document.info.tags) {
    if (SUPPORT.has(tag.name)) {
      continue;
    }

    const members = [...views.values()].filter((view) => {
      const message = document.components.messages[view.key];

      // A request's own reply is shown with the request.
      return message?.tags.some((entry) => entry.name === tag.name) === true && !(replies.has(view.key) && view.key !== 'reply');
    });

    if (members.length === 0) {
      continue;
    }

    pages.push({
      slug: tag.name,
      title: tag['x-title'] ?? tag.name,
      description: tag.description ?? '',
      channel: channelOf(document, members[0]!.key),
      messages: members,
      parts: [],
    });
  }

  // A nested section's groups join its parent's page as parts, so one page holds the event upload
  // and every kind of gameplay event in it.
  for (const page of [...pages]) {
    const parent = JOINS[page.channel];

    if (parent === undefined) {
      continue;
    }

    const host = pages.find((candidate) => candidate.channel === parent);

    if (host === undefined) {
      throw new Error(`reference: ${page.slug} belongs on the ${parent} page, which has no messages`);
    }

    host.parts.push({ slug: page.slug, title: page.title, description: page.description, messages: page.messages });
    pages.splice(pages.indexOf(page), 1);
  }

  const hrefs = new Map<string, string>();
  const payloadNames = new Map<string, string>();

  const frames = new Map<string, string>();

  for (const page of pages) {
    const anchors = new Set<string>();

    for (const message of [...page.messages, ...page.parts.flatMap((part) => part.messages)]) {
      // Parts share their host's page, so an anchor must stay unique across all of them.
      if (anchors.has(message.anchor)) {
        throw new Error(`reference: two messages on ${page.slug} share the anchor ${message.anchor}`);
      }

      anchors.add(message.anchor);

      const href = `${REFERENCE_BASE}/${page.slug}/#${message.anchor}`;

      hrefs.set(message.schema, href);
      payloadNames.set(message.schema, message.name);

      for (const frame of message.reply === null ? [message] : [message, message.reply]) {
        const prefix = FRAME_PREFIXES[channelOf(document, frame.key)];

        frames.set(`${prefix}:${frame.name}`, frame === message ? href : `${REFERENCE_BASE}/${page.slug}/#${frame.anchor}`);
      }
    }
  }

  // The generator keeps only the components the messages and value sets reach.
  const payloads = new Set(Object.values(document.components.messages).map((message) => refName(message.payload.$ref)));
  const types = Object.keys(document.components.schemas).filter((name) => !hrefs.has(name) && !payloads.has(name)).sort();

  for (const name of types) {
    hrefs.set(name, `${REFERENCE_BASE}/${TYPES_SLUG}/#${schemaAnchor(name)}`);
  }

  const sections: Reference['sections'] = [];
  const channels: string[] = [];

  for (const page of pages) {
    const at = channels.indexOf(page.channel);

    if (at !== -1) {
      sections[at]!.pages.push(page);

      continue;
    }

    const section = SECTIONS[page.channel];

    if (section === undefined) {
      throw new Error(`reference: no section for the ${page.channel} channel; add it to SECTIONS`);
    }

    channels.push(page.channel);
    sections.push({ ...section, channel: page.channel, pages: [page] });
  }

  return { document, sections, pages, types, hrefs, payloads: payloadNames, frames };
}

/** What a `required` or `optional` tag means, for its tooltip. */
export function supportDescription(reference: Reference, support: string): string | undefined {
  return reference.document.info.tags.find((tag) => tag.name === support)?.description;
}
