import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { renderAgentYml } from '../src/agent-yml.js';
import { OFFICIAL_IMAGES } from '../src/values.js';

const script = readFileSync(new URL('../node-setup.sh', import.meta.url), 'utf8');

/** The script's `image_repository`, run by bash on `reference`. */
function imageRepository(reference: string): string {
  const fn = /^image_repository\(\) \{\n[\s\S]*?\n}$/m.exec(script)?.[0];

  expect(fn).toBeDefined();

  return execFileSync('bash', ['-c', `${fn}\nimage_repository "$1"`, 'bash', reference], { encoding: 'utf8' });
}

/**
 * The script's `yaml_scalar`, run by bash on `key` in a file that holds `yaml`. The file is real,
 * because Node hands a child's input over a socket on Linux, which `/dev/stdin` cannot open.
 */
function yamlScalar(key: string, yaml: string): string {
  const fn = /^yaml_scalar\(\) \{\n[\s\S]*?\n}$/m.exec(script)?.[0];

  expect(fn).toBeDefined();

  const dir = mkdtempSync(path.join(tmpdir(), 'agent-yml-'));
  const file = path.join(dir, 'agent.yml');

  try {
    writeFileSync(file, yaml);

    return execFileSync('bash', ['-c', `${fn}\nyaml_scalar "$1" "$2"`, 'bash', key, file], { encoding: 'utf8' });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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

  it('reads the images back out of the agent.yml that renderAgentYml writes', () => {
    const digest = `sha256:${'a'.repeat(64)}`;
    const images = [
      { cs2: `${OFFICIAL_IMAGES.cs2}:0.10`, agent: `${OFFICIAL_IMAGES.agent}:0.10` },
      { cs2: `${OFFICIAL_IMAGES.cs2}:latest`, agent: `${OFFICIAL_IMAGES.agent}:latest` },
      { cs2: `${OFFICIAL_IMAGES.cs2}@${digest}`, agent: `${OFFICIAL_IMAGES.agent}:0.10@${digest}` }
    ];

    for (const { cs2, agent } of images) {
      const yaml = renderAgentYml({
        token: 'a'.repeat(32),
        controller: { host: 'controller.test', tls: true },
        cs2_image: cs2,
        agent_image: agent,
        slots: [{ id: 's1', port: 27015, tv_port: 27020, gslt: 'B'.repeat(32) }]
      });

      expect(yamlScalar('cs2_image', yaml)).toBe(cs2);
      expect(yamlScalar('agent_image', yaml)).toBe(agent);
      expect(yamlScalar('official_images_only', yaml)).toBe('');
    }
  });

  it('reads no image from a flow-style agent.yml, which is why the layout is part of the contract', () => {
    const yaml = JSON.stringify({ cs2_image: `${OFFICIAL_IMAGES.cs2}:0.10`, agent_image: `${OFFICIAL_IMAGES.agent}:0.10` });

    expect(yamlScalar('cs2_image', yaml)).toBe('');
    expect(yamlScalar('agent_image', yaml)).toBe('');
  });
});
