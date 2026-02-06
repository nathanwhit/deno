type Runtime = "deno" | "node" | "bun";

type Candidate = {
  label: string;
  runtime: Runtime;
  binary: string;
};

type Options = {
  candidates: Candidate[];
  trials: number;
  orchestratorBinary: string;
  orchestratorArgs: string[];
  orchestratorPath: string;
  passthroughArgs: string[];
  printJson: boolean;
  jsonOutPath?: string;
};

type TrialMetrics = {
  socketThroughputMiBPerSec: number;
  socketDurationSec: number;
  httpReqPerSec?: number;
  httpAvgLatencyMs?: number;
  httpP95LatencyMs?: number;
  httpSuccessRatePct?: number;
};

type CandidateResult = {
  candidate: Candidate;
  trials: TrialMetrics[];
};

function parseRuntime(value: string): Runtime {
  if (value === "deno" || value === "node" || value === "bun") return value;
  throw new Error(`Invalid runtime "${value}". Expected one of: deno, node, bun`);
}

function basename(path: string): string {
  const normalized = path.replaceAll("\\", "/");
  const idx = normalized.lastIndexOf("/");
  return idx === -1 ? normalized : normalized.slice(idx + 1);
}

function parseCandidateSpec(spec: string): Candidate {
  const eqIdx = spec.indexOf("=");
  const label = eqIdx >= 0 ? spec.slice(0, eqIdx).trim() : "";
  const body = eqIdx >= 0 ? spec.slice(eqIdx + 1).trim() : spec.trim();
  if (!body) {
    throw new Error(`Invalid --candidate "${spec}"`);
  }

  const atIdx = body.indexOf("@");
  const runtimePart = atIdx >= 0 ? body.slice(0, atIdx).trim() : body;
  const binaryPart = atIdx >= 0 ? body.slice(atIdx + 1).trim() : runtimePart;

  const runtime = parseRuntime(runtimePart);
  if (!binaryPart) {
    throw new Error(`Invalid --candidate "${spec}": missing binary`);
  }

  const resolvedLabel = label || (binaryPart === runtime ? runtime : `${runtime}:${basename(binaryPart)}`);
  return {
    label: resolvedLabel,
    runtime,
    binary: binaryPart,
  };
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    candidates: [],
    trials: 3,
    orchestratorBinary: "deno",
    orchestratorArgs: [],
    orchestratorPath: new URL("./benchmark-orchestrator.ts", import.meta.url).pathname,
    passthroughArgs: [],
    printJson: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = argv[i + 1];

    if (arg === "--") {
      options.passthroughArgs.push(...argv.slice(i + 1));
      break;
    }
    if (arg === "--candidate" && next) {
      options.candidates.push(parseCandidateSpec(next));
      i += 1;
      continue;
    }
    if (arg === "--trials" && next) {
      options.trials = Number.parseInt(next, 10);
      i += 1;
      continue;
    }
    if (arg === "--orchestrator-binary" && next) {
      options.orchestratorBinary = next;
      i += 1;
      continue;
    }
    if (arg === "--orchestrator-arg" && next) {
      options.orchestratorArgs.push(next);
      i += 1;
      continue;
    }
    if (arg === "--orchestrator-path" && next) {
      options.orchestratorPath = next;
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
    if (arg === "--help" || arg === "-h") {
      printUsage();
      Deno.exit(0);
    }

    options.passthroughArgs.push(arg);
  }

  if (options.candidates.length === 0) {
    options.candidates.push(
      { label: "deno", runtime: "deno", binary: "deno" },
      { label: "node", runtime: "node", binary: "node" },
      { label: "bun", runtime: "bun", binary: "bun" },
    );
  }

  if (!Number.isFinite(options.trials) || options.trials <= 0) {
    throw new Error(`Invalid --trials: ${options.trials}`);
  }
  if (options.orchestratorBinary.trim().length === 0) {
    throw new Error("--orchestrator-binary cannot be empty");
  }

  const labels = new Set<string>();
  for (const candidate of options.candidates) {
    if (labels.has(candidate.label)) {
      throw new Error(`Duplicate candidate label: ${candidate.label}`);
    }
    labels.add(candidate.label);
  }

  return options;
}

