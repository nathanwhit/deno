// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and other Node contributors.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.

// This module ports:
// - https://github.com/nodejs/node/blob/master/src/stream_base-inl.h
// - https://github.com/nodejs/node/blob/master/src/stream_base.h
// - https://github.com/nodejs/node/blob/master/src/stream_base.cc
// - https://github.com/nodejs/node/blob/master/src/stream_wrap.h
// - https://github.com/nodejs/node/blob/master/src/stream_wrap.cc

import { primordials } from "ext:core/mod.js";
const {
  Array,
  Symbol,
  Uint8Array,
} = primordials;

import { TextEncoder } from "ext:deno_web/08_text_encoding.js";
import { Buffer } from "node:buffer";
import { HandleWrap } from "ext:deno_node/internal_binding/handle_wrap.ts";
import {
  AsyncWrap,
  providerType,
} from "ext:deno_node/internal_binding/async_wrap.ts";

// Re-export the Rust-backed LibuvStreamWrap cppgc class.
// Read/write/shutdown are handled in Rust; string encoding helpers are
// added to the prototype below.
import { LibuvStreamWrap } from "ext:core/ops";
export { LibuvStreamWrap };

export interface Reader {
  read(p: Uint8Array): Promise<number | null>;
}

export interface Writer {
  write(p: Uint8Array): Promise<number>;
}

export interface Closer {
  close(): void;
}

export interface Ref {
  ref(): void;
  unref(): void;
}

export interface StreamBase extends Reader, Writer, Closer, Ref {}

const enum StreamBaseStateFields {
  kReadBytesOrError,
  kArrayBufferOffset,
  kBytesWritten,
  kLastWriteWasAsync,
  kNumStreamBaseStateFields,
}

export const kReadBytesOrError = StreamBaseStateFields.kReadBytesOrError;
export const kArrayBufferOffset = StreamBaseStateFields.kArrayBufferOffset;
export const kBytesWritten = StreamBaseStateFields.kBytesWritten;
export const kLastWriteWasAsync = StreamBaseStateFields.kLastWriteWasAsync;
export const kNumStreamBaseStateFields =
  StreamBaseStateFields.kNumStreamBaseStateFields;

export const streamBaseState = new Uint8Array(5);

// This is Deno, it always will be async.
streamBaseState[kLastWriteWasAsync] = 1;

export class WriteWrap<H extends HandleWrap> extends AsyncWrap {
  handle!: H;
  oncomplete!: (status: number) => void;
  async!: boolean;
  bytes!: number;
  buffer!: unknown;
  callback!: unknown;
  _chunks!: unknown[];

  constructor() {
    super(providerType.WRITEWRAP);
  }
}

export class ShutdownWrap<H extends HandleWrap> extends AsyncWrap {
  handle!: H;
  oncomplete!: (status: number) => void;
  callback!: () => void;

  constructor() {
    super(providerType.SHUTDOWNWRAP);
  }
}

export const kStreamBaseField = Symbol("kStreamBaseField");

// ---------------------------------------------------------------------------
// JS-side write helpers - encode strings then delegate to the Rust
// writeBuffer method.
// ---------------------------------------------------------------------------

const encoder = new TextEncoder();

LibuvStreamWrap.prototype.writeAsciiString = function (
  req: WriteWrap<LibuvStreamWrap>,
  data: string,
): number {
  return this.writeBuffer(req, encoder.encode(data));
};

LibuvStreamWrap.prototype.writeUtf8String = function (
  req: WriteWrap<LibuvStreamWrap>,
  data: string,
): number {
  return this.writeBuffer(req, encoder.encode(data));
};

LibuvStreamWrap.prototype.writeLatin1String = function (
  req: WriteWrap<LibuvStreamWrap>,
  data: string,
): number {
  return this.writeBuffer(req, Buffer.from(data, "latin1"));
};

LibuvStreamWrap.prototype.writeUcs2String = function (
  _req: WriteWrap<LibuvStreamWrap>,
  _data: string,
): number {
  throw new Error("Not implemented: LibuvStreamWrap.prototype.writeUcs2String");
};

LibuvStreamWrap.prototype.writev = function (
  req: WriteWrap<LibuvStreamWrap>,
  chunks: Buffer[] | (string | Buffer)[],
  allBuffers: boolean,
): number {
  const count = allBuffers ? chunks.length : chunks.length >> 1;
  const buffers: Buffer[] = new Array(count);

  if (!allBuffers) {
    for (let i = 0; i < count; i++) {
      const chunk = chunks[i * 2];

      if (Buffer.isBuffer(chunk)) {
        buffers[i] = chunk;
      }

      // String chunk
      const encoding = chunks[i * 2 + 1] as string;
      // deno-lint-ignore no-explicit-any
      buffers[i] = Buffer.from(chunk as string, encoding as any);
    }
  } else {
    for (let i = 0; i < count; i++) {
      buffers[i] = chunks[i] as Buffer;
    }
  }

  // deno-lint-ignore prefer-primordials
  return this.writeBuffer(req, Buffer.concat(buffers));
};
