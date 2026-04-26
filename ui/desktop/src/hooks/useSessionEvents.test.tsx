import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { useSessionEvents, type SessionEvent } from './useSessionEvents';

// Mock the SDK call so we can drive the SSE stream from the test.
vi.mock('../api', () => ({
  sessionEvents: vi.fn(),
}));

import { sessionEvents } from '../api';

const sessionEventsMock = sessionEvents as unknown as ReturnType<typeof vi.fn>;

/**
 * Returns a stream that immediately completes without yielding anything,
 * mirroring a silent connection failure (e.g. the renderer waking up to a
 * dead SSE socket where the server has already dropped the subscriber).
 */
function emptyStream() {
  return {
    stream: (async function* () {
      // no events
    })(),
  };
}

/**
 * Returns a stream that yields the provided events once, in order.
 */
function streamYielding(events: SessionEvent[]) {
  return {
    stream: (async function* () {
      for (const event of events) {
        yield event as unknown as never;
      }
    })(),
  };
}

/**
 * Build a `sessionEvents` mock that returns up to `limit` results from
 * `factory`, then wedges the loop on a never-resolving Promise so the
 * reconnect loop has a deterministic stop point. Without this wedge, with
 * `setTimeout` patched to a microtask the loop would spin indefinitely
 * inside a single tick.
 */
function boundedMock(
  limit: number,
  factory: (callIndex: number) => Promise<{ stream: AsyncGenerator<unknown> }>
) {
  let calls = 0;
  return () => {
    const idx = calls++;
    if (idx >= limit) {
      return new Promise(() => {});
    }
    return factory(idx);
  };
}

/**
 * Repeatedly flush microtasks so the reconnect loop can run as far as the
 * bounded mock allows. Each `Promise.resolve()` await yields back to the
 * scheduler so awaited setTimeouts (stubbed to microtasks below) get a
 * chance to fire.
 */
async function flush(times = 200) {
  for (let i = 0; i < times; i++) {
    await Promise.resolve();
  }
}

// Regression coverage for issue #8717: the reconnect loop in
// `useSessionEvents` used to broadcast a synthetic terminal `Error` event
// to every active listener once `consecutiveErrors` hit
// `MAX_CONSECUTIVE_ERRORS = 10`. That made `useChatStream`'s event
// processor treat the transient disconnect as a fatal stream error and
// surface it to the UI, even though the connection eventually recovered.
describe('useSessionEvents reconnect (issue #8717)', () => {
  let originalSetTimeout: typeof globalThis.setTimeout;

  beforeEach(() => {
    sessionEventsMock.mockReset();
    // Replace the backoff setTimeout with an immediate microtask so the
    // reconnect loop runs as fast as the bounded mock will allow.
    originalSetTimeout = globalThis.setTimeout;
    globalThis.setTimeout = ((cb: () => void) => {
      globalThis.queueMicrotask(cb);
      return 0 as unknown as ReturnType<typeof globalThis.setTimeout>;
    }) as typeof globalThis.setTimeout;
  });

  afterEach(() => {
    globalThis.setTimeout = originalSetTimeout;
  });

  it('does not deliver a synthetic terminal Error event after many consecutive reconnect failures', async () => {
    // 25 silent-failure reconnects — well past MAX_CONSECUTIVE_ERRORS (10) —
    // then the loop wedges so the test can finish.
    sessionEventsMock.mockImplementation(boundedMock(25, () => Promise.resolve(emptyStream())));

    const { result, unmount } = renderHook(() => useSessionEvents('sess-1'));

    const handler = vi.fn();
    act(() => {
      result.current.addListener('req-1', handler);
    });

    await flush();

    expect(sessionEventsMock.mock.calls.length).toBeGreaterThanOrEqual(11);

    // A registered listener must never see a synthetic
    // "Lost connection to server" Error event — that is exactly the
    // terminal-error state issue #8717 reports.
    const errorCalls = handler.mock.calls.filter(
      (args) => (args[0] as SessionEvent).type === 'Error'
    );
    expect(errorCalls).toEqual([]);

    unmount();
  });

  it('keeps reconnecting after a long failure streak and delivers events from a recovered stream', async () => {
    const realEvent: SessionEvent = {
      type: 'Message',
      // Server adds chat_request_id at the SSE framing layer; the hook routes
      // events to listeners by that id.
      chat_request_id: 'req-1',
      request_id: 'req-1',
      // The full Message payload shape isn't needed for routing assertions.
    } as unknown as SessionEvent;

    sessionEventsMock.mockImplementation(
      boundedMock(15, (idx) => {
        if (idx < 12) {
          // First 12 attempts fail silently — past the old MAX_CONSECUTIVE_ERRORS
          // threshold, so the buggy code would have already broadcast a
          // terminal Error to every listener.
          return Promise.resolve(emptyStream());
        }
        // Then the connection recovers and a real event is delivered.
        return Promise.resolve(streamYielding([realEvent]));
      })
    );

    const { result, unmount } = renderHook(() => useSessionEvents('sess-1'));

    const handler = vi.fn();
    act(() => {
      result.current.addListener('req-1', handler);
    });

    await flush();

    expect(sessionEventsMock.mock.calls.length).toBeGreaterThan(12);

    // The recovered stream's real Message event reaches the listener.
    const observedTypes = handler.mock.calls.map((args) => (args[0] as SessionEvent).type);
    expect(observedTypes).toContain('Message');

    // And no synthetic terminal Error event was delivered along the way.
    expect(observedTypes).not.toContain('Error');

    unmount();
  });
});
