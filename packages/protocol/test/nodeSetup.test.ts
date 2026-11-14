import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { OFFICIAL_IMAGES } from '../src/values.js';

const script = readFileSync(new URL('../node-setup.sh', import.meta.url), 'utf8');

/** The script's `image_repository`, run by bash on `reference`. */
function imageRepository(reference: string): string {
  const fn = /^image_repository\(\) \{\n[\s\S]*?\n}$/m.exec(script)?.[0];

  expect(fn).toBeDefined();

  return execFileSync('bash', ['-c', `${fn}\nimage_repository "$1"`, 'bash', reference], { encoding: 'utf8' });
}

describe('node-setup.sh', () => {
  it('resolves through the package export, so a controller serves the same file', () => {
    expect(createRequire(import.meta.url).resolve('@ankorcs/protocol/node-setup.sh')).toBe(path.resolve(import.meta.dirname, '../node-setup.sh'));
  });

  it('checks images against OFFICIAL_IMAGES', () => {
    expect(/^readonly OFFICIAL_AGENT_IMAGE=(\S+)$/m.exec(script)?.[1]).toBe(OFFICIAL_IMAGES.agent);
    expect(/^readonly OFFICIAL_CS2_IMAGE=(\S+)$/m.exec(script)?.[1]).toBe(OFFICIAL_IMAGES.cs2);
  });

  it('reads the repository of a reference without its tag or digest', () => {
    const digest = `sha256:${'a'.repeat(64)}`;

    expect(imageRepository(`${OFFICIAL_IMAGES.agent}:0.8`)).toBe(OFFICIAL_IMAGES.agent);
    expect(imageRepository(OFFICIAL_IMAGES.agent)).toBe(OFFICIAL_IMAGES.agent);
    expect(imageRepository(`${OFFICIAL_IMAGES.agent}@${digest}`)).toBe(OFFICIAL_IMAGES.agent);
    expect(imageRepository(`${OFFICIAL_IMAGES.agent}:0.8@${digest}`)).toBe(OFFICIAL_IMAGES.agent);
    expect(imageRepository('localhost:5000/ankor-agent:1')).toBe('localhost:5000/ankor-agent');
    expect(imageRepository(`${OFFICIAL_IMAGES.agent}-x:1`)).toBe(`${OFFICIAL_IMAGES.agent}-x`);
  });
});
