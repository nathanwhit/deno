type Options = {
  workerRuntime: "deno" | "node" | "bun";
  workerBinary: string;
  workerArgs: string[];
  printJson: boolean;
  jsonOutPath?: string;
  echoHost: string;
  echoPort: number;
  bytes: string;
  chunk: string;
  inflight: string;
  httpBind: string;
  httpUrlHost: string;
  httpPort: number;
  httpResponseBytes: string;
  socketTimeoutSec: number;
  ohaConnections: number;
  ohaDuration: string;
  ohaRequests?: number;
};

type SocketStats = {
  host: string;
  port: number;
  sentBytes: number;
  receivedBytes: number;
  durationSec: number;
  throughputMiBPerSec: number;
  throughputGibitPerSec: number;
};

type HttpServerStats = {
  bind: string;
  port: number;
  requests: number;
  bytesOut: number;
  uptimeSec: number;
};

function parseArgs(argv: string[]): Options {
  let workerBinaryProvided = false;
  const options: Options = {
    workerRuntime: "deno",
    workerBinary: "deno",
    workerArgs: [],
    printJson: false,
    echoHost: "127.0.0.1",
    echoPort: 3002,
    bytes: "256m",
    chunk: "64k",
    inflight: "8m",
    httpBind: "127.0.0.1",
    httpUrlHost: "127.0.0.1",
    httpPort: 3003,
    httpResponseBytes: "2",
    socketTimeoutSec: 180,
    ohaConnections: 200,
    ohaDuration: "10s",
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = argv[i + 1];

    if (arg === "--echo-host" && next) {
      options.echoHost = next;
      i += 1;
      continue;
    }
    if (arg === "--worker-runtime" && next) {
      if (next !== "deno" && next !== "node" && next !== "bun") {
        throw new Error(`Invalid --worker-runtime: ${next}`);
      }
      options.workerRuntime = next;
      i += 1;
      continue;
    }
    if ((arg === "--worker-binary" || arg === "--worker-deno-binary") && next) {
      options.workerBinary = next;
      workerBinaryProvided = true;
      i += 1;
      continue;
    }
    if ((arg === "--worker-arg" || arg === "--worker-deno-arg") && next) {
      options.workerArgs.push(next);
      i += 1;
      continue;
    }
    if (arg === "--print-json") {
      options.printJson = true;
      continue;
    }
    if (arg === "--json-out" && next) {
      options.jsonOutPath = next;
      i += 1;
      continue;
    }
    if (arg === "--echo-port" && next) {
      options.echoPort = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--bytes" && next) {
      options.bytes = next;
      i += 1;
      continue;
    }
    if (arg === "--chunk" && next) {
      options.chunk = next;
      i += 1;
      continue;
    }
    if (arg === "--inflight" && next) {
      options.inflight = next;
      i += 1;
      continue;
    }
    if (arg === "--http-bind" && next) {
      options.httpBind = next;
      i += 1;
      continue;
    }
    if (arg === "--http-url-host" && next) {
      options.httpUrlHost = next;
      i += 1;
      continue;
    }
    if (arg === "--http-port" && next) {
      options.httpPort = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--http-response-bytes" && next) {
      options.httpResponseBytes = next;
      i += 1;
      continue;
    }
    if (arg === "--socket-timeout-sec" && next) {
      options.socketTimeoutSec = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--oha-connections" && next) {
      options.ohaConnections = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--oha-duration" && next) {
      options.ohaDuration = next;
      i += 1;
      continue;
    }
    if (arg === "--oha-requests" && next) {
      options.ohaRequests = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--help" || arg === "-h") {
      printUsage();
      Deno.exit(0);
    }

    throw new Error(`Unknown argument: ${arg}`);
  }

  if (
    !Number.isFinite(options.echoPort) || options.echoPort <= 0 ||
    options.echoPort > 65535
  ) {
    throw new Error(`Invalid --echo-port: ${options.echoPort}`);
  }
  if (!workerBinaryProvided) {
    options.workerBinary = options.workerRuntime;
  }
  if (options.workerBinary.trim().length === 0) {
    throw new Error("--worker-binary cannot be empty");
  }
  if (
    !Number.isFinite(options.httpPort) || options.httpPort <= 0 ||
    options.httpPort > 65535
  ) {
    throw new Error(`Invalid --http-port: ${options.httpPort}`);
  }
  if (!Number.isFinite(options.ohaConnections) || options.ohaConnections <= 0) {
    throw new Error(`Invalid --oha-connections: ${options.ohaConnections}`);
  }
  if (
    !Number.isFinite(options.socketTimeoutSec) || options.socketTimeoutSec <= 0
  ) {
    throw new Error(
      `Invalid --socket-timeout-sec: ${options.socketTimeoutSec}`,
    );
  }
  if (
    options.ohaRequests !== undefined &&
    (!Number.isFinite(options.ohaRequests) || options.ohaRequests <= 0)
  ) {
    throw new Error(`Invalid --oha-requests: ${options.ohaRequests}`);
  }

  return options;
}

