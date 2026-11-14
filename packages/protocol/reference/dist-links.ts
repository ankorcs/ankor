/**
 * Makes the guide links in the comments of dist/ absolute, after tsc and generate.ts, so the
 * JavaScript and declarations that the package ships link to the docs site, as its AsyncAPI document
 * does. A docblock links a guide from the site root, such as `[Versioning](/protocol/versioning/)`.
 *
 * Only comments change. Each file is printed without its comments before and after the rewrite, and
 * the build fails if the two differ, such as when a string held a link. tsconfig.dist.json then
 * checks that dist/ parses and typechecks.
 */
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import ts from 'typescript';
import { absoluteGuides } from './asyncapi.ts';

const DIST = path.join(import.meta.dirname, '..', 'dist');

/** The file parsed as JavaScript or as declarations. */
function parse(name: string, text: string): ts.SourceFile {
  return ts.createSourceFile(name, text, ts.ScriptTarget.Latest, false, name.endsWith('.d.ts') ? ts.ScriptKind.TS : ts.ScriptKind.JS);
}

/** Every comment in the file, as [start, end) ranges in order, each once. */
function commentRanges(name: string, text: string): [number, number][] {
  const source = parse(name, text);
  const ranges = new Map<number, number>();
  const add = (list: ts.CommentRange[] | undefined): void => {
    for (const range of list ?? []) {
      ranges.set(range.pos, range.end);
    }
  };
  const visit = (node: ts.Node): void => {
    add(ts.getLeadingCommentRanges(text, node.pos));
    add(ts.getTrailingCommentRanges(text, node.end));
    node.getChildren(source).forEach(visit);
  };

  visit(source);

  return [...ranges].sort(([a], [b]) => a - b);
}

/** The file printed without its comments, which the rewrite must leave as it is. */
function code(name: string, text: string): string {
  return ts.createPrinter({ removeComments: true }).printFile(parse(name, text));
}

const problems: string[] = [];
const files = readdirSync(DIST, { recursive: true, encoding: 'utf8' })
  .filter((name) => name.endsWith('.js') || name.endsWith('.d.ts'))
  .map((name) => path.join(DIST, name));

for (const file of files) {
  const text = readFileSync(file, 'utf8');
  let rewritten = '';
  let at = 0;

  for (const [start, end] of commentRanges(file, text)) {
    rewritten += text.slice(at, start) + absoluteGuides(text.slice(start, end));
    at = end;
  }

  rewritten += text.slice(at);

  if (code(file, rewritten) !== code(file, text)) {
    problems.push(`${path.relative(DIST, file)}: the rewrite changed code outside the comments`);
  } else if (rewritten !== text) {
    writeFileSync(file, rewritten);
  }
}

if (problems.length > 0) {
  throw new Error(`guide links rewritten outside comments:\n${problems.join('\n')}`);
}
