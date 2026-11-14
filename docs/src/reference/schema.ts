/** How a JSON Schema reads in the reference: type labels, constraints and what a field opens onto. */
import { reference } from './data.ts';
import { refName, type JsonSchema } from './model.ts';
import { escapeHtml } from '../html.ts';
import { SITE, sitePath } from '../site.ts';

/** Zod bounds every number to the safe integer range; saying so on every field says nothing. */
const SAFE_BOUND = Number.MAX_SAFE_INTEGER;

export { escapeHtml };

export function inline(text: string): string {
  return strong(escapeHtml(text))
    // A code span naming a schema the reference shows links there, unless it is a link's text already.
    .replace(/`([^`]+)`/g, (span: string, code: string, at: number, whole: string) => {
      const href = reference.hrefs.get(code);

      return href === undefined || whole.startsWith('](', at + span.length) ? `<code>${code}</code>` : `<a href="${href}"><code>${code}</code></a>`;
    })
    .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, text: string, href: string) => {
      const target = contractHref(href);

      return /^https?:\/\//.test(target) ? `<a href="${target}" target="_blank" rel="noopener">${text}</a>` : `<a href="${target}">${text}</a>`;
    });
}

/** A schema the document links to, as `#/components/schemas/Name`, alone or after the document's file name. */
const SCHEMA_LINK = /^(?:asyncapi\.yaml)?#\/components\/schemas\/(\w+)$/;

/**
 * Where a link in the contract's descriptions goes on the site. The package writes them to be read
 * on their own: a schema as a reference into the document, which becomes its reference entry, and a
 * guide at the site's address, which becomes a path on this site. A schema the reference does not
 * show fails the build.
 */
export function contractHref(href: string): string {
  const schema = SCHEMA_LINK.exec(href);

  if (schema !== null) {
    const target = reference.hrefs.get(schema[1]!);

    if (target === undefined) {
      throw new Error(`the contract links to ${schema[1]}, which the protocol reference does not show`);
    }

    return target;
  }

  if (href.startsWith(`${SITE}/`)) {
    return sitePath(href.slice(SITE.length));
  }

  return href.startsWith('/') ? sitePath(href) : href;
}

