/**
 * The session over a fake socket: `send` collects what the controller writes, and `receive` is handed what
 * the agent would answer.
 */
import { STOP_REPLY_MARGIN_SECONDS, STOP_TIMEOUT_SECONDS, type NodeSettings, type ReplyData } from '../src/index.js';
import { afterEach, describe, expect, expectTypeOf, it, vi } from 'vitest';
import { createAgentSession, DEFAULT_REQUEST_TIMEOUT_MS, replyWait, type AgentSessionOptions, type SessionRequest } from '../src/session.js';

const ALLOCATION = '5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e';

const SETTINGS = {
  slots: [{ id: 'a', port: 27015, tv_port: 27020, gslt_hint: 'AAAAAA0001' }],
  pinned_ip: null,
  controller: { host: 'cs.example.com', tls: true, socket_path: '/ws/agent', events_path: '/api/node/events' },
  auto_update: true,
  demo_upload: null,
  memory_mb: null,
} satisfies NodeSettings;

afterEach(() => {
  vi.useRealTimers();
});

function setup(options: Omit<AgentSessionOptions, 'send'> = {}) {
  const sent: Record<string, unknown>[] = [];
  const session = createAgentSession({ ...options, send: (text) => void sent.push(JSON.parse(text) as Record<string, unknown>) });

  /** The id of the last request the controller sent. */
  const lastId = (): string => {
    const id = sent.at(-1)?.['id'];

    if (typeof id !== 'string') {
      throw new Error('no request was sent');
    }

    return id;
  };

  const reply = (frame: Record<string, unknown>) => session.receive(JSON.stringify({ type: 'reply', reply_to: lastId(), ...frame }));

  return { session, sent, lastId, reply };
}

describe('request', () => {
  it('sends the request under an id of its own and resolves with the reply\'s data', async () => {
    const { session, sent, reply } = setup();
    const result = session.request({ type: 'stop_server', allocation_id: ALLOCATION });

    expect(sent).toEqual([{ type: 'stop_server', id: expect.any(String), allocation_id: ALLOCATION }]);
    expect(reply({ request: 'stop_server', ok: true, data: { allocation_id: ALLOCATION } })).toMatchObject({ kind: 'reply', answered: true });
    await expect(result).resolves.toEqual({ status: 'ok', data: { allocation_id: ALLOCATION } });
  });

  it('gives each request its own id and settles each by its own reply, in any order', async () => {
    const { session, sent } = setup();
    const first = session.request({ type: 'reserve_slot', slot_id: 'a' });
    const second = session.request({ type: 'reserve_slot', slot_id: 'b' });
    const [firstId, secondId] = sent.map((frame) => frame['id']);

    expect(firstId).not.toBe(secondId);
    session.receive(JSON.stringify({ type: 'reply', reply_to: secondId, request: 'reserve_slot', ok: true, data: { slot_id: 'b' } }));
    session.receive(JSON.stringify({ type: 'reply', reply_to: firstId, request: 'reserve_slot', ok: true, data: { slot_id: 'a' } }));
    await expect(first).resolves.toEqual({ status: 'ok', data: { slot_id: 'a' } });
    await expect(second).resolves.toEqual({ status: 'ok', data: { slot_id: 'b' } });
  });

  it('resolves with the agent\'s error, also one whose request the agent did not know', async () => {
    const { session, reply } = setup();
    const refused = session.request({ type: 'reserve_slot', slot_id: 'a' });

    reply({ request: 'reserve_slot', ok: false, error: { code: 'slot_held', detail: 'slot a is held' } });
    await expect(refused).resolves.toEqual({ status: 'error', error: { code: 'slot_held', detail: 'slot a is held' } });

    const unknown = session.request({ type: 'reserve_slot', slot_id: 'a' });

    reply({ request: null, ok: false, error: { code: 'unsupported_request', detail: 'unsupported request type reserve_slot' } });
    await expect(unknown).resolves.toMatchObject({ status: 'error', error: { code: 'unsupported_request' } });
  });

  it('types a reply\'s data by the request it answers', () => {
    const { session } = setup();
    const settings = session.request({ type: 'set_config', config: { pinned_ip: null } });
    const relayed = session.request({ type: 'reserve_slot', slot_id: 'a' } as SessionRequest);

    expectTypeOf(settings).resolves.extract<{ status: 'ok' }>().toHaveProperty('data').toEqualTypeOf<NodeSettings>();
    expectTypeOf(relayed).resolves.extract<{ status: 'ok' }>().toHaveProperty('data').toEqualTypeOf<ReplyData<SessionRequest['type']>>();
    session.closed();
  });

  it('takes a request built elsewhere whole, whatever its type', async () => {
    const { session, sent, reply } = setup();
    // As a request arrives from another service: one of every request, typed only as that.
    const relayed: SessionRequest = JSON.parse(JSON.stringify({ type: 'reserve_slot', slot_id: 'a' })) as SessionRequest;
    const result = session.request(relayed);

    expect(sent).toEqual([{ type: 'reserve_slot', slot_id: 'a', id: expect.any(String) }]);
    reply({ request: 'reserve_slot', ok: true, data: { slot_id: 'a' } });
    await expect(result).resolves.toEqual({ status: 'ok', data: { slot_id: 'a' } });
  });

  it('refuses a request the contract refuses, without sending it', async () => {
    const { session, sent } = setup();

    await expect(session.request({ type: 'stop_server', allocation_id: 'not-a-uuid' })).rejects.toThrow();
    expect(sent).toEqual([]);
  });

  it('leaves nothing open when the socket will not take the frame', async () => {
    const failure = new Error('socket closed');
    const session = createAgentSession({ send: () => {
      throw failure;
    } });

    await expect(session.request({ type: 'reserve_slot', slot_id: 'a' })).rejects.toBe(failure);
    // Nothing waits on it, so no reply finds it.
    expect(session.receive(JSON.stringify({ type: 'reply', reply_to: 'any', request: 'reserve_slot', ok: true, data: { slot_id: 'a' } }))).toMatchObject({ kind: 'reply', answered: false });
  });
});

