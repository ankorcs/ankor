import type { APIRoute } from 'astro';
import { guideMarkdown, guidesInOrder, llmsHead } from '../llms.ts';

export const GET: APIRoute = async () =>
  new Response([llmsHead(), ...(await guidesInOrder()).map(guideMarkdown)].join('\n---\n\n'), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
