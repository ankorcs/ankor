import { defineMdastPlugin, type MdastContent, type MdastPluginEntry } from 'satteri';
import { sitePath } from '../site.ts';

/** `[config:demo_upload.limit_mbit]`: a key of `agent.yml`, as the configuration page lists it. */
export const CONFIG_TAG = /\[config:([a-z_]+(?:\[\])?(?:\.[a-z_]+)*)\]/g;

/** The anchor of a key's row on the configuration page; `[]` is dropped to keep it a plain fragment. */
export function configAnchor(key: string): string {
  return `config-${key.replace('[]', '')}`;
}

/**
 * Guides name `agent.yml` keys as tags, which become links to the key's row on the configuration
 * page. The link check fails the build on a key that has no row, as it does on any missing anchor.
 */
export const configTags: MdastPluginEntry = () => defineMdastPlugin({
  name: 'ankor-config-tags',
  text(node, ctx) {
    const parts = node.value.split(CONFIG_TAG);

    if (parts.length === 1) {
      return;
    }

    // split() with a capture group alternates text and key: odd indexes are keys.
    const nodes = parts.flatMap((part, index): MdastContent[] => {
      if (index % 2 === 0) {
        return part === '' ? [] : [{ type: 'text', value: part }];
      }

      return [{ type: 'link', url: sitePath(`/configuration/#${configAnchor(part)}`), children: [{ type: 'inlineCode', value: part }] }];
    });

    ctx.replaceNode(node, nodes);
  },
});
