// Rewrites the relative links of the files that npm shows or ships with @ankorcs/protocol, its README
// and CHANGELOG, because npm resolves no relative link. The source keeps relative links, which read
// on GitHub, and only the published copy is rewritten.
//
//   node scripts/npm-readme.mjs --check            # fails on a link to a missing page or file
//   node scripts/npm-readme.mjs --commit <sha>     # rewrites the files in place, before npm publish
//
// A link to a docs page, a Markdown file with a `slug` in its frontmatter, becomes its address on the
// docs site. Any other file or directory becomes its GitHub address at the commit. Absolute
// addresses and anchors in the same file stay as they are.

import console from 'node:console';
import { existsSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { isExternal, mapLinks } from './markdown-links.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const FILES = ['packages/protocol/README.md', 'packages/protocol/CHANGELOG.md'];
const DOCS_SITE = 'https://docs.ankor.sh';
const REPOSITORY = 'https://github.com/ankorcs/ankor';

function usage(message) {
  console.error(`${message}\nusage: node scripts/npm-readme.mjs --check | --commit <sha>`);
  process.exit(2);
}

/** The `slug` in a Markdown file's frontmatter, or null for a file without one. */
function slugOf(file) {
  const frontmatter = /^---\n([\s\S]*?)\n---\n/.exec(readFileSync(file, 'utf8'));
  const slug = frontmatter === null ? null : /^slug:\s*(\S+)\s*$/m.exec(frontmatter[1]);

  return slug === null ? null : slug[1].replace(/^["']|["']$/g, '');
}

const problems = [];

/** The published address of `target`, linked from `file`. A target that does not exist is a problem. */
function published(file, target, commit) {
  if (isExternal(target) || target.startsWith('#')) {
    return target;
  }

  const [relative, hash = ''] = target.split(/(?=#)/);
  const resolved = path.resolve(path.dirname(file), decodeURIComponent(relative));
  const inRepository = path.relative(ROOT, resolved);

  if (inRepository.startsWith('..') || !existsSync(resolved)) {
    problems.push(`${path.relative(ROOT, file)}: link to missing file ${target}`);

    return target;
  }

  const slug = resolved.endsWith('.md') ? slugOf(resolved) : null;

  if (slug !== null) {
    return `${DOCS_SITE}/${slug === 'index' ? '' : `${slug}/`}${hash}`;
  }

  const kind = statSync(resolved).isDirectory() ? 'tree' : 'blob';

  return `${REPOSITORY}/${kind}/${commit}/${inRepository.split(path.sep).join('/')}${hash}`;
}

/** The file's text with every link published; fenced code and code spans stay as written. */
function rewrite(file, commit) {
  return mapLinks(readFileSync(file, 'utf8'), (target) => published(file, target, commit));
}

const [mode, commit] = process.argv.slice(2);

if (mode !== '--check' && mode !== '--commit') {
  usage(`unknown mode ${mode ?? '(none)'}`);
}

if (mode === '--commit' && !/^[0-9a-f]{7,40}$/.test(commit ?? '')) {
  usage('--commit takes a commit hash');
}

const results = FILES.map((name) => path.join(ROOT, name)).map((file) => [file, rewrite(file, commit ?? 'HEAD')]);

if (problems.length > 0) {
  console.error(problems.join('\n'));
  process.exit(1);
}

if (mode === '--commit') {
  for (const [file, text] of results) {
    writeFileSync(file, text);
  }
}

console.log(`${mode === '--check' ? 'checked' : 'rewrote'} the links of ${FILES.join(' and ')}`);
