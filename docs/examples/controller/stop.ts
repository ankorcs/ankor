// #region example
async function stopServer(node: NodeRecord, allocationId: string): Promise<void> {
  // Your code: mark the launch as stopping, so nothing else acts on it.
  markStopping(allocationId);

  // The session waits as long as the stop can take, including Docker killing a server that does
  // not shut down in time.
  const result = await node.session.request({ type: 'stop_server', allocation_id: allocationId });

  if (result.status === 'ok') {
    // The container has stopped. The `server_exited` before this reply already recorded the end and
    // its reason.
    return;
  }

  if (result.status === 'error' && result.error.code === 'unknown_allocation') {
    // No server runs under this allocation; it has already ended.
    // Your code: record the end, unless a `server_exited` already did.
    endLaunch(allocationId, null);

    return;
  }

  // `failed`, a timeout or a closed socket: the server may still be running.
  // Your code: keep it as running, and stop it again later or once the node reconnects.
  retryStop(node, allocationId);
}
// #endregion

export { stopServer };
