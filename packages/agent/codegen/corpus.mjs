import { writeFileSync } from 'node:fs';
import { container as contract, protocol } from './protocol.mjs';
import RandExp from 'randexp';

const PER_SCHEMA = Number(process.env.PER_SCHEMA ?? 60);

let seed = Number(process.env.SEED ?? 0x2545f491);
const random = () => {
  seed ^= seed << 13;
  seed ^= seed >>> 17;
  seed ^= seed << 5;

  return (seed >>> 0) / 0x100000000;
};

RandExp.prototype.randInt = (low, high) => low + Math.floor(random() * (1 + high - low));

const uuid = () => 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => (c === 'x' ? Math.floor(random() * 16) : 8 + Math.floor(random() * 4)).toString(16));
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (p) => random() < p;
// Off while building the valid payloads the edge cases start from.
let noise = true;
const noisy = (p) => noise && chance(p);

const WRONG = [null, true, 0, -1, 1.5, '', 'x', [], {}, [1], { a: 1 }, 1e20, 'A'.repeat(300)];
const TEXT = ['', ' ', 'a', 'abc', 'Ünïcødé', '😀😀', 'tab\there', 'nul\u0000', 'line\nbreak', 'ctl\u0007', '  padded  ', 'UPPER', 'http://example.com', 'https://a.b/c?d#e', 'ftp://x', 'cs.example.com', '192.168.0.10:3000', '[fd00::1]', 'a/b', '10.0.0.1', '256.1.1.1'];

function lengthy(def) {
  const checks = def.checks ?? [];
  const max = checks.find((c) => c._zod.def.check === 'max_length')?._zod.def.maximum;
  const min = checks.find((c) => c._zod.def.check === 'min_length')?._zod.def.minimum;

  return { max, min };
}

function stringSample(schema) {
  const def = schema._zod.def;
  const patterns = [def.pattern, ...(def.checks ?? []).map((c) => c._zod.def.pattern)].filter(Boolean);
  const { max, min } = lengthy(def);
  const options = [() => pick(TEXT)];

  for (const pattern of patterns) {
    options.push(() => {
      try {
        const generator = new RandExp(pattern);

        generator.max = 8;

        return generator.gen();
      } catch {
        return pick(TEXT);
      }
    });
  }

  if (def.format === 'uuid') {
    options.push(uuid, uuid, () => uuid().toUpperCase());
  }

  if (def.format === 'url') {
    options.push(() => pick(['https://up.example.com/demos', ' http://x.y/z ', 'https:/broken', 'HTTP://CAPS.example', 'https://exa\nmple.com/p', 'mailto:a@b.c']));
  }

  for (const bound of [max, min].filter((b) => b !== undefined)) {
    options.push(() => 'a'.repeat(Math.max(0, bound + pick([-1, 0, 1]))), () => '😀'.repeat(Math.max(0, bound + pick([-1, 0, 1]))));
  }

  return pick(options)();
}

function numberSample(schema) {
  const checks = schema._zod.def.checks ?? [];
  const bounds = checks.map((c) => c._zod.def.value).filter((v) => typeof v === 'number');
  const near = bounds.flatMap((b) => [b - 1, b, b + 1]);

  return pick([0, 1, 2, 3, 7, 27015, 4500, 65535, 65536, -5, 0.5, 2 ** 53, 1700000000000, ...near, ...near]);
}

function sample(schema, depth = 0) {
  const def = schema._zod.def;

  if (depth > 0 && noisy(0.03)) {
    return pick(WRONG);
  }

  switch (def.type) {
    case 'string':
      return stringSample(schema);
    case 'number':
      return numberSample(schema);
    case 'boolean':
      return chance(0.5);
    case 'unknown':
      return pick([1, 'x', null, { a: 1 }]);
    case 'literal':
      return pick(def.values);
    case 'enum':
      return !noise || chance(0.95) ? pick([...schema._zod.values]) : 'not_an_option';
    case 'object': {
      const out = {};

      for (const [key, field] of Object.entries(def.shape)) {
        const omittable = field._zod.optin !== undefined;

        if ((omittable && chance(0.4)) || noisy(0.01)) {
          continue;
        }

        out[key] = sample(field, depth + 1);
      }

      if (noisy(0.1)) {
        out.unexpected_key = 1;
      }

      return out;
    }
    case 'array': {
      const { max } = lengthy(def);
      const length = max !== undefined && noisy(0.05) ? max + 1 : Math.floor(random() * (depth > 3 ? 2 : 4));

      return Array.from({ length }, () => sample(def.element, depth + 1));
    }
    case 'union':
      return sample(pick(def.options), depth + 1);
    case 'nullable':
      return chance(0.3) ? null : sample(def.innerType, depth + 1);
    case 'optional':
    case 'default':
    case 'prefault':
      return sample(def.innerType, depth + 1);
    case 'pipe':
      return sample(def.in, depth + 1);
    case 'transform':
      return undefined;
    case 'intersection':
      return { ...sample(def.left, depth + 1), extra: 'kept' };
    case 'record':
      return { a: 1, b: 'two' };
    default:
      throw new Error(`no sampler for ${def.type}`);
  }
}

