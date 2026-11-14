/**
 * Plain checks the schemas and the helpers share. Nothing here imports the rest of the contract.
 */

/** The validation message for a slot id that is not 1 to 32 letters, digits, `_` or `-`. */
export const SLOT_ID_MESSAGE = 'slot id must be 1-32 of [A-Za-z0-9_-] (it becomes part of a container name)';

/** The validation message for a `controller.host` that {@link isControllerHost} rejects. */
export const CONTROLLER_HOST_MESSAGE =
  'must be a host with an optional port and no scheme or path, e.g. cs.example.com, 192.168.0.10:3000 or [fd00::1]';

const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;

/** Whether a value is a dotted-quad IPv4 address. */
export function isIpv4(value: string): boolean {
  return IPV4.test(value);
}

/**
 * The hostname inside a `host[:port]`, without the brackets of an IPv6 literal. Null for a value
 * that is not a host with an optional port.
 */
export function hostnameOf(host: string): string | null {
  // URL parsing normalizes these away, but callers append paths to the original host.
  if (/[\s\p{Cc}/\\?#]/u.test(host)) {
    return null;
  }

  let url: URL;

  try {
    url = new URL(`http://${host}`);
  } catch {
    return null;
  }

  if (url.username !== '' || url.password !== '' || url.search !== '' || url.hash !== '' || url.pathname !== '/') {
    return null;
  }

  return url.hostname === '' ? null : url.hostname.replace(/^\[|\]$/g, '');
}

/**
 * Whether a value is a valid `controller.host`, by the rules that `agent.yml`, its editors and the
 * node-setup script agree on.
 */
export function isControllerHost(value: string): boolean {
  return hostnameOf(value) !== null;
}
