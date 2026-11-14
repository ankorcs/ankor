/**
 * Reads the JSDoc on zod schema declarations, which zod itself never sees. The result follows the
 * shape of each schema, so it can be laid over the JSON Schema that z.toJSONSchema makes of it.
 */
import { readFileSync } from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

export interface DocNode {
  description?: string;
  properties?: Record<string, DocNode>;
  items?: DocNode;
  /** Union branches, in declaration order. */
  options?: DocNode[];
  allOf?: DocNode[];
  /** Docblocks on the keys of the object a `z.enum(...)` is built from, by enum value. */
  enumDescriptions?: Record<string, string>;
  /** A key whose docblock carries `@internal`, which the reference leaves out. */
  internal?: boolean;
}

export interface SourceDocs {
  /** Every top-level const, by name; `description` is the docblock on the declaration. */
  declarations: Map<string, DocNode>;
  /** The docblock that opens each file, by file name without extension. */
  modules: Map<string, string>;
}

interface Declaration {
  initializer: ts.Expression;
  file: SourceFileInfo;
  description: string | undefined;
}

interface SourceFileInfo {
  source: ts.SourceFile;
  locals: Map<string, Declaration>;
  /** Local name to exported name, for `import { A as B }`. */
  imports: Map<string, string>;
}

const PASS_THROUGH = new Set([
  'nullable', 'optional', 'nullish', 'default', 'prefault', 'catch', 'readonly', 'refine', 'superRefine', 'check', 'pipe',
  'min', 'max', 'length', 'int', 'positive', 'nonnegative', 'negative', 'nonpositive', 'finite', 'safe',
  'transform', 'regex', 'startsWith', 'endsWith', 'includes', 'trim', 'toLowerCase', 'toUpperCase', 'describe', 'meta', 'brand',
  // The JSON Schema decides which keys exist and which are optional, so a narrowed object keeps the
  // docs of the keys it kept.
  'omit', 'pick', 'partial', 'required',
]);

/**
 * Lines keep their breaks and indentation, which lists and line breaks are made of; the renderer
 * joins the wrapped lines of a paragraph. Runs of blank lines become one.
 */
function normalise(text: string): string {
  return text.split(/\n[ \t]*\n/)
    .map((block) => block.split('\n').map((line) => line.trimEnd()).join('\n').replace(/^\n+|\n+$/g, ''))
    .filter((block) => block.trim() !== '')
    .join('\n\n');
}

/**
 * The `` - `value`: meaning `` bullets of an enum's docblock, as each value's description, and the
 * text without them. Only values in `values` count, or any value when it is absent; a line indented
 * under a bullet continues it.
 */
