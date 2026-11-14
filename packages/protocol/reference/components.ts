/** Named zod schemas to JSON Schema components that reference each other, with their docblocks laid over them. */
import { z } from 'zod';
import { type DocNode, valueBullets } from './docblocks.ts';

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
  examples?: unknown[];
  'x-enumDescriptions'?: Record<string, string>;
  [keyword: string]: unknown;
}

export const COMPONENT_PREFIX = '#/components/schemas/';

export function refName(ref: string): string {
  return ref.startsWith(COMPONENT_PREFIX) ? ref.slice(COMPONENT_PREFIX.length) : ref;
}

function isNull(schema: JsonSchema): boolean {
  return schema.type === 'null';
}

function structure(doc: DocNode): DocNode {
  return { ...doc, description: undefined };
}

function annotate(schema: JsonSchema, doc: DocNode): void {
  if (doc.description !== undefined && schema.description === undefined) {
    schema.description = doc.description;
  }

  // JSON Schema has no per-value description; the extension keeps each value's docblock beside the enum,
  // whether a value object's key carries it or a `- \`value\`: …` bullet in the enum's own docblock.
  if (schema.enum !== undefined) {
    const bullets = schema.description === undefined ? undefined : valueBullets(schema.description, schema.enum);
    const descriptions = { ...doc.enumDescriptions, ...bullets?.descriptions };

    if (bullets !== undefined && Object.keys(bullets.descriptions).length > 0) {
      schema.description = bullets.rest === '' ? undefined : bullets.rest;
    }

    if (Object.keys(descriptions).length > 0) {
      schema['x-enumDescriptions'] = descriptions;
    }
  }

  if (schema.$ref !== undefined) {
    return;
  }

  for (const [name, property] of Object.entries(schema.properties ?? {})) {
    const propertyDoc = doc.properties?.[name];

    // An `@internal` key stays in the contract for development, out of the reference.
    if (propertyDoc?.internal === true) {
      delete schema.properties![name];

      if (schema.required !== undefined) {
        schema.required = schema.required.filter((key) => key !== name);
      }

      continue;
    }

    if (propertyDoc !== undefined) {
      annotate(property, propertyDoc);
    }
  }

  if (schema.items !== undefined && doc.items !== undefined) {
    annotate(schema.items, doc.items);
  }

  for (const branches of [schema.anyOf, schema.oneOf]) {
    const present = (branches ?? []).filter((branch) => !isNull(branch));

    present.forEach((branch, index) => {
      annotate(branch, doc.options?.length === present.length ? doc.options[index] ?? {} : structure(doc));
    });
  }

  const parts = schema.allOf ?? [];

  parts.forEach((part, index) => {
    annotate(part, doc.allOf?.length === parts.length ? doc.allOf[index] ?? {} : structure(doc));
  });
}

/**
 * One JSON Schema per named zod schema. A schema used inside another becomes a `$ref` to its
 * component; unnamed ones are inlined. Input shapes, because a reader accepts keys it does not know.
 */
export function schemaComponents(
  named: ReadonlyMap<string, z.ZodType>,
  docs: ReadonlyMap<string, DocNode>,
): Record<string, JsonSchema> {
  const registry = z.registry<{ id: string }>();
  const names = new Map<z.ZodType, string>();

  for (const [id, schema] of named) {
    const existing = names.get(schema);

    if (existing !== undefined) {
      throw new Error(`${id} and ${existing} are the same schema; give it one name`);
    }

    names.set(schema, id);
    registry.add(schema, { id });
  }

  const { schemas } = z.toJSONSchema(registry, {
    target: 'draft-7',
    io: 'input',
    unrepresentable: 'any',
    uri: (id) => `${COMPONENT_PREFIX}${id}`,
  });

  const components: Record<string, JsonSchema> = {};

  for (const [id, generated] of Object.entries(schemas)) {
    const schema = { ...generated } as JsonSchema;
    const doc = docs.get(id);

    delete schema['$schema'];
    delete schema['$id'];

    if (doc !== undefined) {
      annotate(schema, doc);
    }

    components[id] = schema;
  }

  return components;
}

/** The components `roots` reach through `$ref`, roots included, in the order first reached. */
export function reachable(components: Record<string, JsonSchema>, roots: readonly string[]): string[] {
  const seen = new Set<string>();

  const visit = (value: unknown): void => {
    if (Array.isArray(value)) {
      value.forEach(visit);

      return;
    }

    if (typeof value !== 'object' || value === null) {
      return;
    }

    for (const [key, entry] of Object.entries(value)) {
      if (key === '$ref' && typeof entry === 'string') {
        enter(refName(entry));
      } else {
        visit(entry);
      }
    }
  };

  const enter = (id: string): void => {
    if (seen.has(id)) {
      return;
    }

    const component = components[id];

    if (component === undefined) {
      throw new Error(`no component named ${id}`);
    }

    seen.add(id);
    visit(component);
  };

  roots.forEach(enter);

  return [...seen];
}
