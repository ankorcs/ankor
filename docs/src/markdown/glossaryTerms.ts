import { defineMdastPlugin, type MdastContent, type MdastPluginEntry } from 'satteri';
import { GLOSSARY, type GlossaryEntry } from '../glossary.ts';
import { escapeHtml } from '../html.ts';

/** A spelling in capitals, such as `VM` or `GSLT`, names the term only in capitals. */
const inCapitals = (spelling: string): boolean => spelling === spelling.toUpperCase();

const key = (spelling: string): string => (inCapitals(spelling) ? spelling : spelling.toLowerCase());

/** Each spelling names one term, so a word always has the same definition. */
const ENTRIES = new Map<string, GlossaryEntry>();

for (const entry of GLOSSARY) {
  for (const spelling of entry.spellings) {
    if (ENTRIES.has(key(spelling))) {
      throw new Error(`glossary: "${spelling}" names two terms`);
    }

    ENTRIES.set(key(spelling), entry);
  }
}

// Longest first, so "GOTV port" wins over "GOTV". A term is a whole word, not part of `self-update`.
const SPELLINGS = [...GLOSSARY.flatMap((entry) => entry.spellings)].sort((a, b) => b.length - a.length);
const TERM = new RegExp(`(?<![\\w-])(${SPELLINGS.map((spelling) => spelling.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})(?![\\w-])`, 'gi');

/** Raw HTML that opens an element, and the cards, diagrams and message cards whose text is not running prose. */
const OPENS_BLOCK = /<(div|figure)\b/g;
const CLOSES_BLOCK = /<\/(div|figure)>/g;
const NOT_PROSE = /class="(?:[^"]*\s)?(?:parts?|message-cards?|diagram)[\s"-]/;

type Node = { type: string; depth?: number; value?: string; children?: Node[] };

/**
 * Marks the first use of each glossary term in a section of running text, where a section runs from
 * one `h2` to the next. The reader hovers or focuses the term for its definition, in the layout's
 * `data-tip` tooltip. Headings, links, code, table headers and raw-HTML blocks such as cards and
 * diagrams are left as they are.
 */
export const glossaryTerms: MdastPluginEntry = () => {
  const marked = new Map<number, Set<GlossaryEntry>>();

  return defineMdastPlugin({
    name: 'ankor-glossary-terms',
    text(node, ctx) {
      // Climb to the top-level block, skipping text that is not running prose on the way.
      let block: Node = node as Node;
      let parent = ctx.parent(node) as Node | undefined;

      while (parent !== undefined && parent.type !== 'root') {
        if (parent.type === 'link' || parent.type === 'linkReference' || parent.type === 'heading') {
          return;
        }

        if (parent.type === 'tableRow' && ctx.indexOf(parent as never) === 0) {
          return;
        }

        block = parent;
        parent = ctx.parent(parent as never) as Node | undefined;
      }

      const root = parent;
      const index = root === undefined ? undefined : ctx.indexOf(block as never);

      if (root?.children === undefined || index === undefined) {
        return;
      }

      // The section starts after the nearest h1 or h2 above. Inside it, the nearest raw HTML above
      // says whether the block sits in an unclosed card or diagram.
      let section = 0;
      let inBlock = false;
      let htmlSeen = false;

      for (let at = index - 1; at >= 0; at -= 1) {
        const sibling = root.children[at]!;

        if (sibling.type === 'heading' && (sibling.depth ?? 0) <= 2) {
          section = at + 1;

          break;
        }

        if (!htmlSeen && sibling.type === 'html' && sibling.value !== undefined) {
          htmlSeen = true;
          // Wrappers such as `tree-table` or `symptoms` hold prose; only cards and diagrams do not.
          inBlock = NOT_PROSE.test(sibling.value) && (sibling.value.match(OPENS_BLOCK)?.length ?? 0) > (sibling.value.match(CLOSES_BLOCK)?.length ?? 0);
        }
      }

      if (inBlock) {
        return;
      }

      const seen = marked.get(section) ?? new Set<GlossaryEntry>();

      marked.set(section, seen);

      let last = 0;
      const nodes: MdastContent[] = [];

      for (const match of node.value.matchAll(TERM)) {
        const entry = ENTRIES.get(key(match[0]));

        if (entry === undefined || seen.has(entry)) {
          continue;
        }

        const before = node.value.slice(0, match.index).trimEnd().toLowerCase();

        if (entry.notAfter?.some((word) => before.endsWith(word.toLowerCase())) === true) {
          continue;
        }

        seen.add(entry);

        if (match.index > last) {
          nodes.push({ type: 'text', value: node.value.slice(last, match.index) });
        }

        nodes.push({ type: 'html', value: `<span class="term" tabindex="0" data-tip="${escapeHtml(entry.definition)}">${escapeHtml(match[0])}</span>` });
        last = match.index + match[0].length;
      }

      if (nodes.length === 0) {
        return;
      }

      if (last < node.value.length) {
        nodes.push({ type: 'text', value: node.value.slice(last) });
      }

      ctx.replaceNode(node, nodes);
    },
  });
};
