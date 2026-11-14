/**
 * Terms that guides use without explaining them. The `glossaryTerms` plugin marks the first use of
 * each term in a section of running text, and the reader hovers or focuses it for the definition.
 * A spelling written in capitals, such as `VM`, matches only in capitals; any other matches in any
 * case.
 */
export interface GlossaryEntry {
  /** Every spelling that names the term, plurals included. */
  spellings: string[];
  /** One or two short sentences, as plain text. */
  definition: string;
  /** Words that, right before a spelling, give it another meaning, such as "cgroup controllers". */
  notAfter?: string[];
}

export const GLOSSARY: GlossaryEntry[] = [
  {
    spellings: ['controller', 'controllers'],
    definition: 'The service that agents connect to. It organises matches and decides what runs where. It is typically a web service, such as ankor.sh.',
    notAfter: ['cgroup'],
  },
  {
    spellings: ['agent', 'agents'],
    definition: "The one long-running service on a node. It connects to the controller and runs the node's game servers through Docker.",
  },
  {
    spellings: ['game node', 'game nodes'],
    definition: 'A Linux host that runs an agent and its game servers. A node token identifies it to its controller.',
  },
  {
    spellings: ['slot', 'slots'],
    definition: 'Room on a node for one game server. Each slot has an id, a game port, a GOTV port and a login token.',
  },
  {
    spellings: ['game server', 'game servers'],
    definition: 'The CS2 process that a launch starts in a slot. Each one runs in a container of its own.',
  },
  {
    spellings: ['allocation id', 'allocation ids'],
    definition: 'A lowercase UUID that the controller picks for each launch, preferably a UUIDv7. Messages about the launch and its game server carry it, and the agent refuses a launch that reuses one.',
  },
  {
    spellings: ['match id', 'match ids'],
    definition: "The controller's identifier for a match. It names the match's demos.",
  },
  {
    spellings: ['node token', 'node tokens'],
    definition: 'The secret that identifies a node to its controller. It is stored in agent.yml.',
  },
  {
    spellings: ['login token', 'login tokens', 'GSLT', 'GSLTs'],
    definition: 'A Game Server Login Token from Steam. A game server needs one to log on to Steam, and each slot has its own.',
  },
  {
    spellings: ['game port', 'game ports'],
    definition: "The UDP port that players connect to. Each slot has its own.",
  },
  {
    spellings: ['GOTV port', 'GOTV ports'],
    definition: 'The UDP port that spectators connect to for GOTV. Each slot has its own.',
  },
  {
    spellings: ['GOTV'],
    definition: "CS2's broadcast of a match to spectators, sent with a delay.",
  },
  {
    spellings: ['CS2 image'],
    definition: 'The ankor-cs2 image. It carries Ankor\'s plugin, SwiftlyS2 and the server configuration.',
  },
  {
    spellings: ['SwiftlyS2'],
    definition: "The plugin framework for CS2 servers that Ankor's plugin runs on.",
  },
  {
    spellings: ['hash', 'hashes'],
    definition: 'A short, fixed-length fingerprint computed from data. The same input always gives the same hash, and the input cannot be worked out from it.',
  },
  {
    spellings: ['SHA-256'],
    definition: 'A widely used hash function. Its hash is 256 bits, usually written as 64 hexadecimal characters.',
  },
  {
    spellings: ['Docker'],
    definition: 'The container runtime that runs the agent and every game server on a node.',
  },
  {
    spellings: ['VM', 'VMs'],
    definition: 'A virtual machine: a whole computer that a host emulates in software.',
  },
  {
    spellings: ['LXC'],
    definition: "Linux containers, which share the host's kernel. Docker inside one needs nesting turned on.",
  },
  {
    spellings: ['Proxmox'],
    definition: 'Proxmox VE, a server platform that runs virtual machines and LXC containers.',
  },
  {
    spellings: ['package manager', 'package managers'],
    definition: 'The tool that installs software on a Linux distribution, such as apt, dnf or pacman.',
  },
];
