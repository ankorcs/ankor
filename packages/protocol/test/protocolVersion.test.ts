/**
 * The wire version is the package's own `major.minor`, and every copy of it says the same.
 */
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { protocolVersionOf, isCompatibleProtocol, unknownMessageType } from '../src/helpers.js';
import { ControllerToAgentMessage, PluginToControllerMessage } from '../src/index.js';
import { PROTOCOL_VERSION } from '../src/paths.js';

const own = protocolVersionOf(PROTOCOL_VERSION)!;

describe('PROTOCOL_VERSION', () => {
  it('is the major and minor of the package version', () => {
    const { version } = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')) as { version: string };

    expect(PROTOCOL_VERSION).toBe(version.split('.').slice(0, 2).join('.'));
  });

  it('is what node-setup.sh sends', () => {
    const script = readFileSync(new URL('../node-setup.sh', import.meta.url), 'utf8');

    expect(/^readonly PROTOCOL_VERSION=(\S+)$/m.exec(script)?.[1]).toBe(PROTOCOL_VERSION);
  });
});

describe('isCompatibleProtocol', () => {
  it('accepts its own version and refuses anything that is not major.minor', () => {
    expect(isCompatibleProtocol(PROTOCOL_VERSION)).toBe(true);

    for (const header of [undefined, '', '1', '0.5.0', 'v0.5', '00.5', ' 0.5', ['0.5']]) {
      expect(isCompatibleProtocol(header)).toBe(false);
    }
  });

  it('refuses another major', () => {
    expect(isCompatibleProtocol(`${own.major + 1}.${own.minor}`)).toBe(false);
  });

  it('accepts another minor of its major, 0.x included', () => {
    expect(own.major).toBe(0);
    expect(isCompatibleProtocol(`0.${own.minor + 1}`)).toBe(true);
    expect(isCompatibleProtocol(`0.${own.minor - 1}`)).toBe(true);
  });

  it('reads the version a peer sent', () => {
    expect(protocolVersionOf('1.12')).toEqual({ major: 1, minor: 12 });
    expect(protocolVersionOf('1.x')).toBeNull();
  });
});

describe('unknownMessageType', () => {
  it('names a type the union does not have, so a receiver skips the frame', () => {
    expect(unknownMessageType(PluginToControllerMessage, { type: 'bomb_juggled', tick: 1 })).toBe('bomb_juggled');
    expect(unknownMessageType(ControllerToAgentMessage, { type: 'paint_node', id: 'r1' })).toBe('paint_node');
  });

  it('leaves a known type, or a frame with no string type, to the schema', () => {
    expect(unknownMessageType(PluginToControllerMessage, { type: 'round_start' })).toBeNull();
    expect(unknownMessageType(ControllerToAgentMessage, { type: 'welcome' })).toBeNull();
    expect(unknownMessageType(PluginToControllerMessage, { type: 7 })).toBeNull();
    expect(unknownMessageType(PluginToControllerMessage, null)).toBeNull();
  });
});
