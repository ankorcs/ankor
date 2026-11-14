import type { APIRoute } from 'astro';
import { flatLinks } from '../nav.ts';
import { reference } from '../reference/data.ts';
import { REFERENCE_BASE } from '../reference/model.ts';
import { SITE } from '../site.ts';

export const GET: APIRoute = () => {
  const paths = new Set([...flatLinks().map((link) => link.href), ...reference.pages.map((page) => `${REFERENCE_BASE}/${page.slug}/`)]);
  const urls = [...paths].map((href) => `  <url><loc>${SITE}${href}</loc></url>`).join('\n');

  return new Response(`<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`,
    { headers: { 'Content-Type': 'application/xml' } });
};
