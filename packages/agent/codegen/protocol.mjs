// The contract codegen reads: the built @ankorcs/protocol that ANKOR_PROTOCOL_DIR names. The
// scripts build it from packages/protocol (scripts/lib/protocol.sh) and set the variable, so the
// generated Rust always follows the contract in the same checkout. zod is the protocol's own copy,
// because codegen reads its internals.

import { createRequire } from 'node:module';
import path from 'node:path';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

const dir = process.env.ANKOR_PROTOCOL_DIR;

if (dir === undefined || dir === '') {
  throw new Error('ANKOR_PROTOCOL_DIR names no built @ankorcs/protocol; run scripts/protocol-sync.sh');
}

const load = (file) => import(pathToFileURL(path.join(dir, file)).href);

export const protocol = await load('dist/index.js');
export const container = await load('dist/container.js');
export const { z } = await import(pathToFileURL(createRequire(path.join(dir, 'package.json')).resolve('zod')).href);
