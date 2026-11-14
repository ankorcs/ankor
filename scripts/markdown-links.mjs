// Reads the links in Markdown files, which scripts/md-links.mjs checks and scripts/npm-readme.mjs
// rewrites for npm. Fenced code and code spans hold no links.

import { readFileSync } from 'node:fs';

/** An inline link or image's target, with an optional title, or a reference definition's. */
const INLINE_LINK = /(]\()([^)\s]+)((?:\s+"[^"]*")?\))/g;
const DEFINITION = /^(\s{0,3}\[[^\]]+]:\s*)(\S+)(.*)$/;

/**
 * The file's text with each link target replaced by what `replace(target, line)` returns. Fenced
 * code and code spans stay as they are.
 */
export function mapLinks(text, replace) {
  let fenced = false;
  // A code span may wrap onto the next line of its paragraph.
  let inSpan = false;

  return text.split('\n').map((line, index) => {
    if (/^\s*(```|~~~)/.test(line)) {
      fenced = !fenced;

      return line;
    }

    if (fenced) {
      return line;
    }

    if (line.trim() === '') {
      inSpan = false;

      return line;
    }

    const definition = inSpan ? null : DEFINITION.exec(line);

    if (definition !== null) {
      return `${definition[1]}${replace(definition[2], index + 1)}${definition[3]}`;
    }

    return line.split('`').map((part, at) => {
      if (at > 0) {
        inSpan = !inSpan;
      }

      return inSpan ? part : part.replace(INLINE_LINK, (_, open, target, close) => `${open}${replace(target, index + 1)}${close}`);
    }).join('`');
  }).join('\n');
}

/** Whether a target leaves the repository: a URL with a scheme, or a protocol-relative one. */
export function isExternal(target) {
  return /^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('//');
}

/**
 * The anchors a Markdown file offers: its headings as GitHub names them, and the `id`s and
 * `name`s of its HTML. A heading repeated gets `-1`, `-2` and so on, as on GitHub.
 */
export function anchorsOf(file) {
  const anchors = new Set();
  const seen = new Map();
  let fenced = false;

  for (const line of readFileSync(file, 'utf8').split('\n')) {
    if (/^\s*(```|~~~)/.test(line)) {
      fenced = !fenced;

      continue;
    }

    if (fenced) {
      continue;
    }

    for (const [, id] of line.matchAll(/\s(?:id|name)="([^"]+)"/g)) {
      anchors.add(id);
    }

    const heading = /^#{1,6}\s+(.*?)\s*#*\s*$/.exec(line);

    if (heading === null) {
      continue;
    }

    const base = heading[1]
      .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/<[^>]+>/g, '')
      .toLowerCase()
      .replace(/[^\p{L}\p{N}\s_-]/gu, '')
      .replace(/\s/g, '-');
    const count = seen.get(base) ?? 0;

    seen.set(base, count + 1);
    anchors.add(count === 0 ? base : `${base}-${count}`);
  }

  return anchors;
}
