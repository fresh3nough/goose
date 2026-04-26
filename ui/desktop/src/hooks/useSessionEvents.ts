import { useEffect, useRef, useState, useCallback } from 'react';
import { sessionEvents, type MessageEvent } from '../api';

/**
 * An SSE event with an optional request_id (added by the server at the
 * SSE framing layer, not part of the generated MessageEvent type).
 */
export type SessionEvent = MessageEvent & {
  request_id?: string;
  /** Chat-level request UUID used for routing events to the correct handler. */
  chat_request_id?: string;
};

type EventHandler = (event: SessionEvent) => void;
type ActiveRequestsHandler = (requestIds: string[]) => void;

export function useSessionEvents(sessionId: string) {
  const listenersRef = useRef(new Map<string, Set<EventHandler>>());
  const activeRequestsHandlerRef = useRef<ActiveRequestsHandler | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    if (!sessionId) return;

    const abortController = new AbortController();
    abortRef.current = abortController;

    (async () => {
      // The reconnect loop runs for the lifetime of the hook and never
      // surfaces a terminal "Error" event to listeners on its own. Transient
      // disconnects (App Nap, sleep/wake, network blips) recover via the
      // SSE `Last-Event-ID` replay protocol; the only fatal case — the
      // server's replay buffer overflowing — is signalled by a real `Error`
      // event from goosed and handled by the chat-stream consumer.
      let retryDelay = 500;
      const MAX_RETRY_DELAY = 10_000;
      let lastEventId: string | undefined;

      while (!abortController.signal.aborted) {
        try {
          const { stream } = await sessionEvents({
            path: { id: sessionId },
            signal: abortController.signal,
            headers: lastEventId ? { 'Last-Event-ID': lastEventId } : undefined,
            // Disable the inner retry loop so errors surface to our outer
            // loop, which handles backoff.
            sseMaxRetryAttempts: 1,
            onSseEvent: (event) => {
              if (event.id) {
                lastEventId = event.id;
              }
            },
          });

          let receivedEvent = false;

          for await (const event of stream) {
            if (abortController.signal.aborted) break;

            // Only mark as connected after the first real event arrives,
            // since the HTTP request doesn't happen until iteration starts.
            if (!receivedEvent) {
              receivedEvent = true;
              setConnected(true);
              retryDelay = 500;
            }

            // The server adds chat_request_id (the chat UUID) and request_id
            // to the JSON at the SSE framing layer. Route using chat_request_id
            // so that Notification events (which carry their own MCP tool-call
            // request_id) still reach the correct handler.
            const sessionEvent = event as SessionEvent;
            const routingId = sessionEvent.chat_request_id ?? sessionEvent.request_id;

            // ActiveRequests events notify the client about in-flight requests
            // it can reattach to (e.g. after a remount).
            if (sessionEvent.type === 'ActiveRequests') {
              const ids = (sessionEvent as unknown as { request_ids: string[] }).request_ids;
              activeRequestsHandlerRef.current?.(ids);
              continue;
            }

            // Server-level errors without a request ID (e.g. "client too far
            // behind") affect all active listeners — broadcast to everyone.
            if (!routingId && sessionEvent.type === 'Error') {
              for (const [id, handlers] of listenersRef.current) {
                for (const handler of handlers) {
                  handler({ ...sessionEvent, request_id: id, chat_request_id: id });
                }
              }
            } else if (routingId) {
              const handlers = listenersRef.current.get(routingId);
              if (handlers) {
                for (const handler of handlers) {
                  handler(sessionEvent);
                }
              }
            }
          }

          // Stream ended. Reconnect unless we were intentionally aborted.
          if (abortController.signal.aborted) break;
          setConnected(false);

          // If the stream ended without delivering any events, the connection
          // likely failed silently (e.g. 404 with sseMaxRetryAttempts: 1).
          // Back off and reconnect; do NOT synthesize a terminal Error — doing
          // so used to make any in-flight chat session fail permanently after
          // ~1 minute of disconnect (issue #8717).
          if (!receivedEvent) {
            await new Promise((r) => setTimeout(r, retryDelay));
            retryDelay = Math.min(retryDelay * 2, MAX_RETRY_DELAY);
          }
        } catch (error) {
          if (abortController.signal.aborted) break;
          console.warn('SSE connection error, reconnecting:', error);
          setConnected(false);

          // Back off before retrying. We deliberately keep reconnecting
          // forever rather than giving up, so that idle windows transparently
          // resume their SSE stream when the renderer wakes from suspension.
          await new Promise((r) => setTimeout(r, retryDelay));
          retryDelay = Math.min(retryDelay * 2, MAX_RETRY_DELAY);
        }
      }

      setConnected(false);
    })();

    const listeners = listenersRef.current;
    return () => {
      abortController.abort();
      abortRef.current = null;
      listeners.clear();
      setConnected(false);
    };
  }, [sessionId]);

  const addListener = useCallback((requestId: string, handler: EventHandler): (() => void) => {
    if (!listenersRef.current.has(requestId)) {
      listenersRef.current.set(requestId, new Set());
    }
    listenersRef.current.get(requestId)!.add(handler);

    return () => {
      const set = listenersRef.current.get(requestId);
      if (set) {
        set.delete(handler);
        if (set.size === 0) {
          listenersRef.current.delete(requestId);
        }
      }
    };
  }, []);

  const setActiveRequestsHandler = useCallback((handler: ActiveRequestsHandler | null) => {
    activeRequestsHandlerRef.current = handler;
  }, []);

  return { connected, addListener, setActiveRequestsHandler };
}