function printUsage(): void {
  console.log("Usage: deno run -A compare-runtimes.ts [options] [-- benchmark-orchestrator-args]");
  console.log("");
  console.log("Comparison options:");
  console.log("  --candidate <label=runtime@binary>  Candidate to benchmark (repeatable)");
  console.log("                                       runtime: deno | node | bun");
  console.log("                                       examples:");
  console.log("                                       --candidate deno=deno@deno");
  console.log("                                       --candidate deno2=deno@../../deno2/target/release-lite/deno");
  console.log("                                       --candidate node=node@node");
  console.log("                                       --candidate bun=bun@bun");
  console.log("  --trials <n>                         Trials per candidate (default: 3)");
  console.log("  --print-json                         Print full COMPARISON_REPORT JSON");
  console.log("  --json-out <path>                    Write COMPARISON_REPORT JSON to file");
  console.log("");
  console.log("Orchestrator runner:");
  console.log("  --orchestrator-binary <cmd>          Runner command (default: deno)");
  console.log("  --orchestrator-arg <arg>             Extra arg to runner (repeatable)");
  console.log("  --orchestrator-path <path>           Path to benchmark-orchestrator.ts");
  console.log("");
  console.log("Any unrecognized args are forwarded to benchmark-orchestrator.ts.");
  console.log("Use '--' to separate forwarded args explicitly.");
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

function extractReportLine(stdout: string): string {
  const lines = stdout.trim().split(/\r?\n/);
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const line = lines[i];
    if (line.startsWith("BENCHMARK_REPORT ")) {
      return line.slice("BENCHMARK_REPORT ".length);
    }
  }
  throw new Error("Missing BENCHMARK_REPORT line from orchestrator output");
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

function isBuildProgressLine(line: string): boolean {
  const trimmed = line.trim();
  if (
    trimmed.startsWith("[echo] starting Rust echo server") ||
    trimmed.startsWith("[echo] cargo run still starting")
  ) {
    return true;
  }

  const withoutEchoPrefix = trimmed.replace(/^\[echo:err\]\s*/, "");
  return withoutEchoPrefix.startsWith("Compiling ") ||
    withoutEchoPrefix.startsWith("Finished ") ||
    withoutEchoPrefix.startsWith("Checking ") ||
    withoutEchoPrefix.startsWith("Blocking waiting for file lock") ||
    withoutEchoPrefix.startsWith("Updating ") ||
    withoutEchoPrefix.startsWith("Downloading ") ||
    withoutEchoPrefix.startsWith("Downloaded ") ||
    withoutEchoPrefix.startsWith("warning:") ||
    withoutEchoPrefix.startsWith("error:");
}

function toTrialMetrics(report: Record<string, unknown>): TrialMetrics {
  const socket = (report.socket && typeof report.socket === "object")
    ? report.socket as Record<string, unknown>
    : {};
  const http = (report.http && typeof report.http === "object")
    ? report.http as Record<string, unknown>
    : {};
  const oha = (http.oha && typeof http.oha === "object")
    ? http.oha as Record<string, unknown>
    : {};
  const summary = (oha.summary && typeof oha.summary === "object")
    ? oha.summary as Record<string, unknown>
    : {};
  const latency = (oha.latencyPercentiles && typeof oha.latencyPercentiles === "object")
    ? oha.latencyPercentiles as Record<string, unknown>
    : {};

  const socketThroughputMiBPerSec = pickNumber(socket, ["throughputMiBPerSec"]);
  const socketDurationSec = pickNumber(socket, ["durationSec"]);
  if (socketThroughputMiBPerSec === undefined || socketDurationSec === undefined) {
    throw new Error("Benchmark report missing socket throughput/duration");
  }

  const httpReqPerSec = pickNumber(summary, ["requestsPerSec", "requests_per_sec"]);
  const httpAvgLatencySec = pickNumber(summary, ["average"]);
  const httpP95Sec = pickNumber(latency, ["p95", "95"]);
  const httpSuccessRate = pickNumber(summary, ["successRate", "success_rate"]);

  return {
    socketThroughputMiBPerSec,
    socketDurationSec,
    httpReqPerSec,
    httpAvgLatencyMs: httpAvgLatencySec !== undefined ? httpAvgLatencySec * 1000 : undefined,
    httpP95LatencyMs: httpP95Sec !== undefined ? httpP95Sec * 1000 : undefined,
    httpSuccessRatePct: httpSuccessRate !== undefined ? httpSuccessRate * 100 : undefined,
  };
}

function formatNumber(value: number | undefined, digits = 2): string {
  return value === undefined ? "n/a" : value.toFixed(digits);
}

function aggregate(values: number[]): { mean: number; min: number; max: number } | undefined {
  if (values.length === 0) return undefined;
  let sum = 0;
  let min = values[0];
  let max = values[0];
  for (const value of values) {
    sum += value;
    if (value < min) min = value;
    if (value > max) max = value;
  }
  return { mean: sum / values.length, min, max };
}

function metricBlock(name: string, values: number[], digits = 2): string {
  const stats = aggregate(values);
  if (!stats) return `  ${name}: n/a`;
  return `  ${name}: mean ${stats.mean.toFixed(digits)} (min ${stats.min.toFixed(digits)}, max ${stats.max.toFixed(digits)})`;
}

async function runTrial(
  options: Options,
  candidate: Candidate,
  trial: number,
): Promise<{ metrics: TrialMetrics; report: Record<string, unknown> }> {
  const orchestratorArgs = [
    ...options.orchestratorArgs,
    "run",
    "-A",
    options.orchestratorPath,
    "--worker-runtime",
    candidate.runtime,
    "--worker-binary",
    candidate.binary,
    "--print-json",
    ...options.passthroughArgs,
  ];

  const trialTag = `${candidate.label} ${trial}/${options.trials}`;
  console.log(`[${trialTag}] starting`);
  const process = new Deno.Command(options.orchestratorBinary, {
    args: orchestratorArgs,
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const stdoutLines: string[] = [];
  const stderrLines: string[] = [];
  const startedAt = Date.now();
  let echoReady = false;
  const heartbeat = setInterval(() => {
    if (echoReady) return;
    const elapsedSec = ((Date.now() - startedAt) / 1000).toFixed(1);
    console.log(
      `[${trialTag}] waiting on Rust echo build/startup (${elapsedSec}s elapsed)`,
    );
  }, 5000);

  const stdoutTask = consumeLines(process.stdout, (line) => {
    stdoutLines.push(line);
    if (line.startsWith("[echo] ready on ")) {
      echoReady = true;
    }
    if (line.startsWith("BENCHMARK_REPORT ") || !isBuildProgressLine(line)) return;
    console.log(`[${trialTag}] ${line}`);
  });
  const stderrTask = consumeLines(process.stderr, (line) => {
    stderrLines.push(line);
    if (!isBuildProgressLine(line)) return;
    console.error(`[${trialTag}:err] ${line}`);
  });

  let status: Deno.CommandStatus;
  try {
    status = await process.status;
    await Promise.all([stdoutTask, stderrTask]);
  } finally {
    clearInterval(heartbeat);
  }

  const stdout = `${stdoutLines.join("\n")}\n`;
  const stderr = `${stderrLines.join("\n")}\n`;
  if (!status.success) {
    throw new Error(
      `orchestrator failed for ${candidate.label} trial ${trial} (exit ${status.code})\nstdout:\n${stdout}\nstderr:\n${stderr}`,
    );
  }

  const jsonLine = extractReportLine(stdout);
  let report: Record<string, unknown>;
  try {
    report = JSON.parse(jsonLine);
  } catch (error) {
    throw new Error(`Failed parsing BENCHMARK_REPORT JSON: ${(error as Error).message}`);
  }

  const metrics = toTrialMetrics(report);
  console.log(
    `[${candidate.label}] trial ${trial}/${options.trials} done: socket ${metrics.socketThroughputMiBPerSec.toFixed(2)} MiB/s, http ${formatNumber(metrics.httpReqPerSec, 2)} req/s, p95 ${formatNumber(metrics.httpP95LatencyMs, 2)} ms`,
  );

  return { metrics, report };
}

const options = parseArgs(Deno.args);
const allResults: CandidateResult[] = [];
const rawReports: Record<string, unknown[]> = {};

console.log("=== Runtime Comparison ===");
console.log(`candidates: ${options.candidates.map((c) => c.label).join(", ")}`);
console.log(`trials per candidate: ${options.trials}`);
if (options.passthroughArgs.length > 0) {
  console.log(`forwarded args: ${options.passthroughArgs.join(" ")}`);
}
console.log("");

for (const candidate of options.candidates) {
  const candidateTrials: TrialMetrics[] = [];
  const reports: Record<string, unknown>[] = [];

  for (let trial = 1; trial <= options.trials; trial += 1) {
    const { metrics, report } = await runTrial(options, candidate, trial);
    candidateTrials.push(metrics);
    reports.push(report);
  }

  allResults.push({ candidate, trials: candidateTrials });
  rawReports[candidate.label] = reports;

  const socketThroughputs = candidateTrials.map((m) => m.socketThroughputMiBPerSec);
  const socketDurations = candidateTrials.map((m) => m.socketDurationSec);
  const httpReqPerSec = candidateTrials.flatMap((m) => m.httpReqPerSec === undefined ? [] : [m.httpReqPerSec]);
  const httpAvgLatency = candidateTrials.flatMap((m) =>
    m.httpAvgLatencyMs === undefined ? [] : [m.httpAvgLatencyMs]
  );
  const httpP95Latency = candidateTrials.flatMap((m) =>
    m.httpP95LatencyMs === undefined ? [] : [m.httpP95LatencyMs]
  );
  const httpSuccessRate = candidateTrials.flatMap((m) =>
    m.httpSuccessRatePct === undefined ? [] : [m.httpSuccessRatePct]
  );

  console.log("");
  console.log(`${candidate.label} (${candidate.runtime} :: ${candidate.binary})`);
  console.log(metricBlock("socket throughput MiB/s", socketThroughputs));
  console.log(metricBlock("socket duration s", socketDurations, 3));
  console.log(metricBlock("http req/s", httpReqPerSec));
  console.log(metricBlock("http avg latency ms", httpAvgLatency));
  console.log(metricBlock("http p95 latency ms", httpP95Latency));
  console.log(metricBlock("http success %", httpSuccessRate));
  console.log("");
}

const ranking = [...allResults]
  .map((result) => {
    const reqs = result.trials.flatMap((m) => m.httpReqPerSec === undefined ? [] : [m.httpReqPerSec]);
    const socket = result.trials.map((m) => m.socketThroughputMiBPerSec);
    return {
      label: result.candidate.label,
      runtime: result.candidate.runtime,
      binary: result.candidate.binary,
      httpReqPerSecMean: aggregate(reqs)?.mean,
      socketMiBPerSecMean: aggregate(socket)?.mean,
    };
  })
  .sort((a, b) => (b.httpReqPerSecMean ?? -Infinity) - (a.httpReqPerSecMean ?? -Infinity));

console.log("Ranking by HTTP req/s mean");
for (let i = 0; i < ranking.length; i += 1) {
  const row = ranking[i];
  console.log(
    `${i + 1}. ${row.label} (${row.runtime})  req/s=${formatNumber(row.httpReqPerSecMean, 2)}  socket=${formatNumber(row.socketMiBPerSecMean, 2)} MiB/s`,
  );
}

const comparisonReport = {
  candidates: options.candidates,
  trials: options.trials,
  forwardedArgs: options.passthroughArgs,
  results: allResults,
  ranking,
  rawReports,
};

if (options.jsonOutPath) {
  await Deno.writeTextFile(options.jsonOutPath, `${JSON.stringify(comparisonReport, null, 2)}\n`);
  console.log("");
  console.log(`json report: ${options.jsonOutPath}`);
}

if (options.printJson) {
  console.log(`COMPARISON_REPORT ${JSON.stringify(comparisonReport)}`);
}
