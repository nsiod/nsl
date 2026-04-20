import { describe, test, expect, beforeAll, afterAll } from "bun:test";
import {
  allocPort,
  createStateDir,
  cleanupStateDir,
  nsl,
  startProxy,
  stopProxy,
} from "../helpers";

describe("CLI commands", () => {
  let stateDir: string;
  let proxyPort: number;

  beforeAll(async () => {
    stateDir = createStateDir();
    proxyPort = allocPort();
    await startProxy(stateDir, proxyPort);
  });

  afterAll(async () => {
    await stopProxy();
    cleanupStateDir(stateDir);
  });

  test("nsl status prints resolved configuration", async () => {
    const result = await nsl(["status"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("proxy.listen:");
    expect(result.stdout).toContain(String(proxyPort));
    expect(result.stdout).toContain("proxy.https:");
    expect(result.stdout).toContain("localhost");
  });

  test("nsl status shows proxy state", async () => {
    const result = await nsl(["status"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("running");
  });

  test("nsl get returns correct URL format", async () => {
    const result = await nsl(["get", "myapp"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toMatch(/http:\/\/myapp\.localhost/);
  });

  test("nsl list with no routes", async () => {
    const result = await nsl(["list"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("No active routes");
  });

  test("nsl --version prints version", async () => {
    const result = await nsl(["--version"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toMatch(/nsl \d+\.\d+\.\d+/);
  });

  test("nsl --help shows usage", async () => {
    const result = await nsl(["--help"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("Usage:");
    expect(result.stdout).toContain("Commands:");
  });

  test("nsl route requires name", async () => {
    const result = await nsl(["route", "--remove"], stateDir, proxyPort);
    expect(result.exitCode).not.toBe(0);
  });
});