function printUsage(): void {
  console.log("Usage: deno run -A benchmark-orchestrator.ts [options]");
  console.log("");
  console.log("Output:");
  console.log(
    "  --print-json                  Print full BENCHMARK_REPORT JSON to stdout",
  );
  console.log(
    "  --json-out <path>             Write BENCHMARK_REPORT JSON to file",
  );
  console.log("");
  console.log("Infrastructure:");
  console.log(
    "  --worker-runtime <deno|node|bun> Worker runtime (default: deno)",
  );
  console.log(
    "  --worker-binary <path>        Worker binary path (default: deno, node, or bun)",
  );
  console.log(
    "  --worker-arg <arg>            Extra arg for worker runtime (repeatable)",
  );
  console.log(
    "                                Back-compat aliases: --worker-deno-binary, --worker-deno-arg",
  );
  console.log(
    "  --echo-host <host>            Echo server host (default: 127.0.0.1)",
  );
  console.log(
    "  --echo-port <port>            Echo server port (default: 3002)",
  );
  console.log(
    "  --http-bind <host>            HTTP benchmark bind host (default: 127.0.0.1)",
  );
  console.log(
    "  --http-url-host <host>        HTTP benchmark URL host (default: 127.0.0.1)",
  );
  console.log(
    "  --http-port <port>            HTTP benchmark port (default: 3003)",
  );
  console.log("");
  console.log("Socket benchmark:");
  console.log(
    "  --bytes <n[k|m|g]>            Total socket bytes (default: 256m)",
  );
  console.log(
    "  --chunk <n[k|m|g]>            Socket chunk size (default: 64k)",
  );
  console.log(
    "  --inflight <n[k|m|g]>         Socket in-flight bytes (default: 8m)",
  );
  console.log(
    "  --socket-timeout-sec <sec>    Socket benchmark timeout (default: 180)",
  );
  console.log("");
  console.log("HTTP benchmark (oha):");
  console.log(
    "  --http-response-bytes <size>  HTTP response body size (default: 2)",
  );
  console.log("  --oha-connections <n>         oha concurrency (default: 200)");
  console.log(
    "  --oha-duration <dur>          oha duration, e.g. 10s (default: 10s)",
  );
  console.log("  --oha-requests <n>            optional fixed request count");
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitForPort(
  host: string,
  port: number,
  timeoutMs: number,
): Promise<void> {
  const start = Date.now();
  let lastError: unknown;
  while (Date.now() - start < timeoutMs) {
    try {
      const conn = await Deno.connect({ hostname: host, port });
      conn.close();
      return;
    } catch (error) {
      lastError = error;
      await delay(100);
    }
  }
  throw new Error(
    `Timed out waiting for ${host}:${port} (${String(lastError)})`,
  );
}

async function consumeLines(
  stream: ReadableStream<Uint8Array> | null,
  onLine: (line: string) => void,
): Promise<void> {
  if (!stream) return;
  const reader = stream.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += value;
    let idx = buffer.indexOf("\n");
    while (idx !== -1) {
      const line = buffer.slice(0, idx).replace(/\r$/, "");
      buffer = buffer.slice(idx + 1);
      onLine(line);
      idx = buffer.indexOf("\n");
    }
  }
  if (buffer.length > 0) {
    onLine(buffer.replace(/\r$/, ""));
  }
}

