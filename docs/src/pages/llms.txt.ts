import type { APIRoute } from 'astro';
import { guidesInOrder, llmsHead, markdownPath } from '../llms.ts';
import { isGroup, NAV, type NavItem } from '../nav.ts';
import { sitePath } from '../site.ts';

export const GET: APIRoute = async () => {
  const guides = new Map((await guidesInOrder()).map((guide) => [sitePath(guide.data.slug === 'index' ? '/' : `/${guide.data.slug}/`), guide]));
  const lines = (items: readonly NavItem[]): string[] => items.flatMap((item) => {
    if (isGroup(item)) {
      return lines(item.items);
    }

    const guide = guides.get(item.href);

    return guide === undefined ? [] : [`- [${guide.data.title}](${markdownPath(guide.data.slug)}): ${guide.data.description}`];
  });
  const sections = NAV.flatMap((group) => {
    const entries = lines(group.items);

    return entries.length === 0 ? [] : [`## ${group.label}\n\n${entries.join('\n')}`];
  });

  const text = `${llmsHead()}
Every guide below is plain Markdown; [llms-full.txt](${sitePath('/llms-full.txt')}) holds them all in one file.

${sections.join('\n\n')}
`;

  return new Response(text, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
