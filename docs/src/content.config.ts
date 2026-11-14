import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'zod';

export const collections = {
  // The guides sit at the package root so they read the same on GitHub; `slug` is each page's path.
  docs: defineCollection({
    loader: glob({ pattern: ['*.md', 'protocol/*.md'], base: '.' }),
    schema: z.object({ title: z.string(), description: z.string(), slug: z.string() }),
  }),
};
