/** The site sits at the root of its own domain; pages and files are addressed from there. */
export const BASE = '';

/** Where the published site lives; the sitemap needs absolute addresses. */
export const SITE = 'https://docs.ankor.sh';

/** A page or file of the site by its path from the site root, e.g. `/install/`. */
export function sitePath(path: string): string {
  return `${BASE}${path}`;
}
