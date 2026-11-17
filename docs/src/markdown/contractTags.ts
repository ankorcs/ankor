import { fileURLToPath } from 'node:url';
import * as contract from '@ankorcs/protocol';
import { AGENT_BUDGETS, type BudgetName, type Rate } from '@ankorcs/protocol';
import { defineMdastPlugin, markdownToHtml, type MdastPluginEntry } from 'satteri';
import type { JsonSchema } from '../reference/model.ts';
import { contractHref, defaultOf } from '../reference/schema.ts';
import { configAnchor } from './configTags.ts';
import { protocolFile } from './plugins.ts';

/**
 * `[limit:MAX_SLOTS]` and `[limit-time:AGENT_HELLO_TIMEOUT_MS]`: a numeric `@ankorcs/protocol`
 * constant, dotted into objects, as a number, a size, a duration or a percentage. `[limit-burst:state]` and
 * `[limit-refill:state]`: one of the `AGENT_BUDGETS`, with its per-slot share.
 */
const LIMIT_TAG = /\[limit(?:-(size|time|percent|burst|refill))?:([A-Za-z0-9_.]+)\]/g;

/** A paragraph of its own: the table of every `agent.yml` key, from the generated schema. */
export const AGENT_YML_TAG = '[table:agent-yml]';

const agentYmlSchema = protocolFile('agent-yml.schema.json', (text) => JSON.parse(text) as JsonSchema);

const KIB = 1024;

const MIB = 1024 * KIB;

const DURATIONS: [number, string][] = [[86_400_000, 'day'], [3_600_000, 'hour'], [60_000, 'minute'], [1000, 'second']];

function sizeText(bytes: number): string {
  if (bytes % MIB === 0) {
    return `${bytes / MIB} MiB`;
  }

  return bytes % KIB === 0 ? `${bytes / KIB} KiB` : `${bytes} bytes`;
}

/** One unit reads "a minute", so prose such as "within [limit-time:…]" stays natural. */
function timeText(ms: number): string {
  const [unit, name] = DURATIONS.find(([unit]) => ms % unit === 0) ?? [1, 'millisecond'];
  const count = ms / unit;

  return count === 1 ? `${name === 'hour' ? 'an' : 'a'} ${name}` : `${count} ${name}s`;
}

function isBudget(name: string): name is BudgetName {
  return Object.hasOwn(AGENT_BUDGETS, name);
}

function rateText(rate: Rate, part: 'burst' | 'refill', size: boolean): string {
  const amount = (value: number) => size ? sizeText(value) : String(value);

  if (part === 'burst') {
    return amount(rate.burst);
  }

  return rate.perSecond < 1 ? `1 every ${1 / rate.perSecond} s` : `${amount(rate.perSecond)}/s`;
}

/** A tag's text; `where` names the page in the error a tag without a value throws. */
function limitText(kind: string | undefined, name: string, where: string): string {
  if (kind === 'burst' || kind === 'refill') {
    if (!isBudget(name)) {
      throw new Error(`${where}: no budget ${name} in AGENT_BUDGETS`);
    }

    const { base, perSlot } = AGENT_BUDGETS[name];
    const size = /bytes$/i.test(name);
    const text = rateText(base, kind, size);

    return perSlot.burst === 0 && perSlot.perSecond === 0 ? text : `${text} + ${rateText(perSlot, kind, size)} a slot`;
  }

  const value = name.split('.').reduce<unknown>((target, key) => (target as Record<string, unknown> | undefined)?.[key], contract);

  if (typeof value !== 'number') {
    throw new Error(`${where}: ${name} is not a numeric constant of @ankorcs/protocol`);
  }

  if (kind === 'size') {
    return sizeText(value);
  }

  if (kind === 'percent') {
    return `${value * 100}%`;
  }

  return kind === 'time' ? timeText(value) : String(value);
}

export function expandLimits(text: string, where: string): string {
  return text.replace(LIMIT_TAG, (_, kind: string | undefined, name: string) => limitText(kind, name, where));
}

/** Zod bounds every number to the safe integer range; saying so says nothing. */
function bound(schema: JsonSchema, key: string): number | undefined {
  const value = schema[key];

  return typeof value === 'number' && Math.abs(value) !== Number.MAX_SAFE_INTEGER ? value : undefined;
}