function isCargoBuildProgressLine(line: string): boolean {
  const trimmed = line.trim();
  return trimmed.startsWith("Compiling ") ||
    trimmed.startsWith("Finished ") ||
    trimmed.startsWith("Checking ") ||
    trimmed.startsWith("Blocking waiting for file lock") ||
    trimmed.startsWith("Updating ") ||
    trimmed.startsWith("Downloading ") ||
    trimmed.startsWith("Downloaded ") ||
    trimmed.startsWith("warning:") ||
    trimmed.startsWith("error:");
}

function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  message: string,
): Promise<T> {
  let timeoutId: number | undefined;
  const timeoutPromise = new Promise<T>((_, reject) => {
    timeoutId = setTimeout(() => {
      reject(new Error(message));
    }, timeoutMs);
  });
  return Promise.race([promise, timeoutPromise]).finally(() => {
    if (timeoutId !== undefined) clearTimeout(timeoutId);
  });
}

async function killGracefully(
  child: Deno.ChildProcess,
  label: string,
): Promise<void> {
  try {
    child.kill("SIGTERM");
  } catch {
    return;
  }
  try {
    await withTimeout(
      child.status,
      2000,
      `${label} did not exit after SIGTERM`,
    );
  } catch {
    try {
      child.kill("SIGKILL");
    } catch {
      return;
    }
    await child.status.catch(() => undefined);
  }
}