/** `**bold**` outside code spans, which keep their text as written. */
function strong(html: string): string {
  return html.split(/(`[^`]*`)/).map((part, index) =>
    index % 2 === 1 ? part : part.replace(/\*\*(?=\S)(.+?)(?<=\S)\*\*/g, '<strong>$1</strong>')).join('');
}

const BULLET = /^[-*] /;
const NUMBERED = /^\d+\. /;

const BREAK = '\u0000';

/**
 * Wrapped lines run on as in markdown; a line that ends in a backslash breaks there. The lines are
 * joined before the inline markup, which a link or code span wrapped over two lines needs.
 */
function lines(text: string): string {
  const joined = text.split('\n').map((line) => line.trim())
    .map((line, index, all) => line.endsWith('\\') && index < all.length - 1 ? `${line.slice(0, -1).trimEnd()}${BREAK}` : `${line} `)
    .join('').trimEnd();

  return inline(joined).replaceAll(BREAK, '<br>');
}

/** Each item starts at its marker; any line that does not start one continues the item before it. */
function list(block: string[], marker: RegExp): string {
  const items: string[][] = [];

  for (const line of block) {
    if (marker.test(line) || items.length === 0) {
      items.push([line.replace(marker, '')]);
    } else {
      items.at(-1)!.push(line);
    }
  }

  const tag = marker === NUMBERED ? 'ol' : 'ul';

  return `<${tag}>${items.map((item) => `<li>${lines(item.join('\n'))}</li>`).join('')}</${tag}>`;
}

/**
 * The markdown docblocks use: paragraphs whose wrapped lines run on, `- ` and `1. ` lists whose
 * items may wrap, a backslash at a line's end for a line break, `**bold**`, code spans and links.
 */
export function markdown(text: string | undefined): string {
  if (text === undefined) {
    return '';
  }

  return text.split(/\n[ \t]*\n/).map((paragraph) => {
    const block = paragraph.split('\n');

    if (BULLET.test(block[0]!)) {
      return list(block, BULLET);
    }

    if (NUMBERED.test(block[0]!)) {
      return list(block, NUMBERED);
    }

    return `<p>${lines(paragraph)}</p>`;
  }).join('');
}

function isNull(schema: JsonSchema): boolean {
  return schema.type === 'null';
}

/** `X | null` as X, remembering the null. */
export function unwrapNullable(schema: JsonSchema): { schema: JsonSchema; nullable: boolean } {
  const options = schema.anyOf ?? schema.oneOf;

  if (options === undefined || !options.some(isNull)) {
    return { schema, nullable: false };
  }

  const present = options.filter((option) => !isNull(option));
  const inner = present.length === 1 ? present[0]! : { anyOf: present };

  return { schema: { ...inner, description: schema.description ?? inner.description }, nullable: true };
}

/** The alternatives of a union that is more than `X | null`. */
export function variants(schema: JsonSchema): JsonSchema[] | undefined {
  const options = (schema.oneOf ?? schema.anyOf)?.filter((option) => !isNull(option));

  return options !== undefined && options.length > 1 ? options : undefined;
}

function bound(schema: JsonSchema, key: string): number | undefined {
  const value = schema[key];

  return typeof value === 'number' && Math.abs(value) !== SAFE_BOUND ? value : undefined;
}

function range(min: number | undefined, max: number | undefined, unit = ''): string | undefined {
  if (min !== undefined && max !== undefined) {
    return min === max ? `${min}${unit}` : `${min}–${max}${unit}`;
  }

  if (min !== undefined) {
    return `≥ ${min}${unit}`;
  }

  return max === undefined ? undefined : `≤ ${max}${unit}`;
}

/** Enums up to this long read inline in a type; longer ones are listed when the field opens. */
const INLINE_ENUM = 4;

/** Range, length and count rules, short enough for a type label. */
export function constraints(schema: JsonSchema): string[] {
  const rules: string[] = [];
  const exclusiveMin = bound(schema, 'exclusiveMinimum');
  let min = bound(schema, 'minimum');

  if (exclusiveMin !== undefined) {
    min = schema.type === 'integer' ? exclusiveMin + 1 : min;
  }

  const value = range(min, bound(schema, 'maximum'));

  if (value !== undefined) {
    rules.push(value);
  } else if (exclusiveMin !== undefined && schema.type !== 'integer') {
    rules.push(`> ${exclusiveMin}`);
  }

  const length = range(bound(schema, 'minLength'), bound(schema, 'maxLength'), ' chars');
  const count = range(bound(schema, 'minItems'), bound(schema, 'maxItems'), ' items');

  for (const rule of [length, count]) {
    if (rule !== undefined) {
      rules.push(rule);
    }
  }

  if (typeof schema.format === 'string') {
    rules.push(escapeHtml(schema.format));
  }

  return rules;
}

/** Rules too long for a type label, shown when the field opens: a pattern, or the allowed values. */
export function detailRules(schema: JsonSchema): string[] {
  const rules: string[] = [];

  // A format such as uuid already names what zod spells out again as a pattern.
  if (typeof schema.pattern === 'string' && typeof schema.format !== 'string') {
    rules.push(`Must match <code>${escapeHtml(schema.pattern)}</code>`);
  }

  return rules;
}

/** Every value of an enum, as zod declares them, with the docblock each has. */
export function enumValues(schema: JsonSchema): { value: string; description: string | undefined }[] {
  const descriptions = schema['x-enumDescriptions'] ?? {};

  return (schema.enum ?? []).map((value) => ({ value: JSON.stringify(value), description: descriptions[String(value)] }));
}

/** Whether an enum's values need their own list: too many for the type label, or documented one by one. */
export function listsValues(schema: JsonSchema): boolean {
  return schema.enum !== undefined && (schema.enum.length > INLINE_ENUM || schema['x-enumDescriptions'] !== undefined);
}

function literal(value: unknown): string {
  return `<code>${escapeHtml(JSON.stringify(value))}</code>`;
}

/** A component's name, as a link when `hrefs` has it; an empty map gives plain names for a toggle. */
function link(name: string, hrefs: ReadonlyMap<string, string>): string {
  const href = hrefs.get(name);

  return href === undefined ? `<code>${escapeHtml(name)}</code>` : `<a href="${href}"><code>${escapeHtml(name)}</code></a>`;
}

/** A schema's type as HTML: component links, literals, element types and rules. */
export function typeLabel(schema: JsonSchema, hrefs: ReadonlyMap<string, string>): string {
  const { schema: inner, nullable } = unwrapNullable(schema);
  const suffix = nullable ? ' | null' : '';

  if (inner.$ref !== undefined) {
    return link(refName(inner.$ref), hrefs) + suffix;
  }

  if (inner.const !== undefined) {
    return literal(inner.const) + suffix;
  }

  if (inner.enum !== undefined) {
    return (listsValues(inner) ? `${escapeHtml(String(inner.type ?? 'enum'))} enum` : inner.enum.map(literal).join(' | ')) + suffix;
  }

  const options = variants(inner);

  if (options !== undefined) {
    return options.map((option) => typeLabel(option, hrefs)).join(' | ') + suffix;
  }

  if (inner.type === 'array') {
    const items = inner.items === undefined ? 'any' : typeLabel(inner.items, hrefs);
    const element = / \| /.test(items) ? `(${items})` : items;
    const rules = constraints(inner);

    return `${element}[]${rules.length === 0 ? '' : `, ${rules.join(', ')}`}${suffix}`;
  }

  const base = Array.isArray(inner.type) ? inner.type.join(' | ') : inner.type ?? 'any';
  const rules = constraints(inner);

  return `${escapeHtml(base)}${rules.length === 0 ? '' : ` ${rules.join(', ')}`}${suffix}`;
}

/** What tells a union's alternatives apart: the first field whose constant differs between them. */
export function discriminator(options: JsonSchema[]): string | undefined {
  const [first] = options;

  return Object.keys(first?.properties ?? {}).find((key) =>
    options.every((option) => option.properties?.[key]?.const !== undefined)
    && new Set(options.map((option) => JSON.stringify(option.properties?.[key]?.const))).size === options.length);
}

/** Whether a field has more to show than its row: a component, an object, a union, or an array of one. */
export function opens(schema: JsonSchema): boolean {
  const { schema: inner } = unwrapNullable(schema);

  if (inner.$ref !== undefined || inner.properties !== undefined || variants(inner) !== undefined
    || detailRules(inner).length > 0 || listsValues(inner)) {
    return true;
  }

  return inner.type === 'array' && inner.items !== undefined && opens(inner.items);
}