const strip = (value) => JSON.parse(JSON.stringify(value) ?? 'null');

// Edge cases. Each schema gets a valid payload, then copies of it in which one value sits at an
// edge of its rule, just inside or just past it. Every other value stays valid, so each copy
// tests one rule, and a refinement is not hidden behind an earlier failure. The contract judges
// every copy, as it judges the random cases.
const OMIT = Symbol('omit');
const EDGE_DEPTH = 6;

// The smallest payload the contract accepts among a few tries, so edge copies stay small.
function smallest(schema, make, tries) {
  let best;

  for (let index = 0; index < tries; index++) {
    const value = strip(make());

    try {
      if (schema.safeParse(value).success && (best === undefined || JSON.stringify(value).length < JSON.stringify(best).length)) {
        best = value;
      }
    } catch {
      // A transform that throws rejects the sample.
    }
  }

  return best;
}

// Builds a valid payload from valid parts: each field is valid on its own before the whole is
// checked, which random sampling of a frame with many fields rarely reaches.
function build(schema, depth) {
  const def = schema._zod.def;

  if (depth > EDGE_DEPTH + 2) {
    return sample(schema);
  }

  switch (def.type) {
    case 'object': {
      const out = {};

      for (const [key, field] of Object.entries(def.shape)) {
        const value = field._zod.optin !== undefined && chance(0.3) ? undefined : validPart(field, depth + 1);

        if (value !== undefined) {
          out[key] = value;
        }
      }

      return out;
    }
    case 'array': {
      const { min } = lengthy(def);

      return Array.from({ length: Math.max(1, min ?? 0) }, () => validPart(def.element, depth + 1));
    }
    case 'union':
      return validPart(pick(def.options), depth + 1);
    case 'nullable':
      return chance(0.5) ? null : validPart(def.innerType, depth + 1);
    case 'optional':
    case 'default':
    case 'prefault':
      return validPart(def.innerType, depth + 1);
    case 'pipe':
      return validPart(def.in, depth + 1);
    case 'intersection':
      return { ...validPart(def.left, depth + 1), ...validPart(def.right, depth + 1) };
    default:
      return sample(schema);
  }
}

// A pool of distinct valid parts per schema, built once. A frame draws from the pools of the
// types it shares with others, and distinct parts let rules such as unique players pass.
const POOL = 12;
const parts = new Map();

function validPart(schema, depth) {
  if (!parts.has(schema)) {
    const pool = new Map();

    for (let index = 0; index < POOL * 16 && pool.size < POOL; index++) {
      const value = strip(build(schema, depth));

      try {
        if (schema.safeParse(value).success) {
          pool.set(JSON.stringify(value), value);
        }
      } catch {
        // A transform that throws rejects the sample.
      }
    }

    parts.set(schema, [...pool.values()].sort((x, y) => JSON.stringify(x).length - JSON.stringify(y).length));
  }

  const pool = parts.get(schema);

  return pool.length === 0 ? undefined : pick(pool.slice(0, Math.max(1, Math.ceil(pool.length / 2))));
}

// Rules that tie one field to another, which a payload built from valid parts cannot meet. Each
// fix-up copies values across before the contract judges the payload.
const FIXUPS = new Map([
  [
    protocol.AgentHello,
    (hello) => {
      // An idle slot, so the rules that tie a slot's fields to its state hold whatever was built.
      const built = strip(build(protocol.AgentHello._zod.def.shape.slot_status._zod.def.element, 1));
      const status = { ...built, state: 'idle', allocation_id: null, match_id: null, server_steam_id: null };
      const slots = Array.isArray(hello.config?.slots) ? hello.config.slots : [];

      return { ...hello, slot_status: slots.map((slot) => ({ ...status, slot_id: slot.id })) };
    },
  ],
]);

