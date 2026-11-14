import { existsSync, readFileSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineHastPlugin, defineMdastPlugin, type HastPluginEntry, type MdastPluginEntry } from 'satteri';
import { BASE, sitePath } from '../site.ts';

export const REPOSITORY = 'https://github.com/ankorcs/ankor';

/** A relative link: no scheme, no protocol-relative host, no bare fragment. */
const RELATIVE_LINK = /^(?![a-z][a-z0-9+.-]*:|\/|#)([^#]*)(#.*)?$/i;

/** A link from the site root, such as a generated reference page. */
const SITE_LINK = /^\/(?!\/)/;

/** The same in raw HTML, such as the protocol page's message cards. */
const SITE_HREF = /\bhref="(?=\/(?!\/))/g;

/**
 * A file that `@ankorcs/protocol` exports, such as `agent-yml.schema.json`, parsed. Read again when
 * it changes, so a dev server that outlives a protocol build does not hold on to the old one.
 */
export function protocolFile<T>(name: string, parse: (text: string) => T): () => T {
  const file = createRequire(import.meta.url).resolve(`@ankorcs/protocol/${name}`);
  let cached: { modified: number; value: T } | undefined;

  return () => {
    const modified = statSync(file).mtimeMs;

    if (cached?.modified !== modified) {
      cached = { modified, value: parse(readFileSync(file, 'utf8')) };
    }

    return cached.value;
  };
}

/** The page a guide file becomes: its path under docs/ without `.md`, and `index` as its folder. */
export function guideSlug(relativePath: string): string {
  return relativePath.replace(/\.md$/, '').replace(/(^|\/)index$/, '').toLowerCase();
}

/**
 * Guides link to each other and to source by relative file path so they also read on GitHub.
 * The site turns guide links into page links and other files into repository links, and fails
 * the build on a link to a file that does not exist.
 */
export const guideLinks: MdastPluginEntry = ({ fileURL }) => {
  if (fileURL === undefined) {
    return null;
  }

  const file = fileURLToPath(fileURL);
  const docsDir = fileURLToPath(new URL('../..', import.meta.url));
  const repoRoot = path.resolve(docsDir, '..');

  return defineMdastPlugin({
    name: 'ankor-guide-links',
    html(node, ctx) {
      const value = node.value.replace(SITE_HREF, `href="${BASE}`);

      if (value !== node.value) {
        ctx.setProperty(node, 'value', value);
      }
    },
    link(node, ctx) {
      if (SITE_LINK.test(node.url)) {
        ctx.setProperty(node, 'url', sitePath(node.url));

        return;
      }

      const link = RELATIVE_LINK.exec(node.url);

      if (link === null) {
        return;
      }

      const [, relative = '', hash = ''] = link;
      const target = path.resolve(path.dirname(file), relative);

      if (!existsSync(target)) {
        throw new Error(`${file}: link to missing file ${node.url}`);
      }

      const inDocs = path.relative(docsDir, target);


      if (target.endsWith('.md') && !inDocs.startsWith('..')) {
        const slug = guideSlug(inDocs);

        ctx.setProperty(node, 'url', sitePath(`/${slug === '' ? '' : `${slug}/`}${hash}`));

        return;
      }

      const kind = statSync(target).isDirectory() ? 'tree' : 'blob';

      ctx.setProperty(node, 'url', `${REPOSITORY}/${kind}/master/${path.relative(repoRoot, target)}${hash}`);
    },
  });
};

/** Every link off the site, including the repository links above, opens in a new tab. */
export const externalLinks: HastPluginEntry = defineHastPlugin({
  name: 'ankor-external-links',
  element: {
    filter: ['a'],
    visit(node, ctx) {
      if (typeof node.properties.href === 'string' && /^https?:\/\//.test(node.properties.href)) {
        ctx.setProperty(node, 'target', '_blank');
        ctx.setProperty(node, 'rel', 'noopener');
      }
    },
  },
});
