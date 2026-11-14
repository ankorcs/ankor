/**
 * JSON Schema cannot hold a zod refinement, a check written as code such as `.refine()` or
 * `.superRefine()`. A reader of the published schemas learns the rule only from a description, so
 * every refinement needs a sentence in the description of the schema it sits on that opens with
 * "Rejected when", "Rejected if" or "Rejected unless". A rule that spans fields sits on their object.
 *
 * A pattern, a length, a bound, a format or an enum is not a refinement, because JSON Schema holds it.
 */
import type { z } from 'zod';
import type { JsonSchema } from './components.ts';

/** A sentence that states a refinement's rule. */
const RULE = /(?:^|[.!?]\s+)Rejected (?:when|if|unless)\b/;

/** Wrappers whose JSON Schema is the inner schema's, with a keyword such as `default` added. */
const SAME_NODE = new Set(['optional', 'default', 'prefault', 'readonly', 'catch', 'nonoptional']);

function refinements(schema: z.core.$ZodType): number {
  return (schema._zod.def.checks ?? []).filter((check) => check._zod.def.check === 'custom').length;
}

/** Whether a JSON Schema names a component, as a `$ref` or an `allOf` of one `$ref` that leaves room for a description. */
function refers(json: JsonSchema | undefined): boolean {
  return json?.$ref !== undefined || (json?.allOf?.length === 1 && json.allOf[0]!.$ref !== undefined);
}

/** The branches of a JSON Schema union other than `null`. */
function branches(json: JsonSchema | undefined): JsonSchema[] {
  return (json?.anyOf ?? json?.oneOf ?? []).filter((branch) => branch.type !== 'null');
}

/**
 * The paths below `where` at which `schema` carries a refinement whose description in `json` does
 * not state it. A wrapper, a pipe or an array item reads the description of the field it belongs
 * to. A `$ref` below the root is a component of its own, which its own call checks.
 */
export function undescribedRefinements(schema: z.core.$ZodType, json: JsonSchema | undefined, where: string): string[] {
  const problems: string[] = [];
  const visiting = new Set<z.core.$ZodType>();

  const walk = (node: z.core.$ZodType, at: JsonSchema | undefined, inherited: string | undefined, path: string): void => {
    if (visiting.has(node) || (refers(at) && path !== where)) {
      return;
    }

    const description = at?.description ?? inherited;

    if (refinements(node) > 0 && !RULE.test(description ?? '')) {
      problems.push(path);
    }

    visiting.add(node);

    const def = node._zod.def as z.core.$ZodTypeDef & Record<string, unknown>;

    if (SAME_NODE.has(def.type)) {
      walk(def['innerType'] as z.core.$ZodType, at, description, path);
    } else if (def.type === 'nullable') {
      const [inner] = branches(at);

      walk(def['innerType'] as z.core.$ZodType, branches(at).length === 1 ? inner : at, description, path);
    } else if (def.type === 'pipe') {
      walk(def['in'] as z.core.$ZodType, at, description, path);
      walk(def['out'] as z.core.$ZodType, at, description, path);
    } else if (def.type === 'array') {
      walk(def['element'] as z.core.$ZodType, at?.items, description, `${path}[]`);
    } else if (def.type === 'object') {
      for (const [key, value] of Object.entries(def['shape'] as Record<string, z.core.$ZodType>)) {
        walk(value, at?.properties?.[key], undefined, `${path}.${key}`);
      }
    } else if (def.type === 'union') {
      const options = def['options'] as z.core.$ZodType[];
      const json = branches(at);

      options.forEach((option, index) => walk(option, json.length === options.length ? json[index] : undefined, undefined, `${path}|${index}`));
    } else if (def.type === 'intersection') {
      walk(def['left'] as z.core.$ZodType, at?.allOf?.[0], undefined, path);
      walk(def['right'] as z.core.$ZodType, at?.allOf?.[1], undefined, path);
    } else if (def.type === 'record') {
      walk(def['valueType'] as z.core.$ZodType, at?.['additionalProperties'] as JsonSchema | undefined, undefined, `${path}[*]`);
    }

    visiting.delete(node);
  };

  walk(schema, json, undefined, where);

  return problems;
}
