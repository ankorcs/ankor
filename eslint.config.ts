// Flat config for every workspace. Each package's tsconfig fails the build on unused parameters
// (`_` prefix exempt); ESLint reports unused locals, counting `{@link}` in JSDoc as a use, and adds
// the recommended correctness rules, type-only imports and unused caught errors.
import js from '@eslint/js';
import stylistic from '@stylistic/eslint-plugin';
import { defineConfig, globalIgnores } from 'eslint/config';
import globals from 'globals';
import tseslint from 'typescript-eslint';
import type { Rule } from 'eslint';

// The protocol reference and the editor both follow `{@link Name}`, so a file imports what its
// JSDoc links to even when no code uses it. Neither ESLint nor its TypeScript rules read JSDoc.
const jsdocLinks: Rule.RuleModule = {
  meta: { type: 'problem', schema: [] },
  create(context) {
    return {
      Program(program) {
        for (const comment of context.sourceCode.getAllComments()) {
          for (const link of comment.value.matchAll(/\{@link\s+([A-Za-z_$][\w$]*)/g)) {
            context.sourceCode.markVariableAsUsed(link[1]!, program);
          }
        }
      },
    };
  },
};

export default defineConfig([
  globalIgnores([
    '**/dist/**',
    '**/build/**',
    '**/coverage/**',
    '**/node_modules/**',
    'docs/.astro/**',
    // Packages outside the npm workspaces, with their own tooling.
    'packages/agent/**',
    'packages/plugin/**',
  ]),
  js.configs.recommended,
  tseslint.configs.recommended,
  {
    plugins: { '@stylistic': stylistic },
    rules: {
      curly: ['error', 'all'],
      '@stylistic/brace-style': ['error', '1tbs'],
      '@stylistic/indent': ['error', 2, { SwitchCase: 1 }],
      '@stylistic/lines-between-class-members': ['error', 'always', { exceptAfterSingleLine: true }],
      '@stylistic/padding-line-between-statements': [
        'error',
        { blankLine: 'always', prev: '*', next: 'block-like' },
        { blankLine: 'always', prev: 'block-like', next: '*' },
        { blankLine: 'always', prev: '*', next: ['return', 'throw', 'break', 'continue'] },
        { blankLine: 'always', prev: ['const', 'let'], next: '*' },
        { blankLine: 'any', prev: ['const', 'let'], next: ['const', 'let'] },
        { blankLine: 'always', prev: 'import', next: '*' },
        { blankLine: 'any', prev: 'import', next: 'import' },
        { blankLine: 'always', prev: '*', next: 'export' },
      ],
    },
  },
  {
    files: ['**/*.{ts,mts,cts}'],
    plugins: { ankor: { rules: { 'jsdoc-links': jsdocLinks } } },
    languageOptions: {
      parserOptions: { parser: tseslint.parser },
      globals: { ...globals.node, ...globals.browser },
    },
    rules: {
      'ankor/jsdoc-links': 'error',
      // Same contract as noUnusedParameters: a `_`-prefixed parameter is there for arity or
      // inference, not for use.
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_' }],
      '@typescript-eslint/consistent-type-imports': ['error', { fixStyle: 'inline-type-imports' }],
      // A snake_case name is wire or database data (DEVELOPMENT.md conventions); bindings stay camelCase.
      '@typescript-eslint/naming-convention': ['error',
        { selector: 'variable', format: ['camelCase', 'UPPER_CASE', 'PascalCase'], leadingUnderscore: 'allow' },
        { selector: 'parameter', format: ['camelCase', 'PascalCase'], leadingUnderscore: 'allow' },
      ],
      // Regexes that strip ANSI sequences and control characters are the point, not a slip.
      'no-control-regex': 'off',
      // `catch {}` around best-effort cleanup is deliberate; an empty block elsewhere is not.
      'no-empty': ['error', { allowEmptyCatch: true }],
      // The padding rule puts a blank line before a case that opens a block; empty labels above it stay fallthroughs.
      'no-fallthrough': ['error', { allowEmptyCase: true }],
    },
  },
]);
