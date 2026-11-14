# Roadmap

Planned work that needs changes to the agent, the plugin or the contract.

## Matches

- **Technical pauses.** A player who disconnects mid-match can rejoin, but the match plays on
  without them. The plugin is to pause the match automatically.
- **`sv_pure`.** Game servers do not set it yet.

## Nodes

- **Console command `echo`.** The plugin runs an `echo` after each `console_command`
  ([AgentControl.cs](packages/plugin/core/AgentControl.cs)). Without it, output that ends without a
  newline, such as the closing brace of `status_json`, shows only with the next line. The agent's
  line reader also holds a line until its newline, so it is unclear whether CS2 holds it too. Running
  `strace -e trace=write` on the `cs2` process settles it. If CS2 writes the `}` on its own, the
  agent is to pass on an unterminated line after a moment, and the `echo` can go.
- **Paced image pulls.** Pulling a new CS2 or agent image can fill the node's connection while a
  game runs. CS2's files are not a concern, because the agent updates them only on an idle node.
  Docker downloads an image itself, so the agent never sees the bytes. The agent, which runs on the
  host network, is to shape the pull itself while a game runs:
  - An `iptables` rule marks the connections of the process that downloads, by its cgroup. That is
    `dockerd` or, with the containerd image store, possibly `containerd`.
  - A `tc` filter on the interface's ingress restores the mark and polices marked traffic to a
    limit from `agent.yml`, as the demo upload's `limit_mbit` limits uploads.
  - The agent removes the rules when the pull ends, and clears leftovers when it starts.

  The agent container needs `--cap-add NET_ADMIN` and `--cgroupns host`, and its image `iptables`
  and `iproute2`. `node-setup.sh` sets both for new nodes, and a self-update adds them to the
  container it creates. Which process downloads and its cgroup path are to be checked on a node
  first.

  The kernel modules (`xt_cgroup`, `xt_connmark`, `act_connmark`) are the host's, and a container
  cannot load its own. Stock kernels load them on first use, but container-based VPS and custom
  kernels can lack them. So pacing is best-effort: the agent tries a test rule when it starts,
  pulls unpaced and logs why when that fails, and reports in its `hello` whether it can pace.
- **Public IP lookup.** A node asks `ipinfo.io` for its address and country. The lookup service is
  to be configurable.
