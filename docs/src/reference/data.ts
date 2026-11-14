/** The reference as built from the AsyncAPI document that `@ankorcs/protocol` ships. */
import yaml from '@ankorcs/protocol/asyncapi.yaml?raw';
import { loadReference } from './model.ts';

export const reference = loadReference(yaml);
