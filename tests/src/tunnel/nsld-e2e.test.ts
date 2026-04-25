import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawn, type Subprocess } from "bun";
import {
  NSLD_BIN,
  allocPort,
  cleanupStateDir,
  createStateDir,
  nsl,
  startProxy,
  startUpstream,
  stopProxy,
  type UpstreamServer,
} from "../helpers";

const BASE_DOMAIN = "nsl.test";
const TENANT_DOMAIN = `alice.${BASE_DOMAIN}`;
const PUBLIC_HOST = `myapp.${TENANT_DOMAIN}`;
const TOKEN = "nslk_test_secret";

describe("nsl client through nsld", () => {
  let nsldStateDir: string;
  let nslStateDir: string;
  let quicPort: number;
  let publicPort: number;
  let proxyPort: number;
  let nsldProc: Subprocess | null = null;
  let upstream: UpstreamServer | null = null;

  beforeAll(() => {
    nsldStateDir = createStateDir();
    nslStateDir = createStateDir();
    quicPort = allocPort();
    publicPort = allocPort();
    proxyPort = allocPort();

    writeFileSync(
      join(nsldStateDir, "tokens.toml"),
      `[[tokens]]
domain = "${TENANT_DOMAIN}"
key = "${TOKEN}"
`,
    );
    writeFileSync(
      join(nsldStateDir, "config.toml"),
      `[public]
https_listen = ""
http_listen = "127.0.0.1:${publicPort}"

[acme]
enable = false
`,
    );
  });

  afterAll(async () => {
    await stopProxy();
    if (nsldProc) {
      nsldProc.kill("SIGTERM");
      await nsldProc.exited;
      nsldProc = null;
    }
    upstream?.stop();
    cleanupStateDir(nsldStateDir);
    cleanupStateDir(nslStateDir);
  });

  test("routes public nsld HTTP traffic to a local nsl route", async () => {
    const serverIdPromise = startNsld();
    await waitForTcp(publicPort);
    const serverId = await serverIdPromise;

    await startProxy(nslStateDir, proxyPort, {
      NSL_DOMAINS: `localhost,${TENANT_DOMAIN}`,
      NSL_TUNNEL_ENABLE: "1",
      NSL_TUNNEL_DOMAIN: TENANT_DOMAIN,
      NSL_TUNNEL_KEY: TOKEN,
      NSL_TUNNEL_ENDPOINT: `127.0.0.1:${quicPort}`,
      NSL_TUNNEL_SERVER_ID: serverId,
    });

    upstream = startUpstream();
    const route = await nsl(
      ["route", "myapp", String(upstream.port)],
      nslStateDir,
      proxyPort,
      { NSL_DOMAINS: `localhost,${TENANT_DOMAIN}` },
    );
    expect(route.exitCode).toBe(0);

    const res = await waitForPublicRoute();
    expect(res.status).toBe(200);
    expect(res.headers.get("x-nsl")).toBe("1");
    const body = await res.json();
    expect(body.url).toBe("/hello?via=nsld");
    expect(body.headers.host).toBe(PUBLIC_HOST);
  });

  function startNsld(): Promise<string> {
    mkdirSync(nsldStateDir, { recursive: true });
    nsldProc = spawn(
      [
        NSLD_BIN,
        "--state-dir",
        nsldStateDir,
        "serve",
        "--listen",
        `127.0.0.1:${quicPort}`,
        "--base-domain",
        BASE_DOMAIN,
      ],
      {
        stdout: "pipe",
        stderr: "pipe",
      },
    );

    void drain(nsldProc.stderr);
    return readServerId(nsldProc.stdout);
  }

  async function waitForPublicRoute(): Promise<Response> {
    const deadline = Date.now() + 10_000;
    let lastStatus = 0;
    while (Date.now() < deadline) {
      try {
        const res = await fetch(`http://127.0.0.1:${publicPort}/hello?via=nsld`, {
          headers: { Host: PUBLIC_HOST },
        });
        if (res.status === 200) {
          return res;
        }
        lastStatus = res.status;
        await res.arrayBuffer();
      } catch {
        lastStatus = 0;
      }
      await Bun.sleep(100);
    }
    throw new Error(`public route did not become ready; last status ${lastStatus}`);
  }
});

async function waitForTcp(port: number) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const conn = await Bun.connect({
        hostname: "127.0.0.1",
        port,
        socket: {
          data() {},
          open(socket) {
            socket.end();
          },
          error() {},
        },
      });
      conn.end();
      return;
    } catch {
      await Bun.sleep(100);
    }
  }
  throw new Error(`TCP listener did not start on port ${port}`);
}

async function readServerId(stream: ReadableStream<Uint8Array> | null): Promise<string> {
  if (!stream) {
    throw new Error("nsld stdout was not captured");
  }
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let text = "";
  const deadline = Date.now() + 10_000;

  while (Date.now() < deadline) {
    const result = await Promise.race([
      reader.read(),
      Bun.sleep(100).then(() => undefined),
    ]);
    if (!result) {
      continue;
    }
    if (result.done) {
      break;
    }
    text += decoder.decode(result.value, { stream: true });
    const match = text.match(/tunnel server id: ([0-9a-f]{64})/);
    if (match) {
      return match[1];
    }
  }
  throw new Error(`timed out waiting for nsld server id; stdout so far: ${text}`);
}

async function drain(stream: ReadableStream<Uint8Array> | null) {
  if (!stream) {
    return;
  }
  for await (const _ of stream) {
    // keep child stderr pipe from filling
  }
}
