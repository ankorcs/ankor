// Reads the contract's JSDoc from its sources, so the generated Rust carries the same text as the
// docs site. zod keeps no comments at runtime, so this walks the source: an export's JSDoc, and the
// JSDoc of each field, through `z.object`, `.extend`, spreads, `.shape` and wrappers such as
// `.nullable()`.
import { readFileSync } from 'node:fs';
import ts from 'typescript';

function unwrap(node) {
  while (ts.isAsExpression(node) || ts.isSatisfiesExpression(node) || ts.isParenthesizedExpression(node) || ts.isTypeAssertionExpression?.(node)) {
    node = node.expression;
  }

  return node;
}

function jsdoc(node, text) {
  const ranges = ts.getLeadingCommentRanges(text, node.getFullStart()) ?? [];
  const last = ranges.filter((range) => text.startsWith('/**', range.pos)).at(-1);

  if (!last) return undefined;

  const lines = text
    .slice(last.pos + 3, last.end - 2)
    .split('\n')
    .map((line) => line.replace(/^\s*\* ?/, '').trimEnd());
  const end = lines.findIndex((line) => line.startsWith('@'));
  const body = (end === -1 ? lines : lines.slice(0, end)).join('\n').trim();

  return body ? toRust(body) : undefined;
}

/** `{@link X | label}` and `{@link X}` become code spans, which rustdoc shows as the docs site does. */
function toRust(body) {
  return body
    .replace(/\{@link\s+([^}|]+?)\s*\|\s*([^}]+?)\s*\}/g, (_, target, label) => (label.startsWith('`') ? label : `\`${label}\``))
    .replace(/\{@link\s+([^}]+?)\s*\}/g, (_, target) => `\`${target}\``);
}

export function loadDocs(files) {
  const files_ = files.map((file) => {
    const text = readFileSync(file, 'utf8');
    const kind = file.endsWith('.ts') ? ts.ScriptKind.TS : ts.ScriptKind.JS;

    return { file, text, source: ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, kind) };
  });
  const local = new Map();
  const global = new Map();

  for (const { file, text, source } of files_) {
    const own = new Map();

    for (const statement of source.statements) {
      if (!ts.isVariableStatement(statement)) continue;

      const doc = jsdoc(statement, text);
      const exported = statement.modifiers?.some((m) => m.kind === ts.SyntaxKind.ExportKeyword);

      for (const declaration of statement.declarationList.declarations) {
        if (!ts.isIdentifier(declaration.name) || !declaration.initializer) continue;

        const entry = { doc, init: declaration.initializer, text, file };

        own.set(declaration.name.text, entry);
        if (exported && !global.has(declaration.name.text)) global.set(declaration.name.text, entry);
      }
    }

    local.set(file, own);
  }

  const lookup = (name, file) => local.get(file)?.get(name) ?? global.get(name);

  /** The fields an expression defines: key to `{ doc, init, file, text }`. */
  function shapeOf(node, file, seen = new Set()) {
    node = unwrap(node);

    if (seen.has(node)) return new Map();
    seen.add(node);

    if (ts.isIdentifier(node)) {
      const entry = lookup(node.text, file);

      return entry ? shapeOf(entry.init, entry.file, seen) : new Map();
    }

    if (ts.isPropertyAccessExpression(node) && node.name.text === 'shape') {
      return shapeOf(node.expression, file, seen);
    }

    if (ts.isObjectLiteralExpression(node)) {
      const out = new Map();
      const { text } = files_.find((f) => f.file === file);

      for (const property of node.properties) {
        if (ts.isSpreadAssignment(property)) {
          for (const [key, value] of shapeOf(property.expression, file, seen)) out.set(key, value);
        } else if (ts.isPropertyAssignment(property) && (ts.isIdentifier(property.name) || ts.isStringLiteral(property.name))) {
          out.set(property.name.text, { doc: jsdoc(property, text), init: property.initializer, file, text });
        }
      }

      return out;
    }

    if (ts.isCallExpression(node)) {
      const callee = unwrap(node.expression);

      if (ts.isPropertyAccessExpression(callee)) {
        const method = callee.name.text;

        if (method === 'object' || method === 'strictObject' || method === 'looseObject') {
          return node.arguments[0] ? shapeOf(node.arguments[0], file, seen) : new Map();
        }

        if (method === 'extend' || method === 'merge' || method === 'safeExtend') {
          const out = shapeOf(callee.expression, file, seen);

          // A field that the extension redefines without a comment keeps the base field's.
          for (const [key, value] of node.arguments[0] ? shapeOf(node.arguments[0], file, seen) : []) {
            out.set(key, value.doc === undefined && out.get(key) ? { ...value, doc: out.get(key).doc } : value);
          }

          return out;
        }

        // `z.array(X)` and the like name their schema in the argument; any other call, such as
        // `.max(64)` or `.nullable()`, returns a schema with the shape of the one it is called on.
        if (ts.isIdentifier(callee.expression) && callee.expression.text === 'z') {
          return node.arguments[0] ? shapeOf(node.arguments[0], file, seen) : new Map();
        }

        return shapeOf(callee.expression, file, seen);
      }
    }

    return new Map();
  }

  /** The n-th option of a `z.union` or `z.discriminatedUnion`, through calls such as `.superRefine()`. */
  function optionOf(node, file, index) {
    node = unwrap(node);

    if (ts.isIdentifier(node)) {
      const entry = lookup(node.text, file);

      return entry ? optionOf(entry.init, entry.file, index) : undefined;
    }

    if (!ts.isCallExpression(node)) return undefined;

    const callee = unwrap(node.expression);

    if (!ts.isPropertyAccessExpression(callee)) return undefined;

    if (ts.isIdentifier(callee.expression) && callee.expression.text === 'z') {
      const list = unwrap(node.arguments[callee.name.text === 'discriminatedUnion' ? 1 : 0] ?? node);

      return ts.isArrayLiteralExpression(list) && list.elements[index] ? { init: list.elements[index], file } : undefined;
    }

    return optionOf(callee.expression, file, index);
  }

  /** The doc at a generator path: `Export`, `Export.field`, `Export.field.item.inner` and so on. */
  function at(where) {
    const [name, ...rest] = where.split('.');
    const root = global.get(name);

    if (!root) return undefined;

    let entry = root;

    for (const key of rest.filter((key) => key !== 'item' && key !== 'in')) {
      entry = /^\d+$/.test(key) ? optionOf(entry.init, entry.file, Number(key)) : shapeOf(entry.init, entry.file).get(key);

      if (!entry) return undefined;
    }

    return entry.doc;
  }

  return { at };
}

/** A doc as Rust doc-comment lines at an indent. */
export function rustDoc(doc, pad = '') {
  return doc ? `${doc.split('\n').map((line) => (line ? `${pad}/// ${line}` : `${pad}///`)).join('\n')}\n` : '';
}