export function valueBullets(text: string, values?: readonly unknown[]): { descriptions: Record<string, string>; rest: string } {
  const known = values === undefined ? undefined : new Set(values.map(String));
  const descriptions: Record<string, string> = {};
  const kept: string[] = [];
  let current: string | undefined;

  for (const line of text.split('\n')) {
    const bullet = /^- `([^`]+)`: (.*)$/.exec(line);

    if (bullet !== null && (known === undefined || known.has(bullet[1]!))) {
      current = bullet[1]!;
      descriptions[current] = bullet[2]!;

      continue;
    }

    if (current !== undefined && /^\s+\S/.test(line)) {
      descriptions[current] += ` ${line.trim()}`;

      continue;
    }

    current = undefined;
    kept.push(line);
  }

  return { descriptions, rest: kept.join('\n').replace(/\n{3,}/g, '\n\n').trim() };
}

function docblock(node: ts.Node): string | undefined {
  const texts = ts.getJSDocCommentsAndTags(node)
    .filter(ts.isJSDoc)
    .map((doc) => ts.getTextOfJSDocComment(doc.comment) ?? '')
    .map(normalise)
    .filter(Boolean);

  return texts.length === 0 ? undefined : texts.join('\n\n');
}

function isInternal(node: ts.Node): boolean {
  return ts.getJSDocTags(node).some((tag) => tag.tagName.text === 'internal');
}

function moduleDocblock(source: ts.SourceFile): string | undefined {
  const text = source.getFullText();
  const first = ts.getLeadingCommentRanges(text, 0)?.[0];

  if (first === undefined || !text.startsWith('/**', first.pos)) {
    return undefined;
  }

  const body = text.slice(first.pos + 3, first.end - 2).split('\n').map((line) => line.replace(/^\s*\* ?/, ''));

  return normalise(body.join('\n'));
}

function propertyName(name: ts.PropertyName): string | undefined {
  return ts.isIdentifier(name) || ts.isStringLiteral(name) || ts.isNumericLiteral(name) ? name.text : undefined;
}

function merge(base: DocNode, extra: DocNode): DocNode {
  return { ...base, properties: { ...base.properties, ...extra.properties } };
}

/** `links` rewrites the `{@link …}` tags of every docblock read. */
export function readDocblocks(files: readonly string[], links: (text: string) => string = (text) => text): SourceDocs {
  const described = (node: ts.Node): string | undefined => {
    const text = docblock(node);

    return text === undefined ? undefined : links(text);
  };
  const byExport = new Map<string, Declaration[]>();
  const modules = new Map<string, string>();

  for (const file of files) {
    const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true);
    const info: SourceFileInfo = { source, locals: new Map(), imports: new Map() };
    const moduleDoc = moduleDocblock(source);

    if (moduleDoc !== undefined) {
      modules.set(path.basename(file, path.extname(file)), links(moduleDoc));
    }

    for (const statement of source.statements) {
      if (ts.isImportDeclaration(statement)) {
        const bindings = statement.importClause?.namedBindings;

        if (bindings !== undefined && ts.isNamedImports(bindings)) {
          for (const element of bindings.elements) {
            info.imports.set(element.name.text, (element.propertyName ?? element.name).text);
          }
        }
      }

      if (!ts.isVariableStatement(statement)) {
        continue;
      }

      for (const declaration of statement.declarationList.declarations) {
        if (!ts.isIdentifier(declaration.name) || declaration.initializer === undefined) {
          continue;
        }

        const entry: Declaration = { initializer: declaration.initializer, file: info, description: described(declaration) };

        info.locals.set(declaration.name.text, entry);
        byExport.set(declaration.name.text, [...byExport.get(declaration.name.text) ?? [], entry]);
      }
    }
  }

  const resolving = new Set<Declaration>();
  const resolved = new Map<Declaration, DocNode>();

  function resolve(name: string, file: SourceFileInfo): Declaration | undefined {
    const local = file.locals.get(name);

    if (local !== undefined) {
      return local;
    }

    const imported = file.imports.get(name);
    const candidates = imported === undefined ? [] : byExport.get(imported) ?? [];

    return candidates.length === 1 ? candidates[0] : undefined;
  }

  function declarationDoc(declaration: Declaration): DocNode {
    const done = resolved.get(declaration);

    if (done !== undefined) {
      return done;
    }

    if (resolving.has(declaration)) {
      return {};
    }

    resolving.add(declaration);
    const doc = docOf(declaration.initializer, declaration.file);

    resolving.delete(declaration);
    resolved.set(declaration, doc);

    return doc;
  }

  function objectDoc(literal: ts.ObjectLiteralExpression, file: SourceFileInfo): DocNode {
    const properties: Record<string, DocNode> = {};

    for (const property of literal.properties) {
      if (ts.isSpreadAssignment(property)) {
        Object.assign(properties, docOf(property.expression, file).properties);

        continue;
      }

      if (ts.isShorthandPropertyAssignment(property)) {
        properties[property.name.text] = docOf(property.name, file);

        continue;
      }

      if (!ts.isPropertyAssignment(property)) {
        continue;
      }

      const name = propertyName(property.name);

      if (name === undefined) {
        continue;
      }

      const description = described(property);

      properties[name] = {
        ...docOf(property.initializer, file),
        ...description === undefined ? {} : { description },
        ...isInternal(property) ? { internal: true } : {},
      };
    }

    return { properties };
  }

  function enumDoc(argument: ts.Expression | undefined, file: SourceFileInfo): DocNode {
    let target = argument;

    if (target !== undefined && ts.isIdentifier(target)) {
      target = resolve(target.text, file)?.initializer;
    }

    while (target !== undefined && (ts.isAsExpression(target) || ts.isSatisfiesExpression(target))) {
      target = target.expression;
    }

    // `z.enum([...Other.options, 'value'])` keeps the values that the other enum's docblock describes.
    if (target !== undefined && ts.isArrayLiteralExpression(target)) {
      const spread = Object.assign({}, ...target.elements.filter(ts.isSpreadElement).map((element) => {
        const options = element.expression;
        const source = ts.isPropertyAccessExpression(options) && options.name.text === 'options' && ts.isIdentifier(options.expression)
          ? resolve(options.expression.text, file)?.description
          : undefined;

        return source === undefined ? {} : valueBullets(source).descriptions;
      })) as Record<string, string>;

      return Object.keys(spread).length === 0 ? {} : { enumDescriptions: spread };
    }

    if (target === undefined || !ts.isObjectLiteralExpression(target)) {
      return {};
    }

    const enumDescriptions: Record<string, string> = {};

    for (const property of target.properties) {
      const description = described(property);

      if (ts.isPropertyAssignment(property) && ts.isStringLiteral(property.initializer) && description !== undefined) {
        enumDescriptions[property.initializer.text] = description;
      }
    }

    return Object.keys(enumDescriptions).length === 0 ? {} : { enumDescriptions };
  }

  function elements(argument: ts.Expression | undefined, file: SourceFileInfo): DocNode[] {
    return argument !== undefined && ts.isArrayLiteralExpression(argument)
      ? argument.elements.map((element) => docOf(element, file))
      : [];
  }

  function zodCall(method: string, args: ts.NodeArray<ts.Expression>, file: SourceFileInfo): DocNode {
    switch (method) {
      case 'object':
      case 'looseObject':
      case 'strictObject':
        return args[0] === undefined ? {} : docOf(args[0], file);

      case 'array':
        return args[0] === undefined ? {} : { items: docOf(args[0], file) };

      case 'union':
        return { options: elements(args[0], file) };

      case 'discriminatedUnion':
        return { options: elements(args[1], file) };

      case 'enum':
        return enumDoc(args[0], file);

      default:
        return {};
    }
  }

  function docOf(expression: ts.Expression, file: SourceFileInfo): DocNode {
    if (ts.isParenthesizedExpression(expression) || ts.isAsExpression(expression)
      || ts.isSatisfiesExpression(expression) || ts.isNonNullExpression(expression)) {
      return docOf(expression.expression, file);
    }

    if (ts.isIdentifier(expression)) {
      const declaration = resolve(expression.text, file);

      // Structure only: a const's own docblock describes the schema, not each field that uses it.
      return declaration === undefined ? {} : { ...declarationDoc(declaration), description: undefined };
    }

    if (ts.isObjectLiteralExpression(expression)) {
      return objectDoc(expression, file);
    }

    if (ts.isPropertyAccessExpression(expression)) {
      const target = docOf(expression.expression, file);

      return expression.name.text === 'shape' ? target : target.properties?.[expression.name.text] ?? {};
    }

    if (!ts.isCallExpression(expression) || !ts.isPropertyAccessExpression(expression.expression)) {
      return {};
    }

    const method = expression.expression.name.text;
    const receiver = expression.expression.expression;
    const [argument] = expression.arguments;

    if (ts.isIdentifier(receiver) && receiver.text === 'z') {
      return zodCall(method, expression.arguments, file);
    }

    if (method === 'extend' && argument !== undefined) {
      return merge(docOf(receiver, file), docOf(argument, file));
    }

    if (method === 'and' && argument !== undefined) {
      return { allOf: [docOf(receiver, file), docOf(argument, file)] };
    }

    if (method === 'or' && argument !== undefined) {
      return { options: [docOf(receiver, file), docOf(argument, file)] };
    }

    return PASS_THROUGH.has(method) ? docOf(receiver, file) : {};
  }

  /** The docblock of the constant a schema is built from: the object or list of `z.enum(X)`, or the pattern of `.regex(X)`. */
  function sourceDescription(declaration: Declaration): string | undefined {
    let expression: ts.Expression = declaration.initializer;

    while (ts.isCallExpression(expression) && ts.isPropertyAccessExpression(expression.expression)) {
      const [argument] = expression.arguments;
      const method = expression.expression.name.text;

      if ((method === 'enum' || method === 'regex') && argument !== undefined && ts.isIdentifier(argument)) {
        return resolve(argument.text, declaration.file)?.description;
      }

      expression = expression.expression.expression;
    }

    return undefined;
  }

  const declarations = new Map<string, DocNode>();

  for (const [name, entries] of byExport) {
    const [entry] = entries;

    // A name declared in two files is ambiguous; neither is described rather than the wrong one.
    if (entries.length === 1 && entry !== undefined) {
      declarations.set(name, { ...declarationDoc(entry), description: entry.description ?? sourceDescription(entry) });
    }
  }

  return { declarations, modules };
}
