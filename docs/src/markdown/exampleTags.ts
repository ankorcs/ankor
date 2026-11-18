import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineMdastPlugin, type MdastPluginEntry } from 'satteri';

/** `[example:controller/accept]`: the code of `docs/examples/controller/accept.ts`, as a block. */
export const EXAMPLE_TAG = /^\[example:([a-z0-9-]+(?:\/[a-z0-9-]+)*)\]$/;

/**
 * The docs package's `examples/`, found from this file upwards: this module runs from `src/` in the
 * Markdown pipeline and from a bundled chunk under `dist/` when the build renders the Markdown copies.
 */
function examplesDir(): string {
  for (let dir = path.dirname(fileURLToPath(import.meta.url)); dir !== path.dirname(dir); dir = path.dirname(dir)) {
    if (existsSync(path.join(dir, 'examples', 'controller'))) {
      return path.join(dir, 'examples');
    }
  }

  throw new Error(`no examples/ above ${fileURLToPath(import.meta.url)}`);
}

/**
 * The parts of an example file that a page shows: every `// #region example` up to its
 * `// #endregion`, each dedented and trimmed, and joined by a blank line. The rest of the file,
 * such as the function an excerpt sits in, only makes it compile, so `astro check` holds every
 * example to the protocol package. A missing file or a file without a region fails the build.
 */
export function exampleCode(name: string): string {
  const file = path.join(examplesDir(), `${name}.ts`);

  if (!existsSync(file)) {
    throw new Error(`[example:${name}]: ${file} does not exist`);
  }

  const regions: string[][] = [];
  let open: string[] | null = null;

  for (const [index, line] of readFileSync(file, 'utf8').split('\n').entries()) {
    const where = `${file}:${index + 1}`;
    const marker = /^\s*\/\/ #(region|endregion)\b\s*(.*)$/.exec(line);

    if (marker?.[1] === 'region') {
      if (marker[2] !== 'example') {
        throw new Error(`${where}: only "// #region example" marks code to show`);
      }

      if (open !== null) {
        throw new Error(`${where}: a region opens inside another`);
      }

      open = [];
    } else if (marker?.[1] === 'endregion') {
      if (open === null) {
        throw new Error(`${where}: "// #endregion" closes no region`);
      }

      if (open.every((code) => code.trim() === '')) {
        throw new Error(`${where}: the region is empty`);
      }

      regions.push(open);
      open = null;
    } else if (open !== null) {
      open.push(line);
    }
  }

  if (open !== null) {
    throw new Error(`[example:${name}]: ${file} ends inside a region`);
  }

  if (regions.length === 0) {
    throw new Error(`[example:${name}]: ${file} has no "// #region example"`);
  }

  return regions.map((lines) => {
    const indent = Math.min(...lines.filter((line) => line.trim() !== '').map((line) => /^ */.exec(line)![0].length));

    return lines.map((line) => line.slice(indent)).join('\n').replace(/^\n+|\n+$/g, '');
  }).join('\n\n');
}

/** A paragraph that is only an example tag becomes the example's code block. */
export const exampleTags: MdastPluginEntry = () => defineMdastPlugin({
  name: 'ankor-example-tags',
  paragraph(node, ctx) {
    const [text] = node.children;
    const value = node.children.length === 1 && text?.type === 'text' ? text.value : '';

    if (!value.startsWith('[example:')) {
      return;
    }

    const tag = EXAMPLE_TAG.exec(value);

    if (tag === null) {
      throw new Error(`${value} is not an example tag: use lowercase letters, digits, - and /`);
    }

    ctx.replaceNode(node, { type: 'code', lang: 'ts', value: exampleCode(tag[1]!) });
  },
});
