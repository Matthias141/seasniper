import { useEffect, useRef, useState } from 'react';
import type { ServerEvent } from '../types';
import { getAuthToken } from './api';

function wsUrl(): string {
  const base = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/events`;
  // Browsers can't set an Authorization header on a WebSocket handshake —
  // a ?token= query param is what api.rs's auth middleware checks for this
  // route specifically (see auth.rs's doc comment for why not a
  // subprotocol instead). initAuth() must have resolved before this hook
  // mounts, or the connection is unauthenticated and the server rejects it.
  const token = getAuthToken();
  return token ? `${base}?token=${encodeURIComponent(token)}` : base;
}

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
      socket = new WebSocket(wsUrl());

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
