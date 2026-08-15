import { useEffect, useRef, useState } from 'react';
import type { ServerEvent } from '../types';

const WS_URL = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/events`;

/**
 * Reconnects with backoff on drop. A control panel for a time-sensitive
 * action cannot silently go stale — `connected` is exposed explicitly so
 * the UI can show a hard "DISCONNECTED" state rather than quietly showing
 * the last-known-good data as if it were current.
 */
export function useEventSocket(onEvent: (e: ServerEvent) => void) {
  const [connected, setConnected] = useState(false);
  const handlerRef = useRef(onEvent);
  handlerRef.current = onEvent;

  useEffect(() => {
    let socket: WebSocket | null = null;
    let retryDelay = 500;
    let closed = false;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;

    function connect() {
      if (closed) return;
      socket = new WebSocket(WS_URL);

      socket.onopen = () => {
        setConnected(true);
        retryDelay = 500;
      };

      socket.onmessage = (msg) => {
        try {
          const event = JSON.parse(msg.data) as ServerEvent;
          handlerRef.current(event);
        } catch {
          // ignore malformed frame
        }
      };

      socket.onclose = () => {
        setConnected(false);
        if (!closed) {
          retryTimer = setTimeout(connect, retryDelay);
          retryDelay = Math.min(retryDelay * 1.7, 8000);
        }
      };

      socket.onerror = () => {
        socket?.close();
      };
    }

    connect();
    return () => {
      closed = true;
      clearTimeout(retryTimer);
      socket?.close();
    };
  }, []);

  return { connected };
}
