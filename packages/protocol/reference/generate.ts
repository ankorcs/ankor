/**
 * Builds the AsyncAPI document and the `agent.yml` JSON Schema from the contract into dist/, which
 * the package exports as `@ankorcs/protocol/asyncapi.yaml` and `@ankorcs/protocol/agent-yml.schema.json`.
 * The zod schemas stay the only definition, so the files are build output and never committed.
 * The run fails on an example that does not match its schema, a document the AsyncAPI parser
 * rejects, a link that does not resolve, or a refinement whose description does not state its rule.
 */
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { Parser } from '@asyncapi/parser';
import { stringify } from 'yaml';
import { absoluteGuides, agentYmlSchema, contractDocs, nodeProtocolDocument, SCHEMA_LINK } from './asyncapi.ts';

const OUTPUT = path.join(import.meta.dirname, '..', 'dist');

/** The document's file name, which the `agent.yml` schema's links to schemas are relative to. */
const DOCUMENT_FILE = 'asyncapi.yaml';

/** Every string in `value` but the example payloads, rewritten. */
function mapStrings<T>(value: T, rewrite: (text: string) => string): T {
  if (typeof value === 'string') {
    return rewrite(value) as T;
  }

  if (Array.isArray(value)) {
    return value.map((item: unknown) => mapStrings(item, rewrite)) as T;
  }

  if (value === null || typeof value !== 'object') {
    return value;
  }

  return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, key === 'examples' ? item : mapStrings(item, rewrite)])) as T;
}

const docs = contractDocs();

const protocol = mapStrings(nodeProtocolDocument(docs), absoluteGuides);
const text = stringify(protocol, { lineWidth: 0, aliasDuplicateObjects: false });

/**
 * A paragraph on one line. Editors show a JSON Schema description as plain text, where the
 * docblock's wrapped lines would break mid-sentence. Blank lines between paragraphs stay.
 */
function unwrap(text: string): string {
  return text.replace(/(?<!\n)\n(?!\n)/g, ' ');
}

const agentYml = `${JSON.stringify(mapStrings(agentYmlSchema(docs), (value) => unwrap(absoluteGuides(value).replaceAll(`](${SCHEMA_LINK}`, `](${DOCUMENT_FILE}${SCHEMA_LINK}`))), null, 2)}\n`;

// Every link has to stand on its own outside the site: a schema the document shows, or an absolute address.
const shown = new Set(Object.keys(protocol.components.schemas));
const problems = new Set<string>();

for (const [, target = ''] of `${text}${agentYml}`.matchAll(/\]\(([^)\s]+)\)/g)) {
  const schema = new RegExp(`^(?:${DOCUMENT_FILE.replace('.', '\\.')})?${SCHEMA_LINK}(\\w+)$`).exec(target);

  if (schema !== null ? !shown.has(schema[1]!) : !/^https:\/\//.test(target)) {
    problems.add(target);
  }
}

if (problems.size > 0) {
  throw new Error(`links that resolve to nothing in the document, or to no absolute address: ${[...problems].sort().join(', ')}`);
}

const { document, diagnostics } = await new Parser().parse(text);

if (document === undefined || diagnostics.length > 0) {
  for (const diagnostic of diagnostics) {
    console.error(`${diagnostic.path.join('.')}: ${diagnostic.message}`);
  }

  throw new Error('the generated AsyncAPI document is not valid');
}

writeFileSync(path.join(OUTPUT, DOCUMENT_FILE), text);
writeFileSync(path.join(OUTPUT, 'agent-yml.schema.json'), agentYml);
