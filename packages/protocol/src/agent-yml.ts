/**
 * The `agent.yml` that a controller renders for a node. Its layout is part of the contract,
 * because `node-setup.sh` reads the images back out with `sed`, so those top-level scalars stay
 * unindented and unquoted.
 */
import { Document } from 'yaml';
import type { z } from 'zod';
import { AgentConfigSchema, type AgentConfig } from './agent-config.js';
import { DEFAULT_EVENTS_PATH, DEFAULT_SOCKET_PATH } from './paths.js';

/**
 * The keys the controller owns. An absent key is left alone, and `null` removes it from the file.
 */
export type AgentYmlFields = Partial<Pick<AgentConfig, 'token' | 'controller' | 'cs2_image' | 'agent_image' | 'pinned_ip' | 'auto_update' | 'demo_upload' | 'slots'>> & {
  /** Null removes the cap. */
  memory_mb?: number | null;
};

/** The given fields under their `agent.yml` keys, in file order. */
export function agentYmlEntries(fields: AgentYmlFields): [string, unknown][] {
  const entries: [string, unknown][] = [];

  if (fields.token !== undefined) {
    entries.push(['token', fields.token]);
  }

  if (fields.controller !== undefined) {
    const controller = fields.controller;

    entries.push(['controller', {
      host: controller.host,
      tls: controller.tls,
      ...(controller.socket_path === DEFAULT_SOCKET_PATH ? {} : { socket_path: controller.socket_path }),
      ...(controller.events_path === DEFAULT_EVENTS_PATH ? {} : { events_path: controller.events_path }),
    }]);
  }

  if (fields.cs2_image !== undefined) {
    entries.push(['cs2_image', fields.cs2_image]);
  }

  if (fields.agent_image !== undefined) {
    entries.push(['agent_image', fields.agent_image]);
  }

  if (fields.pinned_ip !== undefined) {
    entries.push(['pinned_ip', fields.pinned_ip]);
  }

  if (fields.memory_mb !== undefined) {
    entries.push(['memory_mb', fields.memory_mb]);
  }

  // The default stays out of the file.
  if (fields.auto_update !== undefined) {
    entries.push(['auto_update', fields.auto_update ? null : false]);
  }

  if (fields.demo_upload !== undefined) {
    const upload = fields.demo_upload;

    entries.push(['demo_upload', upload === null ? null : {
      url: upload.url,
      ...(upload.header === null ? {} : { header: { name: upload.header.name, value: upload.header.value } }),
      ...(upload.limit_mbit === null ? {} : { limit_mbit: upload.limit_mbit }),
    }]);
  }

  if (fields.slots !== undefined) {
    entries.push(['slots', fields.slots.map((slot) => ({
      id: slot.id, port: slot.port, tv_port: slot.tv_port, gslt: slot.gslt,
    }))]);
  }

  return entries;
}

/**
 * The keys the controller owns, as `agent.yml` takes them. A key that the file may leave out may
 * be left out here too.
 */
export type AgentYmlInput = Omit<z.input<typeof AgentConfigSchema>, 'official_images_only'>;

/**
 * Renders the keys the controller owns as `agent.yml`, leaving out a key at its default. It throws
 * on input that `agent.yml` would not accept.
 */
export function renderAgentYml(input: AgentYmlInput): string {
  const config = AgentConfigSchema.parse(input);
  const values = Object.fromEntries(agentYmlEntries(config).filter(([, value]) => value !== null));
  const document = new Document(values);

  document.commentBefore = [
    ' agent.yml for this Ankor node.',
    " Keep it secret: it carries this node's auth token and its slots' GSLTs.",
    '',
    ' The controller can manage some parts of this file remotely.',
    ' See https://docs.ankor.sh for documentation and setup help.'
  ].join('\n');

  return document.toString();
}
