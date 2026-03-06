import { spawn, ChildProcess } from "child_process";
import * as net from "net";
import * as os from "os";
import * as path from "path";
import * as fs from "fs";
import { fileURLToPath } from "url";
import { scenarios, type BenchmarkCommand, type Scenario } from "./scenarios.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// ---------------------------------------------------------------------------
// Socket / daemon helpers
// ---------------------------------------------------------------------------

function getSocketDir(): string {
  if (process.env.AGENT_BROWSER_SOCKET_DIR) {
    return process.env.AGENT_BROWSER_SOCKET_DIR;
  }
  if (process.env.XDG_RUNTIME_DIR) {
    return path.join(process.env.XDG_RUNTIME_DIR, "agent-browser");
  }
  const home = os.homedir();
  if (home) {
    return path.join(home, ".agent-browser");
  }
  return path.join(os.tmpdir(), "agent-browser");
}

function getSocketPath(session: string): string {
  return path.join(getSocketDir(), `${session}.sock`);
}

function getProjectRoot(): string {
  return path.resolve(__dirname, "../..");
}

function getNativeBinaryPath(): string {
  const root = getProjectRoot();
  const p = os.platform();
  const a = os.arch();

  const osKey =
    p === "darwin" ? "darwin" : p === "linux" ? "linux" : p === "win32" ? "win32" : null;
  const archKey =
    a === "x64" || a === "x86_64" ? "x64" : a === "arm64" || a === "aarch64" ? "arm64" : null;

  if (!osKey || !archKey) {
    throw new Error(`Unsupported platform: ${p}-${a}`);
  }

  const ext = p === "win32" ? ".exe" : "";
  const binName = `agent-browser-${osKey}-${archKey}${ext}`;

  const candidates = [
    path.join(root, "cli/target/release/agent-browser"),
    path.join(root, "cli/target/debug/agent-browser"),
    path.join(root, "bin", binName),
  ];

  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }

  throw new Error(
    `Native binary not found. Tried:\n${candidates.map((c) => "  " + c).join("\n")}\n` +
      'Run "pnpm build:native" to build the native binary.',
  );
}

function sendCommand(session: string, cmd: BenchmarkCommand): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    const socketPath = getSocketPath(session);
    const client = net.createConnection({ path: socketPath }, () => {
      client.write(JSON.stringify(cmd) + "\n");
    });

    let data = "";
    client.on("data", (chunk) => {
      data += chunk.toString();
      const newlineIdx = data.indexOf("\n");
      if (newlineIdx !== -1) {
        const line = data.slice(0, newlineIdx);
        client.destroy();
        try {
          resolve(JSON.parse(line));
        } catch {
          reject(new Error(`Invalid JSON response: ${line}`));
        }
      }
    });

    client.on("error", (err) => reject(err));
    client.on("timeout", () => {
      client.destroy();
      reject(new Error("Socket timeout"));
    });
    client.setTimeout(30_000);
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitForSocket(session: string, timeoutMs = 15_000): Promise<void> {
  const start = Date.now();
  const socketPath = getSocketPath(session);
  while (Date.now() - start < timeoutMs) {
    if (fs.existsSync(socketPath)) {
      try {
        await new Promise<void>((resolve, reject) => {
          const c = net.createConnection({ path: socketPath }, () => {
            c.destroy();
            resolve();
          });
          c.on("error", reject);
          c.setTimeout(1000);
          c.on("timeout", () => {
            c.destroy();
            reject(new Error("timeout"));
          });
        });
        return;
      } catch {
        // not ready yet
      }
    }
    await sleep(100);
  }
  throw new Error(`Daemon '${session}' did not become ready within ${timeoutMs}ms`);
}

interface DaemonHandle {
  session: string;
  process: ChildProcess;
}

function spawnNodeDaemon(session: string): DaemonHandle {
  const daemonPath = path.join(getProjectRoot(), "dist/daemon.js");
  if (!fs.existsSync(daemonPath)) {
    throw new Error(`Node daemon not found at ${daemonPath}. Run "pnpm build" first.`);
  }

  const child = spawn("node", [daemonPath], {
    env: {
      ...process.env,
      AGENT_BROWSER_DAEMON: "1",
      AGENT_BROWSER_SESSION: session,
    },
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
  });

  child.stderr?.on("data", (chunk) => {
    const msg = chunk.toString().trim();
    if (msg && process.env.BENCH_DEBUG) {
      process.stderr.write(`[node-daemon] ${msg}\n`);
    }
  });

  return { session, process: child };
}

