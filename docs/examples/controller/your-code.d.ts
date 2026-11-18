// The controller's own code that the examples call. Each call in an example carries a comment
// starting "Your code" that says what the function should do; these declarations only give the
// examples types, so that `astro check` holds them to the protocol package.
import type { IncomingMessage, Server } from 'node:http';
import type {
  AgentHello,
  Cs2State,
  Cs2UpdateOutcome,
  EventBatchFrame,
  EventFrame,
  MatchPhase,
  NodeSettings,
  ServerExitReason,
  ServerLaunchConfigInput,
} from '@ankorcs/protocol';
import type { ProtocolVersion } from '@ankorcs/protocol/helpers';
import type { AgentReport, AgentSession, RequestResult } from '@ankorcs/protocol/session';
import type { WebSocket, WebSocketServer } from 'ws';

declare global {
  /**
   * A node as the controller keeps it in memory: the same object every time the node is looked up,
   * because it holds the node's live socket and session.
   */
  interface NodeRecord {
    id: string;
    protocol: ProtocolVersion | null;
    session: AgentSession;
    ws?: WebSocket | undefined;
    cs2UpdateId: string | null;
  }

  /** One server's events and cursor, locked for the transaction that handles its batch. */
  interface LockedLaunch {
    /** The highest `seq` handled so far, and -1 before the server's first event. */
    cursor: number;
    matchesStored(frame: EventFrame): Promise<boolean>;
    store(frame: EventBatchFrame): Promise<void>;
    quarantine(seq: number, reason: string): Promise<void>;
    saveCursor(seq: number): Promise<void>;
  }

  /** The storage transaction that one event batch runs in. */
  interface Transaction {
    lockLaunch(node: NodeRecord, allocationId: string): Promise<LockedLaunch | null>;
  }

  const server: Server;
  const sockets: WebSocketServer;
  const log: { info(text: string): void; warn(text: string): void };

  function findNodeByTokenHash(hash: string): NodeRecord | null;
  function run(node: NodeRecord, ws: WebSocket): void;
  function saveHello(node: NodeRecord, hello: AgentHello): void;
  function handleReport(node: NodeRecord, report: AgentReport): void;
  function saveSettings(node: NodeRecord, settings: NodeSettings): void;
  function showProgress(node: NodeRecord, updateId: string, percent: number | null, message: string): void;
  function saveCs2(node: NodeRecord, cs2: Cs2State): void;
  function installEnded(node: NodeRecord, outcome: Cs2UpdateOutcome, detail: string | null): void;
  function saveLaunch(node: NodeRecord, allocationId: string): void;
  function launchElsewhere(allocationId: string, launch: ServerLaunchConfigInput): void;
  function settleLaunch(node: NodeRecord, allocationId: string, result: RequestResult<'start_server'>): void;
  function markRunning(allocationId: string, slotId: string): void;
  function savePhase(allocationId: string, phase: MatchPhase): void;
  function saveScore(allocationId: string, team1: number, team2: number): void;
  /** Records an end once per allocation. A later call with a reason fills in one that was missing. */
  function endLaunch(allocationId: string, reason: ServerExitReason | null): void;
  function openLaunches(node: NodeRecord): string[];
  function hasLaunch(node: NodeRecord, allocationId: string): boolean;
  function markStopping(allocationId: string): void;
  function retryStop(node: NodeRecord, allocationId: string): void;
  function readJson(request: IncomingMessage): Promise<unknown>;
  function inTransaction<T>(work: (tx: Transaction) => Promise<T>): Promise<T>;
  function namesOnlyRoster(launch: LockedLaunch, frame: EventBatchFrame): boolean;
}

export {};