// Rules that count across several arrays, whose edges no single field reaches. Each gives inputs
// at the edge, and the contract judges them as it judges every case.
const steamIds = (count) => Array.from({ length: count }, (_, index) => String(76561198000000000n + BigInt(index + 1)));
const rosterOf = (count) => {
  const ids = steamIds(count);
  const half = Math.ceil(count / 2);

  return { team1: { name: 'Alpha', players: ids.slice(0, half) }, team2: { name: 'Bravo', players: ids.slice(half) }, spectators: [] };
};
const COUNT_EDGES = new Map([
  [protocol.Roster, () => [protocol.MAX_SERVER_PLAYERS, protocol.MAX_SERVER_PLAYERS + 1].map(rosterOf)],
  [
    protocol.ServerLaunchConfig,
    (launch) => {
      let matchSchema = protocol.ServerLaunchConfig._zod.def.shape.match;

      while (matchSchema._zod.def.innerType) matchSchema = matchSchema._zod.def.innerType;

      const game = validPart(matchSchema, 1);

      if (game === undefined) return [];

      const players = 10;
      const match = { ...game, restore: null, roster: rosterOf(players) };
      const stock = { ...launch, workshop_id: null, map_name: launch.map_name ?? 'de_dust2', match };

      return [players, players - 1].map((max_players) => ({ ...stock, max_players }));
    },
  ],
]);

function validBase(schema, tries = 60) {
  const fixup = FIXUPS.get(schema) ?? ((value) => value);

  noise = false;

  try {
    return smallest(schema, () => fixup(strip(build(schema, 0))), tries) ?? smallest(schema, () => sample(schema), tries * 5);
  } finally {
    noise = true;
  }
}

function numberBounds(def) {
  const checks = def.checks ?? [];
  const bounds = checks.map((c) => c._zod.def.value).filter((v) => typeof v === 'number');

  if (checks.some((c) => c._zod.def.format === 'safeint')) {
    bounds.push(Number.MAX_SAFE_INTEGER, Number.MIN_SAFE_INTEGER);
  }

  return bounds;
}

function leafEdges(schema, base) {
  const def = schema._zod.def;

  switch (def.type) {
    case 'string': {
      const { max, min } = lengthy(def);
      const text = typeof base === 'string' ? base : '';
      const fill = Array.from(text).at(-1) ?? 'a';
      // Whole characters, since half of a surrogate pair is not valid JSON text.
      const sized = (length) => (text.length >= length ? Array.from(text).slice(0, length).join('') : text + fill.repeat(length - text.length));
      const lengths = [min, max].filter((b) => b !== undefined).flatMap((b) => [b - 1, b, b + 1]).filter((l) => l >= 0);

      // A leading zero on the first number, as in an address part or a version.
      const padded = /\d/.test(text) ? [text.replace(/\d+/, (digits) => `0${digits}`)] : [];

      return [1, '', text.toUpperCase(), text.toLowerCase(), ` ${text}`, `${text} `, `${text}\n`, `${text}/`, ...padded, ...lengths.map(sized)];
    }
    case 'number': {
      const near = numberBounds(def).flatMap((b) => [b - 1, b - 0.5, b, b + 0.5, b + 1]);

      return ['1', typeof base === 'number' ? base + 0.5 : 0.5, ...near];
    }
    case 'boolean':
      return ['true', !base];
    case 'enum':
      return [...schema._zod.values, 'not_an_option', 1];
    case 'literal':
      return [...def.values, 'not_the_literal'];
    default:
      return [];
  }
}

