import type { APIRoute, GetStaticPaths } from 'astro';
import { guideMarkdown, guidesInOrder, type Guide } from '../llms.ts';

export const getStaticPaths: GetStaticPaths = async () =>
  (await guidesInOrder()).map((guide) => ({ params: { slug: guide.data.slug }, props: { guide } }));

export const GET: APIRoute = ({ props }) =>
  new Response(guideMarkdown((props as { guide: Guide }).guide), { headers: { 'Content-Type': 'text/markdown; charset=utf-8' } });
