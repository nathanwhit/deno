// Benchmark: Node.js setTimeout / setInterval performance
// Run with: deno run bench_timers.mjs
//       or: node bench_timers.mjs
import {
  clearInterval,
  clearTimeout,
  setInterval,
  setTimeout,
} from "node:timers";

function bench(name, fn) {
  return new Promise((resolve) => {
    fn((ops, elapsed) => {
      const opsPerSec = ((ops / elapsed) * 1000).toFixed(0);
      console.log(
        `${name}: ${ops} ops in ${elapsed.toFixed(1)}ms (${opsPerSec} ops/sec)`,
      );
      resolve();
    });
  });
}

// 1) Sequential 0ms timer chain - measures per-tick overhead
await bench("fire-0ms-chain", (done) => {
  const target = 200;
  let count = 0;
  const start = performance.now();
  function next() {
    if (++count >= target) {
      done(count, performance.now() - start);
      return;
    }
    setTimeout(next, 0);
  }
  setTimeout(next, 0);
});

// 2) Burst: schedule many timers at once, all fire on same tick
await bench("burst", (done) => {
  const N = 200;
  let remaining = N;
  const start = performance.now();
  for (let i = 0; i < N; i++) {
    setTimeout(() => {
      if (--remaining === 0) {
        done(N, performance.now() - start);
      }
    }, 1);
  }
});

// 3) Burst with staggered delays (0-9ms)
await bench("burst-staggered", (done) => {
  const N = 200;
  let remaining = N;
  const start = performance.now();
  for (let i = 0; i < N; i++) {
    setTimeout(() => {
      if (--remaining === 0) {
        done(N, performance.now() - start);
      }
    }, i % 10);
  }
});

// 4) Create + cancel: pure sync overhead, no event loop ticks needed
await bench("create-cancel", (done) => {
  const N = 10_000;
  const start = performance.now();
  for (let i = 0; i < N; i++) {
    clearTimeout(setTimeout(() => {}, 1000));
  }
  done(N, performance.now() - start);
});

// 5) setInterval throughput
await bench("interval-ticks", (done) => {
  const target = 200;
  let count = 0;
  const start = performance.now();
  const id = setInterval(() => {
    if (++count >= target) {
      clearInterval(id);
      done(count, performance.now() - start);
    }
  }, 0);
});

// 6) Refresh: reuse same timer object repeatedly
await bench("refresh-chain", (done) => {
  const target = 200;
  let count = 0;
  const start = performance.now();
  const t = setTimeout(fire, 0);
  function fire() {
    if (++count >= target) {
      done(count, performance.now() - start);
      return;
    }
    t.refresh();
  }
});

// 7) Mixed concurrent timers with random delays 0-20ms
await bench("mixed-concurrent", (done) => {
  const N = 200;
  let remaining = N;
  const start = performance.now();
  for (let i = 0; i < N; i++) {
    const delay = Math.floor(Math.random() * 20);
    setTimeout(() => {
      if (--remaining === 0) {
        done(N, performance.now() - start);
      }
    }, delay);
  }
});

// 8) Unref/ref churn - pure sync overhead
await bench("unref-ref", (done) => {
  const N = 10_000;
  const start = performance.now();
  const t = setTimeout(() => {
    done(N, performance.now() - start);
  }, 1);
  for (let i = 0; i < N; i++) {
    t.unref();
    t.ref();
  }
});
