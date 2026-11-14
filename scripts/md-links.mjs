// Checks every relative link in the repository's tracked Markdown files: the file or directory it
// names must exist, and an `#anchor` must name a heading or an HTML id in the Markdown file it
// points to. The docs site checks its own built pages, so a site path such as `/reference/` is
// left to that check.
//
//   node scripts/md-links.mjs

import { execFileSync } from 'node:child_process';
import console from 'node:console';
import { existsSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { anchorsOf, isExternal, mapLinks } from './markdown-links.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const files = execFileSync('git', ['ls-files', '--', '*.md'], { cwd: ROOT, encoding: 'utf8' }).split('\n').filter(Boolean);
const anchorCache = new Map();
const problems = [];

function anchors(file) {
  if (!anchorCache.has(file)) {
    anchorCache.set(file, anchorsOf(file));
  }

  return anchorCache.get(file);
}

for (const name of files) {
  const file = path.join(ROOT, name);

  mapLinks(readFileSync(file, 'utf8'), (target, line) => {
    if (isExternal(target) || target.startsWith('/')) {
      return target;
    }

    const [relative, hash = ''] = target.split('#');
    const resolved = relative === '' ? file : path.resolve(path.dirname(file), decodeURIComponent(relative));

    if (path.relative(ROOT, resolved).startsWith('..') || !existsSync(resolved)) {
      problems.push(`${name}:${line}: no file ${target}`);
    } else if (hash !== '' && resolved.endsWith('.md') && statSync(resolved).isFile() && !anchors(resolved).has(decodeURIComponent(hash))) {
      problems.push(`${name}:${line}: no anchor #${hash} in ${path.relative(ROOT, resolved)}`);
    }

    return target;
  });
}

if (problems.length > 0) {
  console.error(problems.join('\n'));
  process.exit(1);
}

console.log(`checked the links of ${files.length} Markdown files`);
