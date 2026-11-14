/** The sidebar: the guides in reading order, then the reference as the generated document groups it. */
import { REPOSITORY } from './markdown/plugins.ts';
import { reference } from './reference/data.ts';
import { REFERENCE_BASE, TYPES_SLUG } from './reference/model.ts';
import { sitePath } from './site.ts';

export interface NavLink {
  label: string;
  href: string;
}

export interface NavGroup {
  label: string;
  items: (NavLink | NavGroup)[];
}

export type NavItem = NavLink | NavGroup;

export { REPOSITORY };

/** The project's Discord server, as the repository README invites to it. */
export const DISCORD = 'https://discord.gg/H3sGar9BMY';

export const NAV: NavGroup[] = [
  {
    label: 'Start here',
    items: [
      { label: 'Overview', href: sitePath('/') },
      { label: 'How it works', href: sitePath('/how-it-works/') },
      { label: 'Quick start', href: sitePath('/quickstart/') },
      { label: 'ankor.sh', href: sitePath('/ankor-web/') },
    ],
  },
  {
    label: 'Running nodes',
    items: [
      { label: 'Requirements', href: sitePath('/requirements/') },
      { label: 'Install a node', href: sitePath('/install/') },
      { label: 'Setup script', href: sitePath('/setup-script/') },
      { label: 'Configuration and files', href: sitePath('/configuration/') },
      { label: 'Operating a node', href: sitePath('/operations/') },
      { label: 'Ready system', href: sitePath('/ready-system/') },
      { label: 'Backups and restores', href: sitePath('/backups/') },
      { label: 'Troubleshooting', href: sitePath('/troubleshooting/') },
    ],
  },
  {
    label: 'Protocol',
    items: [
      { label: 'Overview', href: sitePath('/protocol/') },
      { label: 'Versioning', href: sitePath('/protocol/versioning/') },
      { label: 'A minimal controller', href: sitePath('/protocol/minimal-controller/') },
      { label: 'Setup endpoint', href: sitePath('/protocol/node-setup/') },
      { label: 'Event uploads', href: sitePath('/protocol/events/') },
      { label: 'Demo uploads', href: sitePath('/protocol/demos/') },
      { label: 'Limits', href: sitePath('/protocol/limits/') },
    ],
  },
  {
    label: 'Protocol reference',
    items: [
      { label: 'Contents', href: `${REFERENCE_BASE}/` },
      // A section of one page is that page, not a group holding a link of the same name.
      ...reference.sections.map((section): NavItem => {
        const links = section.pages.map((page) => ({ label: page.title, href: `${REFERENCE_BASE}/${page.slug}/` }));

        return links.length === 1 ? { label: section.label, href: links[0]!.href } : { label: section.label, items: links };
      }),
      { label: 'Types & Values', href: `${REFERENCE_BASE}/${TYPES_SLUG}/` },
    ],
  },
];

export function isGroup(item: NavItem): item is NavGroup {
  return 'items' in item;
}

/** The top-level group a page sits in, named above its title. */
export function sectionOf(href: string): string | undefined {
  return NAV.find((group) => flatLinks(group.items).some((link) => link.href === href))?.label;
}

/** Every link in reading order, for the previous and next page. */
export function flatLinks(items: readonly NavItem[] = NAV): NavLink[] {
  return items.flatMap((item) => isGroup(item) ? flatLinks(item.items) : [item]);
}