function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GiB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(2)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(2)} KiB`;
  return `${bytes} B`;
}

function pickNumber(obj: unknown, candidates: string[]): number | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const rec = obj as Record<string, unknown>;
  for (const key of candidates) {
    const value = rec[key];
    if (typeof value === "number" && Number.isFinite(value)) return value;
  }
  return undefined;
}

const options = parseArgs(Deno.args);
const workerScriptPath =
  new URL("./socket-http-benchmark.mjs", import.meta.url).pathname;

console.log(
  `[echo] starting Rust echo server via cargo run --release (first build can take a while)`,
);
const echo = new Deno.Command("cargo", {
  args: [
    "run",
    "--release",
    "--bin",
    "echo_server",
    "--",
    "--port",
    String(options.echoPort),
  ],
  stdout: "piped",
  stderr: "piped",
}).spawn();
const echoOutput: string[] = [];
const echoLineTasks = [
  consumeLines(echo.stdout, (line) => echoOutput.push(`[echo] ${line}`)),
  consumeLines(echo.stderr, (line) => {
    if (isCargoBuildProgressLine(line)) {
      console.error(`[echo:err] ${line}`);
    }
  }),
];

let worker: Deno.ChildProcess | undefined;
let socketStats: SocketStats | undefined;
let workerHttpStats: HttpServerStats | undefined;
const workerOutput: string[] = [];
let ohaOutputRaw = "";
let ohaErrorRaw = "";

try {
  const echoWaitStartedAt = Date.now();
  const echoWaitTimer = setInterval(() => {
    const elapsedSec = ((Date.now() - echoWaitStartedAt) / 1000).toFixed(1);
    console.log(
      `[echo] cargo run still starting (${elapsedSec}s elapsed)`,
    );
  }, 2000);
  try {
    await waitForPort(options.echoHost, options.echoPort, 30_000);
  } finally {
    clearInterval(echoWaitTimer);
  }
  console.log(`[echo] ready on ${options.echoHost}:${options.echoPort}`);

  const workerBenchmarkArgs = [
    "--host",
    options.echoHost,
    "--port",
    String(options.echoPort),
    "--bytes",
    options.bytes,
    "--chunk",
    options.chunk,
    "--inflight",
    options.inflight,
    "--http-bind",
    options.httpBind,
    "--http-url-host",
    options.httpUrlHost,
    "--http-port",
    String(options.httpPort),
    "--http-response-bytes",
    options.httpResponseBytes,
  ];

  const workerArgs = options.workerRuntime === "deno"
    ? [
      ...options.workerArgs,
      "run",
      "-A",
      workerScriptPath,
      ...workerBenchmarkArgs,
    ]
    : [...options.workerArgs, workerScriptPath, ...workerBenchmarkArgs];

  worker = new Deno.Command(options.workerBinary, {
    args: workerArgs,
    stdout: "piped",
    stderr: "piped",
  }).spawn();

  let socketStatsResolve: ((value: SocketStats) => void) | undefined;
  let socketStatsReject: ((reason?: unknown) => void) | undefined;
  const socketStatsReady = new Promise<SocketStats>((resolve, reject) => {
    socketStatsResolve = resolve;
    socketStatsReject = reject;
  });

  const workerLineTasks = [
    consumeLines(worker.stdout, (line) => {
      workerOutput.push(`[worker] ${line}`);
      if (line.startsWith("SOCKET_STATS ")) {
        try {
          const payload = line.slice("SOCKET_STATS ".length);
          socketStatsResolve?.(JSON.parse(payload));
        } catch (error) {
          socketStatsReject?.(
            new Error(
              `Failed to parse SOCKET_STATS: ${(error as Error).message}`,
            ),
          );
        }
      } else if (line.startsWith("HTTP_SERVER_STATS ")) {
        try {
          const payload = line.slice("HTTP_SERVER_STATS ".length);
          workerHttpStats = JSON.parse(payload);
        } catch {
          // ignore malformed line
        }
      }
    }),
    consumeLines(
      worker.stderr,
      (line) => workerOutput.push(`[worker:err] ${line}`),
    ),
  ];

  await waitForPort(options.httpUrlHost, options.httpPort, 5000);
  const targetUrl = `http://${options.httpUrlHost}:${options.httpPort}/`;
  const ohaArgs = [
    "--output-format",
    "json",
    "-c",
    String(options.ohaConnections),
    "-z",
    options.ohaDuration,
  ];
  if (options.ohaRequests !== undefined) {
    ohaArgs.push("-n", String(options.ohaRequests));
  }
  ohaArgs.push(targetUrl);

  // Run HTTP and socket benchmarks concurrently to stress the same event loop window.
  const ohaPromise = new Deno.Command("oha", {
    args: ohaArgs,
    env: { NO_COLOR: "false" },
    stdout: "piped",
    stderr: "piped",
  }).output();
  const socketStatsPromise = withTimeout(
    socketStatsReady,
    options.socketTimeoutSec * 1000,
    `Timed out waiting for socket benchmark after ${options.socketTimeoutSec}s`,
  );

  const [socketResult, ohaOutput] = await Promise.all([
    socketStatsPromise,
    ohaPromise,
  ]);
  socketStats = socketResult;
  ohaOutputRaw = new TextDecoder().decode(ohaOutput.stdout).trim();
  ohaErrorRaw = new TextDecoder().decode(ohaOutput.stderr).trim();
  if (!ohaOutput.success) {
    throw new Error(
      `oha failed (${ohaOutput.code}): ${ohaErrorRaw || ohaOutputRaw}`,
    );
  }

  await killGracefully(worker, "worker");
  await Promise.all(workerLineTasks);
} finally {
  if (worker) {
    await killGracefully(worker, "worker");
  }
  await killGracefully(echo, "echo");
  await Promise.allSettled(echoLineTasks);
}

if (!socketStats) {
  throw new Error("Missing socket stats");
}

