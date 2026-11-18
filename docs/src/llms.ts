/**
 * The guides as plain Markdown for language models (llmstxt.org): the site's HTML blocks become
 * text, tags and relative links resolve the way the site resolves them, and pages link to each
 * other's Markdown copies.
 */
import { existsSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import agentYml from '@ankorcs/protocol/agent-yml.schema.json';
import { getCollection, type CollectionEntry } from 'astro:content';
import { CONFIG_TAG, configAnchor } from './markdown/configTags.ts';
import { AGENT_YML_TAG, agentYmlTable, expandLimits } from './markdown/contractTags.ts';
import { FRAME_TAG } from './markdown/frameTags.ts';
import { guideSlug, REPOSITORY } from './markdown/plugins.ts';
import { flatLinks, NAV } from './nav.ts';
import { reference } from './reference/data.ts';
import { REFERENCE_BASE } from './reference/model.ts';
import { BASE, sitePath } from './site.ts';
import { EXAMPLE_TAG, exampleCode } from './markdown/exampleTags.ts';

export type Guide = CollectionEntry<'docs'>;

const DOCS_DIR = fileURLToPath(new URL('..', import.meta.url));
const REPO_ROOT = path.resolve(DOCS_DIR, '..');

/** The head of `llms.txt` and `llms-full.txt`: the guides leave the contract to the reference. */
export function llmsHead(): string {
  return `# Ankor

> Ankor runs competitive Counter-Strike 2 matches on your own Linux hosts. An agent on each host,
> a game node, connects to a controller that decides what runs where. These docs cover running
> nodes and the protocol a controller implements.

The full contract, every message, field and endpoint, is the AsyncAPI document at
[${sitePath('/asyncapi.yaml')}](${sitePath('/asyncapi.yaml')}), rendered as the reference at
[${REFERENCE_BASE}/](${REFERENCE_BASE}/). The \`@ankorcs/protocol\` npm package carries the same zod
schemas, with the same descriptions as JSDoc, and its \`@ankorcs/protocol/session\` runs the agent
socket's requests from a Node.js controller. The guides link to reference entries rather than repeat them.
`;
}

/** The Markdown copy of a guide, by its slug. */
export function markdownPath(slug: string): string {
  return sitePath(`/${slug}.md`);
}

/** The guides in sidebar order. */
export async function guidesInOrder(): Promise<Guide[]> {
  const guides = await getCollection('docs');
  const bySlug = new Map(guides.map((guide) => [guide.data.slug, guide]));

  return flatLinks(NAV).flatMap((link) => {
    const slug = link.href === sitePath('/') ? 'index' : link.href.slice(BASE.length + 1, -1);
    const guide = bySlug.get(slug);

    return guide === undefined ? [] : [guide];
  });
}

/** One guide as Markdown, headed by its title and description. */
export function guideMarkdown(guide: Guide): string {
  const file = path.resolve(DOCS_DIR, guide.filePath ?? '');
  let text = expandLimits((guide.body ?? '').replace(AGENT_YML_TAG, agentYmlTable(agentYml)), file);

  text = text.replace(new RegExp(EXAMPLE_TAG.source, 'gm'), (_, name: string) => `\`\`\`ts\n${exampleCode(name)}\n\`\`\``);
  text = text.replace(/<figure[^>]*aria-label="([^"]*)"[^>]*>[\s\S]*?<\/figure>/g, (_, label: string) => `> Diagram: ${decode(label)}`);
  text = text.replace(/<span class="part-icon"[\s\S]*?<\/span>/g, '');
  text = text.replace(/<p class="choice-name">([\s\S]*?)<\/p>/g, (_, name: string) => `### ${inline(name)}`);
  text = text.replace(/<p class="choice-sub">([\s\S]*?)<\/p>/g, (_, sub: string) => `*${inline(sub)}*`);
  text = text.replace(/<p class="(?:choice|parts|callout)-(?:label|title)[^"]*">([\s\S]*?)<\/p>/g, (_, label: string) => `**${inline(label)}**`);
  text = text.replace(/<p class="message-card-head">([\s\S]*?)<span class="message-card-direction">([\s\S]*?)<\/span><\/p>/g,
    (_, head: string, direction: string) => `**${inline(head)}** (${inline(direction)})`);
  // Tabs show one panel at a time; in Markdown every panel follows the others under its tab's name.
  let tabs: string[] = [];

  text = text.replace(/<div class="tablist"[^>]*>([\s\S]*?)<\/div>|<div class="tabpanel"[^>]*data-panel="(\d+)"[^>]*>/g,
    (_, list: string | undefined, panel: string | undefined) => {
      if (list !== undefined) {
        tabs = [...list.matchAll(/<button[^>]*>([\s\S]*?)<\/button>/g)].map((match) => inline(match[1] ?? ''));

        return '';
      }

      return `**${tabs[Number(panel)] ?? ''}**`;
    });
  text = text.replace(/<\/?(?:div|span|p)\b[^>]*>/g, '');
  text = inline(text);
  text = text.replace(FRAME_TAG, (_, frame: string) => `[\`${frame.slice(frame.indexOf(':') + 1)}\`](${reference.frames.get(frame)})`);
  text = text.replace(CONFIG_TAG, (_, key: string) => `[\`${key}\`](${markdownPath('configuration')}#${configAnchor(key)})`);
  text = text.replace(/\]\(([^)\s]+)\)/g, (_, url: string) => `](${linkFor(file, url)})`);
  text = text.replace(/\n{3,}/g, '\n\n').trim();

  return `# ${guide.data.title}\n\n> ${guide.data.description}\n\n${text}\n`;
}

/** Links between guides point at their Markdown copies; other files at the repository. */
function linkFor(file: string, url: string): string {
  if (/^\/(?!\/)/.test(url)) {
    return url.startsWith(`${BASE}/`) ? url : sitePath(url);
  }

  const relative = /^(?![a-z][a-z0-9+.-]*:|#)([^#]*)(#.*)?$/i.exec(url);

  if (relative === null) {
    return url;
  }

  const [, target = '', hash = ''] = relative;
  const resolved = path.resolve(path.dirname(file), target);
  const inDocs = path.relative(DOCS_DIR, resolved);

  if (resolved.endsWith('.md') && !inDocs.startsWith('..')) {
    return `${markdownPath(guideSlug(inDocs) || 'index')}${hash}`;
  }

  const kind = existsSync(resolved) && statSync(resolved).isDirectory() ? 'tree' : 'blob';

  return `${REPOSITORY}/${kind}/master/${path.relative(REPO_ROOT, resolved)}${hash}`;
}

/** Inline HTML the guides use, as Markdown. */
function inline(html: string): string {
  return decode(html
    .replace(/<a href="([^"]*)"[^>]*>([\s\S]*?)<\/a>/g, (_, href: string, label: string) => `[${inline(label)}](${href})`)
    .replace(/<code>([\s\S]*?)<\/code>/g, '`$1`')
    .replace(/<b>([\s\S]*?)<\/b>/g, '$1')
    .replace(/<small>([\s\S]*?)<\/small>/g, ' ($1)')
    .replace(/<br\s*\/?>/g, ' '));
}

function decode(text: string): string {
  return text.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
}
