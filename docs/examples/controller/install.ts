import type { Cs2State } from '@ankorcs/protocol';
import type { AgentReport } from '@ankorcs/protocol/session';

export async function install(node: NodeRecord): Promise<void> {
  // #region example
  // Once the node is welcomed. Until CS2 is installed, its `hello` has `cs2.build_id` null.
  const result = await node.session.request({ type: 'update_cs2', mode: 'update' });

  if (result.status === 'ok') {
    // The node's reports about this install carry this id.
    // Your code: keep it with the node, also across a controller restart.
    node.cs2UpdateId = result.data.update_id;
  }
  // #endregion
}

export function onInstallReport(node: NodeRecord, report: AgentReport): void {
  // #region example

  // In handleReport(node, report):
  switch (report.type) {
    case 'cs2_update_progress':
      // `percent` is null when steamcmd reports none, and `message` is its latest line.
      // Your code: show the progress, for example in your UI.
      showProgress(node, report.update_id, report.percent, report.message);

      break;
    case 'node_status':
      onCs2(node, report.cs2);

      break;
  }
  // #endregion
}

// #region example

// Each `node_status` carries the node's full CS2 state, including updates the node starts itself.
// Call this with the `hello`'s `cs2` too, because an install can end while the node is away.
function onCs2(node: NodeRecord, cs2: Cs2State): void {
  // Your code: store the node's CS2 state.
  saveCs2(node, cs2);

  const update = cs2.update;

  // `cs2.update` stays on the last finished update, so act on its outcome only once.
  if (update !== null && update.id === node.cs2UpdateId && update.outcome !== null) {
    node.cs2UpdateId = null;
    // After `completed`, the node takes launches. Any other outcome is a failure, which `detail`
    // explains.
    // Your code: tell whoever started the install how it ended.
    installEnded(node, update.outcome, update.detail);
  }
}
// #endregion