function edges(schema, base, depth) {
  const def = schema._zod.def;

  if (depth > EDGE_DEPTH) {
    return [];
  }

  switch (def.type) {
    case 'object': {
      if (base === null || typeof base !== 'object' || Array.isArray(base)) {
        return [];
      }

      const out = [null, [], { ...base, unexpected_key: 1 }];
      const inner = (field) => {
        while (['optional', 'nullable', 'default', 'prefault'].includes(field._zod.def.type)) field = field._zod.def.innerType;

        return field;
      };

      // One field's value in another of the same type, and one key's value between two objects that
      // share it, for rules that two fields must differ, such as an assister who is not the victim.
      for (const [from, source] of Object.entries(def.shape)) {
        for (const [to, target] of Object.entries(def.shape)) {
          if (from === to || base[from] == null || base[to] == null) continue;

          if (inner(source) === inner(target)) {
            out.push({ ...base, [to]: base[from] });
          } else if (typeof base[from] === 'object' && typeof base[to] === 'object' && !Array.isArray(base[from]) && !Array.isArray(base[to])) {
            for (const key of Object.keys(base[to]).filter((key) => key in base[from])) {
              out.push({ ...base, [to]: { ...base[to], [key]: base[from][key] } });
            }
          }
        }
      }

      for (const [key, field] of Object.entries(def.shape)) {
        const without = { ...base };

        delete without[key];
        out.push(without);

        const current = key in base ? base[key] : validBase(field, 50);

        if (current === undefined) {
          continue;
        }

        out.push({ ...base, [key]: current });

        for (const value of edges(field, current, depth + 1)) {
          out.push(value === OMIT ? without : { ...base, [key]: value });
        }
      }

      return out;
    }
    case 'array': {
      const { max, min } = lengthy(def);
      const element = Array.isArray(base) && base.length > 0 ? base[0] : validBase(def.element, 50);
      const out = [null, {}];

      if (element === undefined) {
        return out;
      }

      const lengths = [0, 1, ...[min, max].filter((b) => b !== undefined).flatMap((b) => [b - 1, b, b + 1])].filter((l) => l >= 0 && l <= 1000);

      out.push(...lengths.map((length) => Array.from({ length }, () => element)));

      const rest = Array.isArray(base) ? base.slice(1) : [];

      for (const value of edges(def.element, element, depth + 1)) {
        if (value !== OMIT) {
          out.push([value, ...rest]);
        }
      }

      return out;
    }
    case 'union': {
      const out = [null];

      for (const option of def.options) {
        const valid = validBase(option, 100);

        if (valid !== undefined) {
          out.push(valid, ...edges(option, valid, depth + 1));
        }
      }

      return out;
    }
    case 'optional':
    case 'default':
    case 'prefault':
      return [OMIT, ...edges(def.innerType, base, depth)];
    case 'nullable':
      return base === null ? [validBase(def.innerType, 50)].filter((v) => v !== undefined) : [null, ...edges(def.innerType, base, depth)];
    case 'pipe':
      return edges(def.in, base, depth);
    case 'intersection':
      return [...edges(def.left, base, depth), ...edges(def.right, base, depth)];
    case 'record':
      return [null, {}, { a: 1 }];
    case 'transform':
    case 'unknown':
      return [];
    default:
      return [null, ...leafEdges(schema, base)];
  }
}

function edgeCases(schema, found) {
  const base = validBase(schema) ?? found;

  if (base === undefined) {
    return undefined;
  }

  const seen = new Set();

  return [base, ...edges(schema, base, 0), ...(COUNT_EDGES.get(schema)?.(base) ?? [])]
    .filter((value) => value !== OMIT)
    .map(strip)
    .filter((value) => {
      const key = JSON.stringify(value);

      return !seen.has(key) && seen.add(key);
    });
}

function corpus(module, out, skip = new Set()) {
  const lines = [];
  const unbased = [];
  const valid = new Map();
  const judge = (name, schema, input) => {
    let parsed;

    // safeParse answers every input with a verdict; one that throws is a contract bug, and skipping
    // its input would leave the Rust side unchecked on exactly that case.
    try {
      parsed = schema.safeParse(input);
    } catch (error) {
      throw new Error(`${name} threw instead of answering ${JSON.stringify(input)}`, { cause: error });
    }

    if (parsed.success && !valid.has(name)) {
      valid.set(name, input);
    }

    const result = parsed.success
      ? { ok: strip(parsed.data) }
      : { issues: parsed.error.issues.map((issue) => ({ code: issue.code, path: issue.path, message: issue.message })) };

    lines.push(JSON.stringify({ schema: name, input, ...result }));
  };

  for (const [name, schema] of Object.entries(module)) {
    if (!schema?._zod || skip.has(schema)) {
      continue;
    }

    for (let index = 0; index < PER_SCHEMA; index++) {
      judge(name, schema, strip(index === 0 ? pick(WRONG) : sample(schema)));
    }
  }

  // After the random cases, and the seed is put back afterwards, so the random cases of this
  // corpus and the next keep the inputs they had before.
  const seeded = seed;

  for (const [name, schema] of Object.entries(module)) {
    if (!schema?._zod || skip.has(schema)) {
      continue;
    }

    const cases = edgeCases(schema, valid.get(name));

    if (cases === undefined) {
      noise = false;
      const attempt = strip(build(schema, 0));
      noise = true;
      const issues = schema.safeParse(attempt).error?.issues ?? [];

      unbased.push(`${name} (${issues.map((issue) => `${issue.path.join('.')}: ${issue.message}`).join('; ')})`);
      continue;
    }

    for (const input of cases) {
      judge(name, schema, input);
    }
  }

  seed = seeded;

  if (unbased.length > 0) {
    console.log(`no valid payload found for edge cases:\n  ${unbased.join('\n  ')}`);
  }

  writeFileSync(out, `${lines.join('\n')}\n`);

  const failed = lines.filter((line) => line.includes('"issues"')).length;

  console.log(`${out.pathname.split('/crates/')[1]}: ${lines.length} cases, ${lines.length - failed} valid, ${failed} invalid`);
}

corpus(protocol, new URL('../crates/protocol/tests/corpus.jsonl', import.meta.url));
corpus(contract, new URL('../crates/agent/tests/contract_corpus.jsonl', import.meta.url), new Set(Object.values(protocol)));