/** What the schema checks beyond its pattern, which the key's docblock words. */
function rules(schema: JsonSchema): string[] {
  const sentences: string[] = [];

  if (schema.type === 'array' && schema.items?.properties !== undefined) {
    sentences.push('A list. Each entry has the keys below.');
  }

  const exclusiveMin = bound(schema, 'exclusiveMinimum');
  const min = bound(schema, 'minimum') ?? (exclusiveMin === undefined ? undefined : exclusiveMin + 1);
  const max = bound(schema, 'maximum');

  if (schema.type === 'integer') {
    if (min !== undefined && max !== undefined) {
      sentences.push(`A whole number from ${min} to ${max}.`);
    } else if (min !== undefined) {
      sentences.push(`A whole number, at least ${min}.`);
    } else {
      sentences.push('A whole number.');
    }
  }

  const maxLength = bound(schema, 'maxLength');

  if (maxLength !== undefined) {
    sentences.push(`At most ${maxLength} characters.`);
  }

  const maxItems = bound(schema, 'maxItems');

  if (maxItems !== undefined) {
    sentences.push(`At most ${maxItems} entries.`);
  }

  const fallback = defaultOf(schema);

  if (fallback === 'fields') {
    sentences.push('Each field takes its default.');
  } else if (fallback !== undefined) {
    sentences.push(`Default \`${fallback.json}\`.`);
  }

  return sentences;
}

/** The keys of an object, or of an array's objects, which the table lists under their parent. */
function children(schema: JsonSchema): [string, JsonSchema][] {
  const object = schema.type === 'array' ? schema.items : schema;

  return Object.entries(object?.properties ?? {});
}

function rows(schema: JsonSchema, path: string, indent: string): string[] {
  const object = schema.type === 'array' ? schema.items : schema;
  const keys = children(schema);

  return keys.flatMap(([key, field], index) => {
    const last = index === keys.length - 1;
    const dotted = path === '' ? key : `${path}.${key}`;
    const branch = path === '' ? '' : `${indent}${last ? '└─ ' : '├─ '}`;
    const required = object?.required?.includes(key) === true
      ? '<span class="required">yes</span>'
      : '<span class="optional">no</span>';
    // A table cell is one line, however the docblock wrapped or broke it.
    const value = [field.description ?? '', ...rules(field)].join(' ').replace(/\s*\n\s*/g, ' ').trim().replaceAll('|', '\\|')
      .replace(/\]\(([^)\s]+)\)/g, (_, href: string) => `](${contractHref(href)})`);
    // An array reads as one, so its keys are not taken for a single object's.
    const label = field.type === 'array' ? `${key}[]` : key;
    const row = `| <span id="${configAnchor(dotted)}"></span><span class="branch">${branch}</span>${label} | ${required} | ${value} |`;
    const nested = path === '' ? '' : `${indent}${last ? '   ' : '│  '}`;

    return [row, ...rows(field, dotted, nested)];
  });
}

/** The `agent.yml` key table as the guide's Markdown, anchored the way `[config:…]` tags link. */
export function agentYmlTable(schema: JsonSchema): string {
  return [
    '<div class="tree-table">',
    '',
    '| Key | Required | Value |',
    '|-----|----------|-------|',
    ...rows(schema, '', ''),
    '',
    '</div>',
  ].join('\n');
}

/**
 * Values guides would otherwise copy from the contract: limit tags become the constant's value, and
 * the `agent.yml` table tag becomes the table. Listed before the other plugins, so its links and
 * frame tags resolve like the guide's own.
 */
export const contractTags: MdastPluginEntry = ({ fileURL }) => {
  const where = fileURL === undefined ? 'a guide' : fileURLToPath(fileURL);

  return defineMdastPlugin({
    name: 'ankor-contract-tags',
    // Diagrams written as HTML quote limits too.
    html(node, ctx) {
      const value = expandLimits(node.value, where);

      if (value !== node.value) {
        ctx.replaceNode(node, [{ type: 'html', value }]);
      }
    },
    paragraph(node, ctx) {
      const [text] = node.children;

      if (node.children.length !== 1 || text?.type !== 'text' || text.value !== AGENT_YML_TAG) {
        return;
      }

      const { html } = markdownToHtml(agentYmlTable(agentYmlSchema()), { features: { gfm: true, smartPunctuation: true } });

      ctx.replaceNode(node, { type: 'html', value: html });
    },
    text(node, ctx) {
      const value = expandLimits(node.value, where);

      if (value !== node.value) {
        ctx.setProperty(node, 'value', value);
      }
    },
  });
};
