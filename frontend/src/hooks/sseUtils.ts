// ---------------------------------------------------------------------------
// SSE data parser
// ---------------------------------------------------------------------------

/**
 * Parse the SSE data string (full AgentEvent JSON) and extract the payload's
 * `data` field.
 */
export function parsePayloadData(raw: string): Record<string, unknown> | null {
  try {
    const event = JSON.parse(raw);
    return (event?.payload?.data as Record<string, unknown>) ?? null;
  } catch {
    console.warn("[SSE] failed to parse event data:", raw);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Managed EventSource (fetch-based)
// ---------------------------------------------------------------------------

export interface ManagedEventSource {
  close: () => void;
}

export interface CreateEventSourceOptions {
  /** Named SSE event listeners */
  listeners: Record<string, (e: MessageEvent) => void>;
  /** Optional custom headers for the fetch request (e.g., auth tokens) */
  headers?: Record<string, string>;
  /** Called when the connection opens */
  onOpen?: () => void;
  /** Called when the connection drops (before reconnect attempt) */
  onClose?: () => void;
}

/**
 * Compute reconnect delay with exponential backoff and +/- 20% jitter.
 * Sequence: ~0.2s, ~1s, ~2s, ~4s, ~8s, ~16s, capped at 30s.
 *
 * The first retry is deliberately near-immediate. This single shared
 * connection is critical-path for all live UI — streaming text and the typing
 * indicator both ride it — so a transient drop (laptop wake, network blip, or
 * an ao-server restart in dev) needs to self-heal fast; the historical ~1s
 * first delay surfaced as visible send/stream lag. Persistent outages still
 * back off exponentially so we don't hammer a down server.
 */
function backoffDelay(attempt: number): number {
  const base = attempt === 0 ? 200 : Math.min(1000 * Math.pow(2, attempt - 1), 30000);
  return base * (0.8 + Math.random() * 0.4);
}

// ---------------------------------------------------------------------------
// SSE line parser
// ---------------------------------------------------------------------------

interface SSEFrame {
  event: string;
  data: string;
}

/**
 * Parse a single SSE frame from accumulated lines. An SSE frame is terminated
 * by an empty line. Returns the parsed frame or null if incomplete / comment.
 */
function parseSSELines(lines: string[]): SSEFrame | null {
  let event = "message";
  let dataLines: string[] = [];

  for (const line of lines) {
    if (line.startsWith(":")) continue; // comment
    if (line.startsWith("event:")) {
      event = line.slice(6).trim();
    } else if (line.startsWith("data:")) {
      dataLines.push(line.slice(5).trimStart());
    } else if (line.includes(":")) {
      // Unknown field — ignore per SSE spec
    }
  }

  if (dataLines.length === 0) return null;
  return { event, data: dataLines.join("\n") };
}

// ---------------------------------------------------------------------------
// Fetch-based SSE stream reader
// ---------------------------------------------------------------------------

/**
 * Read an SSE stream from a fetch Response body and dispatch events to
 * listeners. Resolves when the stream ends or the abort signal fires.
 */
async function readSSEStream(
  response: Response,
  listeners: Record<string, (e: MessageEvent) => void>,
  signal: AbortSignal
): Promise<void> {
  const body = response.body;
  if (!body) return;

  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  try {
    while (!signal.aborted) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });

      // Process complete frames (delimited by double newline)
      let delimIdx: number;
      while ((delimIdx = buffer.indexOf("\n\n")) !== -1) {
        const rawFrame = buffer.slice(0, delimIdx);
        buffer = buffer.slice(delimIdx + 2);

        const lines = rawFrame.split("\n");
        const frame = parseSSELines(lines);
        if (!frame) continue;

        const handler = listeners[frame.event];
        if (handler) {
          // Construct a MessageEvent-compatible object
          const messageEvent = new MessageEvent(frame.event, {
            data: frame.data,
          });
          handler(messageEvent);
        }
      }
    }
  } catch (err) {
    // AbortError is expected when we close the connection
    if (err instanceof DOMException && err.name === "AbortError") return;
    throw err;
  } finally {
    reader.releaseLock();
  }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/** Creates a fetch-based SSE connection for a given URL with automatic
 *  reconnection and exponential backoff. Generic low-level primitive — callers
 *  supply the full URL rather than an entity id. */
export function createManagedEventSource(
  url: string,
  options: CreateEventSourceOptions
): ManagedEventSource {
  let abortController: AbortController | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let closed = false;
  let reconnectAttempt = 0;

  function cleanup() {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (abortController) {
      abortController.abort();
      abortController = null;
    }
  }

  function scheduleReconnect() {
    if (closed) return;
    options.onClose?.();
    const delay = backoffDelay(reconnectAttempt);
    reconnectAttempt++;
    reconnectTimer = setTimeout(connect, delay);
  }

  async function connect() {
    if (closed) return;
    cleanup();

    abortController = new AbortController();
    const { signal } = abortController;

    try {
      const response = await fetch(url, {
        headers: { Accept: "text/event-stream", ...options.headers },
        signal,
      });

      if (!response.ok) {
        console.warn(`[SSE] HTTP ${response.status} for ${url}, will reconnect`);
        scheduleReconnect();
        return;
      }

      reconnectAttempt = 0;
      options.onOpen?.();
      await readSSEStream(response, options.listeners, signal);

      if (!closed) {
        console.warn(`[SSE] stream ended for ${url}, will reconnect`);
        scheduleReconnect();
      }
    } catch (err) {
      if (signal.aborted) return;
      console.warn(`[SSE] connection error for ${url}, will reconnect`);
      scheduleReconnect();
    }
  }

  connect();

  return {
    close() {
      closed = true;
      cleanup();
    },
  };
}