let ohaJson: Record<string, unknown>;
try {
  ohaJson = JSON.parse(ohaOutputRaw);
} catch (error) {
  throw new Error(
    `Failed to parse oha JSON output: ${
      (error as Error).message
    }\nRaw output:\n${ohaOutputRaw}`,
  );
}

const summary = (ohaJson.summary && typeof ohaJson.summary === "object")
  ? (ohaJson.summary as Record<string, unknown>)
  : {};
const latency =
  (ohaJson.latencyPercentiles && typeof ohaJson.latencyPercentiles === "object")
    ? (ohaJson.latencyPercentiles as Record<string, unknown>)
    : {};

const ohaRequestsPerSec = pickNumber(summary, [
  "requestsPerSec",
  "requests_per_sec",
]);
const ohaAverageSec = pickNumber(summary, ["average"]);
const ohaP95Sec = pickNumber(latency, ["p95", "95"]);
const ohaSuccessRate = pickNumber(summary, ["successRate", "success_rate"]);
const ohaTotalData = pickNumber(summary, ["totalData", "total_data"]);

console.log("=== Benchmark Report ===");
console.log("");
console.log("Socket benchmark");
console.log(
  `  worker:       ${options.workerRuntime} :: ${options.workerBinary} ${
    options.workerArgs.join(" ")
  }`.trimEnd(),
);
console.log(`  target:       ${socketStats.host}:${socketStats.port}`);
console.log(`  sent:         ${formatBytes(socketStats.sentBytes)}`);
console.log(`  received:     ${formatBytes(socketStats.receivedBytes)}`);
console.log(`  duration:     ${socketStats.durationSec.toFixed(3)} s`);
console.log(
  `  throughput:   ${socketStats.throughputMiBPerSec.toFixed(2)} MiB/s (${
    socketStats.throughputGibitPerSec.toFixed(2)
  } Gibit/s)`,
);
console.log("");
console.log("HTTP benchmark (oha)");
console.log(
  `  target:       http://${options.httpUrlHost}:${options.httpPort}/`,
);
console.log("  mode:         concurrent with socket benchmark");
if (ohaRequestsPerSec !== undefined) {
  console.log(`  req/sec:      ${ohaRequestsPerSec.toFixed(2)}`);
}
if (ohaAverageSec !== undefined) {
  console.log(`  avg latency:  ${(ohaAverageSec * 1000).toFixed(2)} ms`);
}
if (ohaP95Sec !== undefined) {
  console.log(`  p95 latency:  ${(ohaP95Sec * 1000).toFixed(2)} ms`);
}
if (ohaSuccessRate !== undefined) {
  console.log(`  success rate: ${(ohaSuccessRate * 100).toFixed(2)} %`);
}
if (ohaTotalData !== undefined) {
  console.log(`  total data:   ${formatBytes(ohaTotalData)}`);
}
console.log("");
console.log("HTTP server (worker)");
if (workerHttpStats) {
  console.log(`  requests:     ${workerHttpStats.requests}`);
  console.log(`  bytes out:    ${formatBytes(workerHttpStats.bytesOut)}`);
  console.log(`  uptime:       ${workerHttpStats.uptimeSec.toFixed(3)} s`);
} else {
  console.log("  stats:        unavailable");
}
console.log("");

const report = {
  runtime: {
    workerRuntime: options.workerRuntime,
    workerBinary: options.workerBinary,
    workerArgs: options.workerArgs,
  },
  socket: socketStats,
  http: {
    oha: ohaJson,
    server: workerHttpStats ?? null,
  },
  logs: {
    echo: echoOutput,
    worker: workerOutput,
    ohaStderr: ohaErrorRaw || null,
  },
};

if (options.jsonOutPath) {
  await Deno.writeTextFile(
    options.jsonOutPath,
    `${JSON.stringify(report, null, 2)}\n`,
  );
  console.log(`json report:  ${options.jsonOutPath}`);
}

if (options.printJson) {
  console.log(`BENCHMARK_REPORT ${JSON.stringify(report)}`);
}