describe('timeouts', () => {
  it('ends a request unanswered after the session\'s wait, and hands back its late reply whole', async () => {
    vi.useFakeTimers();
    const { session, reply, lastId } = setup();
    const result = session.request({ type: 'set_config', config: { pinned_ip: null } });

    vi.advanceTimersByTime(DEFAULT_REQUEST_TIMEOUT_MS);
    await expect(result).resolves.toEqual({ status: 'timeout' });

    const late = { request: 'set_config', ok: true, data: SETTINGS };

    expect(reply(late)).toEqual({ kind: 'reply', answered: false, message: { type: 'reply', reply_to: lastId(), ...late } });
  });

  it('waits a stop by its grace period and the reply margin, not the session\'s wait', async () => {
    vi.useFakeTimers();
    const { session } = setup({ timeoutMs: 1_000 });
    let settled = false;
    const stop = session.request({ type: 'stop_server', allocation_id: ALLOCATION }).finally(() => {
      settled = true;
    });

    await vi.advanceTimersByTimeAsync((STOP_TIMEOUT_SECONDS + STOP_REPLY_MARGIN_SECONDS) * 1000 - 1);
    expect(settled).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await expect(stop).resolves.toEqual({ status: 'timeout' });
  });

  it('waits a stop by its grace period whatever wait the call sets', async () => {
    vi.useFakeTimers();
    const { session } = setup({ timeoutMs: 1_000 });
    let settled = false;
    const stop = session.request({ type: 'stop_server', allocation_id: ALLOCATION }, { timeoutMs: null }).finally(() => {
      settled = true;
    });

    await vi.advanceTimersByTimeAsync((STOP_TIMEOUT_SECONDS + STOP_REPLY_MARGIN_SECONDS) * 1000 - 1);
    expect(settled).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await expect(stop).resolves.toEqual({ status: 'timeout' });
  });

  it('waits a stop its grace and the margin; any other request the wait it is given', () => {
    expect(replyWait({ type: 'stop_server' }, 60_000)).toBe((STOP_TIMEOUT_SECONDS + STOP_REPLY_MARGIN_SECONDS) * 1000);
    expect(replyWait({ type: 'stop_server' }, null)).toBe((STOP_TIMEOUT_SECONDS + STOP_REPLY_MARGIN_SECONDS) * 1000);
    expect(replyWait({ type: 'reserve_slot' }, 60_000)).toBe(60_000);
    expect(replyWait({ type: 'reserve_slot' }, null)).toBeNull();
  });

  it('takes a wait from the call over the session\'s, and null as no deadline', async () => {
    vi.useFakeTimers();
    const { session, reply } = setup({ timeoutMs: 1_000 });
    const quick = session.request({ type: 'reserve_slot', slot_id: 'a' });

    vi.advanceTimersByTime(1_000);
    await expect(quick).resolves.toEqual({ status: 'timeout' });

    const slow = session.request({ type: 'reserve_slot', slot_id: 'b' }, { timeoutMs: null });

    vi.advanceTimersByTime(10 * 60_000);
    reply({ request: 'reserve_slot', ok: true, data: { slot_id: 'b' } });
    await expect(slow).resolves.toEqual({ status: 'ok', data: { slot_id: 'b' } });
  });
});

