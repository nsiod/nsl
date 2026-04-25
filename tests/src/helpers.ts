import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const NSL_BIN = process.env.NSL_BIN ?? "nsl";
export const NSLD_BIN = process.env.NSLD_BIN ?? "nsld";

// -------------------------------------------------------------------------
// State directory isolation
// -------------------------------------------------------------------------

export function createStateDir(): string {
  return mkdtempSync(join(tmpdir(), "nsl-e2e-"));
}

export function cleanupStateDir(dir: string) {
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    // best-effort
  }
}

// -------------------------------------------------------------------------
// NSL CLI wrappers
// -------------------------------------------------------------------------

export function nslEnv(
  stateDir: string,
  proxyPort: number,
  extraEnv?: Record<string, string>,
) {
  return {
    ...process.env,
    NSL_STATE_DIR: stateDir,
    NSL_LISTEN: `127.0.0.1:${proxyPort}`,
    ...extraEnv,
  };
}

/** Run an nsl command and return stdout. */
export async function nsl(
  args: string[],
  stateDir: string,
  proxyPort: number,
  extraEnv?: Record<string, string>,
): Promise<{ stdout: string; stderr: string; exitCode: number }> {
  const proc = spawn([NSL_BIN, ...args], {
    env: nslEnv(stateDir, proxyPort, extraEnv),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  const exitCode = await proc.exited;
  return { stdout: stdout.trim(), stderr: stderr.trim(), exitCode };
}

// -------------------------------------------------------------------------
// Proxy lifecycle
// -------------------------------------------------------------------------

let proxyProc: Subprocess | null = null;

export async function startProxy(
  stateDir: string,
  port: number,
  extraEnv?: Record<string, string>,
) {
  proxyProc = spawn([NSL_BIN, "start", "--foreground"], {
    env: nslEnv(stateDir, port, extraEnv),
    stdout: "pipe",
    stderr: "pipe",
  });

  // Wait until the proxy is accepting connections
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const conn = await Bun.connect({
        hostname: "127.0.0.1",
        port,
        socket: {
          data() {},
          open(socket) { socket.end(); },
          error() {},
        },
      });
      conn.end();
      return;
    } catch {
      await Bun.sleep(100);
    }
  }
  throw new Error(`Proxy did not start within 10 s on port ${port}`);
}

export async function stopProxy() {
  if (proxyProc) {
    proxyProc.kill("SIGTERM");
    await proxyProc.exited;
    proxyProc = null;
  }
}

// -------------------------------------------------------------------------
// Simple upstream HTTP server (Bun.serve)
// -------------------------------------------------------------------------

export interface UpstreamServer {
  port: number;
  stop: () => void;
  requests: { method: string; url: string; headers: Record<string, string>; body: string }[];
}

export function startUpstream(handler?: (req: Request) => Response | Promise<Response>): UpstreamServer {
  const requests: UpstreamServer["requests"] = [];

  const defaultHandler = async (req: Request) => {
    const body = await req.text();
    const headers: Record<string, string> = {};
    req.headers.forEach((v, k) => { headers[k] = v; });
    requests.push({ method: req.method, url: req.url, headers, body });

    return new Response(JSON.stringify({
      method: req.method,
      url: new URL(req.url).pathname + (new URL(req.url).search || ""),
      headers,
      body,
    }), {
      headers: { "Content-Type": "application/json" },
    });
  };

  const server = Bun.serve({
    port: 0,
    fetch: handler ?? defaultHandler,
  });

  return {
    port: server.port,
    stop: () => server.stop(true),
    requests,
  };
}

// -------------------------------------------------------------------------
// Simple upstream WebSocket server
// -------------------------------------------------------------------------

export interface WsUpstream {
  port: number;
  stop: () => void;
  received: string[];
}

export function startWsUpstream(): WsUpstream {
  const received: string[] = [];

  const server = Bun.serve({
    port: 0,
    fetch(req, server) {
      if (server.upgrade(req)) return undefined;
      return new Response("Not a WebSocket request", { status: 400 });
    },
    websocket: {
      message(ws, message) {
        const text = typeof message === "string" ? message : new TextDecoder().decode(message);
        received.push(text);
        ws.send(`echo:${text}`);
      },
    },
  });

  return { port: server.port, stop: () => server.stop(true), received };
}

// -------------------------------------------------------------------------
// Port allocation
// -------------------------------------------------------------------------

let nextPort = 18_000 + Math.floor(Math.random() * 1000);

export function allocPort(): number {
  return nextPort++;
}
