import { fileURLToPath } from 'node:url';
import { defineMdastPlugin, type MdastContent, type MdastPluginEntry } from 'satteri';
import { loadReference } from '../reference/model.ts';
import { protocolFile } from './plugins.ts';

/** `[agent:hello]`, `[event:map_end]`, `[upload:event_batch]` or `[plugin:quitting]`: a frame of the protocol reference. */
export const FRAME_TAG = /\[((?:agent|event|upload|plugin):[a-z0-9_]+)\]/g;

const frameHrefs = protocolFile('asyncapi.yaml', (text) => loadReference(text).frames);

/**
 * Guides name frames as tags, which become links to the frame in the reference. A tag no compiled
 * frame answers to fails the build, so a renamed or removed frame cannot leave a dead mention.
 */
export const frameTags: MdastPluginEntry = ({ fileURL }) => {
  const hrefOf = (tag: string): string => {
    const href = frameHrefs().get(tag);

    if (href === undefined) {
      throw new Error(`${fileURL === undefined ? 'a guide' : fileURLToPath(fileURL)}: no frame ${tag} in the protocol reference`);
    }

    return href;
  };

  return defineMdastPlugin({
    name: 'ankor-frame-tags',
    // Components written as HTML, such as the message cards, name their frames as tags too.
    html(node, ctx) {
      if (!node.value.match(FRAME_TAG)) {
        return;
      }

      const value = node.value.replace(FRAME_TAG, (_, tag: string) => `<a href="${hrefOf(tag)}"><code>${tag.slice(tag.indexOf(':') + 1)}</code></a>`);

      ctx.replaceNode(node, [{ type: 'html', value }]);
    },
    text(node, ctx) {
      const parts = node.value.split(FRAME_TAG);

      if (parts.length === 1) {
        return;
      }

      // split() with a capture group alternates text and tag: odd indexes are tags.
      const nodes = parts.flatMap((part, index): MdastContent[] => {
        if (index % 2 === 0) {
          return part === '' ? [] : [{ type: 'text', value: part }];
        }

        return [{ type: 'link', url: hrefOf(part), children: [{ type: 'inlineCode', value: part.slice(part.indexOf(':') + 1) }] }];
      });

      ctx.replaceNode(node, nodes);
    },
  });
};