describe('closed', () => {
  it('ends every open request closed, and a later one at once without sending it', async () => {
    const { session, sent } = setup();
    const open = session.request({ type: 'reserve_slot', slot_id: 'a' });

    session.closed();
    await expect(open).resolves.toEqual({ status: 'closed' });
    await expect(session.request({ type: 'reserve_slot', slot_id: 'b' })).resolves.toEqual({ status: 'closed' });
    expect(sent).toHaveLength(1);
  });
});

describe('shutdown', () => {
  it('lets open requests settle by their replies, and answers a later one shutting_down without sending it', async () => {
    const { session, sent, reply } = setup();
    const open = session.request({ type: 'reserve_slot', slot_id: 'a' });

    session.receive(JSON.stringify({ type: 'shutdown', signal: 'SIGTERM' }));
    reply({ request: 'reserve_slot', ok: true, data: { slot_id: 'a' } });
    await expect(open).resolves.toEqual({ status: 'ok', data: { slot_id: 'a' } });
    await expect(session.request({ type: 'reserve_slot', slot_id: 'b' })).resolves.toEqual({
      status: 'error',
      error: { code: 'shutting_down', detail: 'the agent is shutting down' },
    });
    expect(sent).toHaveLength(1);
  });
});

describe('receive', () => {
  it('hands back a report parsed, from text or bytes', () => {
    const { session } = setup();
    const frame = { type: 'shutdown', signal: 'SIGTERM' };

    expect(session.receive(JSON.stringify(frame))).toEqual({ kind: 'report', message: frame });
    expect(session.receive(new TextEncoder().encode(JSON.stringify(frame)))).toEqual({ kind: 'report', message: frame });
  });

  it('reads a frame the controller already parsed as it reads text', () => {
    const { session } = setup();

    expect(session.receiveParsed({ type: 'shutdown', signal: 'SIGTERM' })).toEqual({ kind: 'report', message: { type: 'shutdown', signal: 'SIGTERM' } });
    expect(session.receiveParsed('{"type":"shutdown","signal":"SIGTERM"}')).toMatchObject({ kind: 'invalid' });
  });

  it('calls a frame invalid when it is not JSON or fails the contract, with zod\'s issues', () => {
    const { session } = setup();

    expect(session.receive('{')).toEqual({ kind: 'invalid', detail: 'not JSON', issues: [] });

    const refused = session.receive(JSON.stringify({ type: 'shutdown', signal: 'SIGHUP' }));

    expect(refused).toMatchObject({ kind: 'invalid', detail: expect.stringContaining('signal') });
    expect(refused.kind === 'invalid' && refused.issues.map((issue) => issue.path)).toEqual([['signal']]);
  });

  it('skips a message a newer agent sends that this version does not have, and ignores keys it does not know', () => {
    const { session } = setup();

    expect(session.receiveParsed({ type: 'node_weather', sky: 'clear' })).toEqual({ kind: 'unsupported', type: 'node_weather' });
    expect(session.receiveParsed({ type: 'shutdown', signal: 'SIGTERM', added_later: 1 })).toEqual({
      kind: 'report',
      message: { type: 'shutdown', signal: 'SIGTERM' },
    });
  });

  it('calls a reply invalid when it names another request than the one its id answers, and leaves that request open', async () => {
    const { session, reply } = setup();
    const result = session.request({ type: 'reserve_slot', slot_id: 'a' });

    expect(reply({ request: 'delete_demo', ok: true, data: { slot_id: 'a', file_name: 'x.dem' } })).toMatchObject({ kind: 'invalid', issues: [{ path: ['request'] }] });
    reply({ request: 'reserve_slot', ok: true, data: { slot_id: 'a' } });
    await expect(result).resolves.toEqual({ status: 'ok', data: { slot_id: 'a' } });
  });
});

describe('notify', () => {
  it('sends a frame that gets no reply, and refuses one the contract refuses', () => {
    const { session, sent } = setup();

    session.notify({ type: 'welcome' });
    expect(sent).toEqual([{ type: 'welcome' }]);
    expect(() => session.notify({ type: 'watch_resources', replay: 'yes' as unknown as boolean })).toThrow();
    expect(sent).toHaveLength(1);
  });
});
