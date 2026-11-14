import { satteri } from '@astrojs/markdown-satteri';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'astro/config';
import { createCssVariablesTheme } from 'shiki/core';
import { configTags } from './src/markdown/configTags.ts';
import { contractTags } from './src/markdown/contractTags.ts';
import { frameTags } from './src/markdown/frameTags.ts';
import { glossaryTerms } from './src/markdown/glossaryTerms.ts';
import { externalLinks, guideLinks } from './src/markdown/plugins.ts';
import { BASE } from './src/site.ts';
import { watchReference } from './tools/watchReference.ts';

// Shiki's css-variables theme, regrouped to read like PhpStorm's Islands Dark: names, operators and
// punctuation are plain text, while keywords, strings, numbers, properties and functions take the
// colours of their own. Later rules win over the base theme's. Every colour is an `--astro-code-*`
// variable in global.css.
const codeTheme = createCssVariablesTheme({ name: 'ankor', variablePrefix: '--astro-code-', fontStyle: true });
const plain = { foreground: 'var(--astro-code-foreground)' };

codeTheme.tokenColors = [
  ...(codeTheme.tokenColors ?? []),
  // Variables, parameters, types and imported names, which the base theme colours in part.
  {
    scope: [
      'variable.other.readwrite',
      'variable.other.constant',
      'variable.other.object',
      'variable.other.class',
      'variable.language.this',
      'support.class',
      'support.variable',
      'entity.name.type',
      'entity.other.inherited-class',
      'meta.instance.constructor',
    ],
    settings: plain,
  },
  // Operators are punctuation, and the word operators are keywords.
  { scope: ['keyword.operator', 'punctuation', 'meta.brace'], settings: { foreground: 'var(--astro-code-token-punctuation)' } },
  {
    scope: ['keyword.operator.new', 'keyword.operator.expression', 'constant.language'],
    settings: { foreground: 'var(--astro-code-token-keyword)' },
  },
  // A string's quotes are part of it, as is a JSON key's.
  { scope: ['punctuation.definition.string', 'string punctuation'], settings: { foreground: 'var(--astro-code-token-string)' } },
  { scope: 'support.type.property-name.json punctuation', settings: { foreground: 'var(--astro-code-token-property)' } },
  { scope: 'constant.numeric', settings: { foreground: 'var(--astro-code-token-number)' } },
  { scope: 'string.regexp', settings: { foreground: 'var(--astro-code-token-regexp)' } },
  { scope: 'entity.name.type.parameter', settings: { foreground: 'var(--astro-code-token-type-parameter)' } },
  {
    scope: ['comment.block.documentation', 'comment.block.documentation punctuation'],
    settings: { foreground: 'var(--astro-code-token-doc-comment)', fontStyle: 'italic' },
  },
  // Property names: object keys and members, and JSON and YAML keys, which the base theme files
  // under `keyword` by their exact scopes.
  {
    scope: [
      'support.type.property-name',
      'support.type.property-name.json',
      'entity.name.tag.yaml',
      'meta.object-literal.key',
      'variable.other.property',
      'variable.other.object.property',
    ],
    settings: { foreground: 'var(--astro-code-token-property)' },
  },
];

export default defineConfig({
  base: BASE || '/',
  trailingSlash: 'always',
  // The dev server runs in the toolchain container, whose published port reaches it only on every
  // interface. compose publishes it on the host's 127.0.0.1 alone.
  server: { host: true },
  markdown: {
    shikiConfig: { theme: codeTheme },
    processor: satteri({ mdastPlugins: [contractTags, guideLinks, frameTags, configTags, glossaryTerms], hastPlugins: [externalLinks] }),
  },
  vite: {
    plugins: [tailwindcss(), watchReference()],
    // The prerender bundle imports astro's runtime deps from docs/. Bundling `cookie` keeps astro's
    // version even when another workspace hoists an older one to the root.
    environments: { prerender: { resolve: { noExternal: ['cookie'] } } },
  },
});
