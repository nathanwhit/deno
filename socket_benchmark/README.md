# Socket + http server benchmark

Benchmark that sends a bunch of data via a net.Socket to an echo server, and at the same time load tests an http server running in the same JS process.

## Setup

Install oha (`cargo install oha`)

## Run once


This will send 20GB to an echo server and read it back, while sending load to the http server via `oha`, using the `deno` binary. (The worker runtime is for permission flags and that nonsense).
```bash
deno run -A benchmark-orchestrator.ts --bytes 20g --oha-duration 10s --worker-runtime deno --worker-binary deno
```

## Compare a bunch of runtimes

This will send 20GB to an echo server and read it back, while sending load to the http server via `oha`, and compare the results
across the runtimes given.

```bash
deno run -A compare-runtimes.ts \
          --trials 3 \
          --candidate deno=deno@deno \
          --candidate deno-libuv=deno@./target/release/deno \
          --candidate node=node@node \
          --candidate bun=bun@bun \
          -- --bytes 20g --oha-duration 10s
```
