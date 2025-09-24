#!/usr/bin/env -S deno run --allow-env --allow-read --allow-write --allow-net=github.com,release-assets.githubusercontent.com

import { UntarStream } from "jsr:@std/tar/untar-stream";
import { dirname, join, normalize } from "jsr:@std/path";

import $ from "jsr:@david/dax";

const response = await fetch(
  "https://github.com/nghttp2/nghttp2/releases/download/v1.66.0/nghttp2-1.66.0.tar.gz",
);

if (!response.body) {
  throw new Error("Failed to fetch nghttp2");
}

await $`tar -xzf -C nghttp2 --strip-components=1`.stdin(response.body);

// await extractTarball(response.body, "./nghttp2");

// async function exists(path: string) {
//   try {
//     await Deno.stat(path);
//     return true;
//   } catch (error) {
//     if (error instanceof Deno.errors.NotFound) {
//       return false;
//     }
//     throw error;
//   }
// }

// async function extractTarball(
//   bytes: Uint8Array | ReadableStream<Uint8Array>,
//   dest: string,
// ) {
//   if (!(await exists(dest))) {
//     await Deno.mkdir(dest, { recursive: true });
//   }
//   const stream = bytes instanceof ReadableStream
//     ? bytes
//     : new Blob([bytes]).stream();
//   for await (
//     const entry of stream
//       .pipeThrough(new DecompressionStream("gzip"))
//       .pipeThrough(new UntarStream())
//   ) {
//     if (!entry.path.startsWith("./nghttp2-")) {
//       console.log(`Skipping ${entry.path}`);
//       entry.readable?.cancel();
//       continue;
//     }
//     const path = join(dest, normalize(entry.path));
//     await Deno.mkdir(dirname(path), { recursive: true });
//     await entry.readable?.pipeTo((await Deno.create(path)).writable);
//   }
// }