function spawnNativeDaemon(session: string): DaemonHandle {
  const binaryPath = getNativeBinaryPath();

  const child = spawn(binaryPath, [], {
    env: {
      ...process.env,
      AGENT_BROWSER_DAEMON: "1",
      AGENT_BROWSER_SESSION: session,
    },
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
  });

  child.stderr?.on("data", (chunk) => {
    const msg = chunk.toString().trim();
    if (msg && process.env.BENCH_DEBUG) {
      process.stderr.write(`[native-daemon] ${msg}\n`);
    }
  });

  return { session, process: child };
}

async function closeDaemon(handle: DaemonHandle): Promise<void> {
  try {
    await sendCommand(handle.session, { id: "close", action: "close" });
  } catch {
    // daemon may already be gone
  }
  await sleep(200);
  try {
    handle.process.kill("SIGTERM");
  } catch {
    // already exited
  }
}

function cleanupSockets(): void {
  for (const session of ["bench-node", "bench-native"]) {
    const sockPath = getSocketPath(session);
    const pidPath = sockPath.replace(/\.sock$/, ".pid");
    try {
      fs.unlinkSync(sockPath);
    } catch {
      /* */
    }
    try {
      fs.unlinkSync(pidPath);
    } catch {
      /* */
    }
  }
}

// ---------------------------------------------------------------------------
// Statistics (microsecond precision)
// ---------------------------------------------------------------------------

interface Stats {
  avgUs: number;
  minUs: number;
  maxUs: number;
  p50Us: number;
  p95Us: number;
}

function computeStats(timingsUs: number[]): Stats {
  const sorted = [...timingsUs].sort((a, b) => a - b);
  const sum = sorted.reduce((a, b) => a + b, 0);
  return {
    avgUs: Math.round(sum / sorted.length),
    minUs: sorted[0],
    maxUs: sorted[sorted.length - 1],
    p50Us: sorted[Math.floor(sorted.length * 0.5)],
    p95Us: sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))],
  };
}

function formatDuration(us: number): string {
  if (us >= 1_000_000) return `${(us / 1_000_000).toFixed(2)}s`;
  if (us >= 1_000) return `${(us / 1_000).toFixed(1)}ms`;
  return `${us}us`;
}

// ---------------------------------------------------------------------------
// Benchmark runner
// ---------------------------------------------------------------------------

async function runCommands(session: string, commands: BenchmarkCommand[]): Promise<void> {
  for (const cmd of commands) {
    const resp = await sendCommand(session, cmd);
    if (!(resp as { success?: boolean }).success) {
      throw new Error(
        `Command '${cmd.action}' failed on session '${session}': ${JSON.stringify(resp)}`,
      );
    }
  }
}

async function timeCommands(session: string, commands: BenchmarkCommand[]): Promise<number> {
  const start = process.hrtime.bigint();
  await runCommands(session, commands);
  const elapsedNs = process.hrtime.bigint() - start;
  return Number(elapsedNs / 1000n); // microseconds
}

interface ScenarioResult {
  name: string;
  nodeStats: Stats | null;
  nativeStats: Stats | null;
}

