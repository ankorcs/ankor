import type { AgentReport } from '@ankorcs/protocol/session';
// #region example
import { randomUUIDv7 } from 'node:crypto';
import { StartRefusal, type ServerLaunchConfigInput } from '@ankorcs/protocol';
// #endregion

export async function launchMatch(node: NodeRecord): Promise<void> {
  // #region example
  // Every launch gets a new allocation id. Store it before the request goes out.
  const allocationId = randomUUIDv7();

  // Your code: store the launch as unconfirmed.
  saveLaunch(node, allocationId);

  const launch: ServerLaunchConfigInput = {
    map_name: 'de_mirage',
    max_players: 10,
    idle_shutdown_seconds: 300,
    exec: [],
    commands: [],
    match: {
      roster: {
        team1: {
          name: 'Alpha',
          players: [
            '76561198000000001',
            '76561198000000002',
            '76561198000000003',
            '76561198000000004',
            '76561198000000005',
          ],
        },
        team2: {
          name: 'Bravo',
          players: [
            '76561198000000006',
            '76561198000000007',
            '76561198000000008',
            '76561198000000009',
            '76561198000000010',
          ],
        },
        spectators: [],
      },
      sides: 'knife',
      record_demo: true,
      send_statistics: true,
    },
  };

  const result = await node.session.request({ type: 'start_server', allocation_id: allocationId, launch });

  if (result.status === 'error' && StartRefusal.safeParse(result.error.code).success) {
    // Refused: no server started, so try another node.
    // Your code: send the same allocation and launch to the next node.
    launchElsewhere(allocationId, launch);
  } else if (result.status !== 'ok') {
    // Any other error, a timeout or a closed socket may have left a server running.
    // Your code: stop it on this node, or wait for the node's next `hello`, before you launch it
    // elsewhere. Launches in A minimal controller explains why.
    settleLaunch(node, allocationId, result);
  }
  // #endregion
}

export function onLaunchReport(report: AgentReport): void {
  // #region example
  // In handleReport(node, report):
  switch (report.type) {
    case 'slot_status':
      // `slot_status` repeats for later changes to a running slot, such as its SteamID.
      if (report.state === 'running' && report.allocation_id !== null) {
        // CS2 is up. Players join the node's address on this slot's port.
        // Your code: mark the launch running and hand out the address, once per launch.
        markRunning(report.allocation_id, report.slot_id);
      }

      break;
    case 'phase_change':
      // `post_live` means the match is over.
      // Your code: store the match's phase.
      savePhase(report.allocation_id, report.phase);

      break;
    case 'score':
      // The last score of a match is its final score.
      // Your code: store the score.
      saveScore(report.allocation_id, report.team1_score, report.team2_score);

      break;
    case 'server_exited':
      // Your code: record that the server ended, and why. The slot then returns to idle.
      endLaunch(report.allocation_id, report.reason);

      break;
  }
  // #endregion
}
