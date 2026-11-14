/**
 * Fails the build on a link inside the site that points at a missing page or a missing anchor.
 * The markdown plugin already fails on links to missing files; this also checks the fragments.
 */
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { BASE } from '../src/site.ts';

const DIST = path.join(import.meta.dirname, '..', 'dist');

function pages(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);

    if (entry.isDirectory()) {
      return entry.name === '_astro' || entry.name === 'pagefind' ? [] : pages(full);
    }

    return entry.name.endsWith('.html') ? [full] : [];
  });
}

function route(file: string): string {
  const relative = path.relative(DIST, file).split(path.sep).join('/');

  return `/${relative.replace(/(^|\/)index\.html$/, '$1').replace(/\.html$/, '/')}`;
}

const html = new Map(pages(DIST).map((file) => [route(file), readFileSync(file, 'utf8')]));
const ids = new Map([...html].map(([page, text]) => [page, new Set([...text.matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]))]));
const problems: string[] = [];

for (const [page, text] of html) {
  for (const [, href = ''] of text.matchAll(/\bhref="([^"]+)"/g)) {
    if (!href.startsWith('/') && !href.startsWith('#')) {
      continue;
    }

    if (href.startsWith('/') && !href.startsWith(`${BASE}/`)) {
      problems.push(`${page}: ${href} (outside ${BASE}/)`);

      continue;
    }

    const [target = '', anchor] = href.startsWith('#') ? [page, href.slice(1)] : href.slice(BASE.length).split('#');

    if (/^\/(_astro|pagefind)\//.test(target)) {
      continue;
    }

    // A file the build writes beside the pages, such as llms.txt or asyncapi.yaml.
    if (/\.[a-z0-9]+$/.test(target)) {
      if (!existsSync(path.join(DIST, target))) {
        problems.push(`${page}: ${href} (no such file)`);
      }

      continue;
    }

    const found = ids.get(target.endsWith('/') ? target : `${target}/`);

    if (found === undefined) {
      problems.push(`${page}: ${href} (no such page)`);
    } else if (anchor !== undefined && anchor !== '' && !found.has(decodeURIComponent(anchor))) {
      problems.push(`${page}: ${href} (no such anchor)`);
    }
  }
}

if (problems.length > 0) {
  console.error(`${problems.length} broken links:\n${[...new Set(problems)].join('\n')}`);
  process.exit(1);
}

console.log(`links: ${html.size} pages checked`);