async function runScenario(
  scenario: Scenario,
  sessions: { node?: string; native?: string },
  iterations: number,
  warmup: number,
): Promise<ScenarioResult> {
  const result: ScenarioResult = {
    name: scenario.name,
    nodeStats: null,
    nativeStats: null,
  };

  for (const [label, session] of Object.entries(sessions)) {
    if (!session) continue;

    if (scenario.setup) {
      await runCommands(session, scenario.setup);
    }

    for (let i = 0; i < warmup; i++) {
      await timeCommands(session, scenario.commands);
    }

    const timings: number[] = [];
    for (let i = 0; i < iterations; i++) {
      timings.push(await timeCommands(session, scenario.commands));
    }

    if (scenario.teardown) {
      await runCommands(session, scenario.teardown);
    }

    const stats = computeStats(timings);
    if (label === "node") result.nodeStats = stats;
    else result.nativeStats = stats;
  }

  return result;
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

function pad(s: string, len: number): string {
  return s.padEnd(len);
}

function rpad(s: string, len: number): string {
  return s.padStart(len);
}

function formatSpeedup(nodeUs: number, nativeUs: number): string {
  if (nativeUs === 0 && nodeUs === 0) return "  --";
  if (nativeUs === 0) return "  >>>";
  const ratio = nodeUs / nativeUs;
  return `${ratio.toFixed(1)}x`;
}

function printResults(results: ScenarioResult[], iterations: number, warmup: number): void {
  const bothPaths = results[0].nodeStats !== null && results[0].nativeStats !== null;

  console.log("");
  const header = bothPaths
    ? `agent-browser benchmark: node vs native (${iterations} iterations, ${warmup} warmup)`
    : `agent-browser benchmark (${iterations} iterations, ${warmup} warmup)`;
  console.log(header);
  console.log("=".repeat(header.length));
  console.log("");

  if (bothPaths) {
    const nameW = 20;
    const colW = 14;

    console.log(
      pad("Scenario", nameW) +
        rpad("Node (avg)", colW) +
        rpad("Native (avg)", colW) +
        rpad("Speedup", 10),
    );
    console.log("-".repeat(nameW + colW * 2 + 10));

    let totalNodeUs = 0;
    let totalNativeUs = 0;
    let count = 0;

    for (const r of results) {
      if (!r.nodeStats || !r.nativeStats) continue;
      totalNodeUs += r.nodeStats.avgUs;
      totalNativeUs += r.nativeStats.avgUs;
      count++;

      console.log(
        pad(r.name, nameW) +
          rpad(formatDuration(r.nodeStats.avgUs), colW) +
          rpad(formatDuration(r.nativeStats.avgUs), colW) +
          rpad(formatSpeedup(r.nodeStats.avgUs, r.nativeStats.avgUs), 10),
      );
    }

    console.log("-".repeat(nameW + colW * 2 + 10));

    if (count > 0 && totalNativeUs > 0) {
      const overallSpeedup = totalNodeUs / totalNativeUs;
      const winner = overallSpeedup >= 1.0 ? "native is faster" : "node is faster";
      console.log(`Overall average speedup: ${overallSpeedup.toFixed(1)}x (${winner})`);
      console.log("");

      const allNativeFaster = results.every(
        (r) => !r.nodeStats || !r.nativeStats || r.nodeStats.avgUs >= r.nativeStats.avgUs,
      );
      if (allNativeFaster) {
        console.log("Result: PASS -- native is faster across all scenarios");
      } else {
        const slower = results
          .filter((r) => r.nodeStats && r.nativeStats && r.nodeStats.avgUs < r.nativeStats.avgUs)
          .map((r) => r.name);
        console.log(`Result: WARN -- native is slower in: ${slower.join(", ")}`);
      }
    }
  } else {
    const nameW = 20;
    const label = results[0].nodeStats ? "Node" : "Native";
    console.log(
      pad("Scenario", nameW) +
        rpad(`${label} avg`, 10) +
        rpad("min", 10) +
        rpad("max", 10) +
        rpad("p50", 10) +
        rpad("p95", 10),
    );
    console.log("-".repeat(nameW + 50));
    for (const r of results) {
      const s = r.nodeStats ?? r.nativeStats;
      if (!s) continue;
      console.log(
        pad(r.name, nameW) +
          rpad(formatDuration(s.avgUs), 10) +
          rpad(formatDuration(s.minUs), 10) +
          rpad(formatDuration(s.maxUs), 10) +
          rpad(formatDuration(s.p50Us), 10) +
          rpad(formatDuration(s.p95Us), 10),
      );
    }
  }

  console.log("");
}

function writeJsonResults(results: ScenarioResult[], outputPath: string): void {
  const toMs = (us: number) => +(us / 1000).toFixed(2);
  const json = results.map((r) => ({
    scenario: r.name,
    node: r.nodeStats
      ? {
          avg_ms: toMs(r.nodeStats.avgUs),
          min_ms: toMs(r.nodeStats.minUs),
          max_ms: toMs(r.nodeStats.maxUs),
          p50_ms: toMs(r.nodeStats.p50Us),
          p95_ms: toMs(r.nodeStats.p95Us),
        }
      : null,
    native: r.nativeStats
      ? {
          avg_ms: toMs(r.nativeStats.avgUs),
          min_ms: toMs(r.nativeStats.minUs),
          max_ms: toMs(r.nativeStats.maxUs),
          p50_ms: toMs(r.nativeStats.p50Us),
          p95_ms: toMs(r.nativeStats.p95Us),
        }
      : null,
    speedup:
      r.nodeStats && r.nativeStats && r.nativeStats.avgUs > 0
        ? +(r.nodeStats.avgUs / r.nativeStats.avgUs).toFixed(2)
        : null,
  }));
  fs.writeFileSync(outputPath, JSON.stringify(json, null, 2) + "\n");
  console.log(`JSON results written to ${outputPath}`);
}

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

interface CliArgs {
  iterations: number;
  warmup: number;
  nodeOnly: boolean;
  nativeOnly: boolean;
  json: boolean;
}

function parseArgs(): CliArgs {
  const args = process.argv.slice(2);
  const result: CliArgs = {
    iterations: 10,
    warmup: 3,
    nodeOnly: false,
    nativeOnly: false,
    json: false,
  };

  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case "--iterations":
        result.iterations = parseInt(args[++i], 10);
        break;
      case "--warmup":
        result.warmup = parseInt(args[++i], 10);
        break;
      case "--node-only":
        result.nodeOnly = true;
        break;
      case "--native-only":
        result.nativeOnly = true;
        break;
      case "--json":
        result.json = true;
        break;
      default:
        console.error(`Unknown flag: ${args[i]}`);
        process.exit(1);
    }
  }

  return result;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  const args = parseArgs();
  const runNode = !args.nativeOnly;
  const runNative = !args.nodeOnly;

  cleanupSockets();

  console.log("Starting benchmark daemons...");

  let nodeHandle: DaemonHandle | undefined;
  let nativeHandle: DaemonHandle | undefined;

  try {
    if (runNode) {
      nodeHandle = spawnNodeDaemon("bench-node");
      await waitForSocket("bench-node");
      console.log("  Node daemon ready");
    }

    if (runNative) {
      nativeHandle = spawnNativeDaemon("bench-native");
      await waitForSocket("bench-native");
      console.log("  Native daemon ready");
    }

    const sessions: { node?: string; native?: string } = {};
    if (runNode) sessions.node = "bench-node";
    if (runNative) sessions.native = "bench-native";

    // Launch browsers on both daemons
    for (const session of Object.values(sessions)) {
      const resp = await sendCommand(session, {
        id: "launch",
        action: "launch",
        headless: true,
      });
      if (!(resp as { success?: boolean }).success) {
        throw new Error(`Failed to launch browser on ${session}: ${JSON.stringify(resp)}`);
      }
    }
    console.log("  Browsers launched");
    console.log("");

    // Run all scenarios
    const results: ScenarioResult[] = [];
    for (const scenario of scenarios) {
      process.stdout.write(`  Running: ${scenario.name}...`);
      const result = await runScenario(scenario, sessions, args.iterations, args.warmup);
      results.push(result);

      if (result.nodeStats && result.nativeStats) {
        const speedup = formatSpeedup(result.nodeStats.avgUs, result.nativeStats.avgUs);
        process.stdout.write(
          ` node=${formatDuration(result.nodeStats.avgUs)} native=${formatDuration(result.nativeStats.avgUs)} (${speedup})\n`,
        );
      } else {
        const s = result.nodeStats ?? result.nativeStats;
        process.stdout.write(` avg=${s ? formatDuration(s.avgUs) : "??"}\n`);
      }
    }

    printResults(results, args.iterations, args.warmup);

    if (args.json) {
      writeJsonResults(results, path.join(getProjectRoot(), "test/benchmarks/results.json"));
    }

    // Close browsers
    for (const session of Object.values(sessions)) {
      await sendCommand(session, { id: "close", action: "close" }).catch(() => {});
    }

    await sleep(300);

    // CI gate: exit 1 if native is slower overall (total avg across all scenarios)
    if (runNode && runNative) {
      let totalNodeUs = 0;
      let totalNativeUs = 0;
      for (const r of results) {
        if (r.nodeStats && r.nativeStats) {
          totalNodeUs += r.nodeStats.avgUs;
          totalNativeUs += r.nativeStats.avgUs;
        }
      }
      if (totalNativeUs > 0 && totalNodeUs / totalNativeUs < 1.0) {
        process.exit(1);
      }
    }
  } finally {
    if (nodeHandle) await closeDaemon(nodeHandle);
    if (nativeHandle) await closeDaemon(nativeHandle);
    cleanupSockets();
  }
}

main().catch((err) => {
  console.error("Benchmark failed:", err.message || err);
  process.exit(2);
});
