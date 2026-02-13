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

import { core, primordials } from "ext:core/mod.js";
const {
  Array,
  MapPrototypeGet,
  ObjectPrototypeIsPrototypeOf,
  PromiseResolve,
  Symbol,
  TypedArrayPrototypeSlice,
  Uint8Array,
  Uint8ArrayPrototype,
} = primordials;

import { TextEncoder } from "ext:deno_web/08_text_encoding.js";
import { Buffer } from "node:buffer";
import { notImplemented } from "ext:deno_node/_utils.ts";
import { HandleWrap } from "ext:deno_node/internal_binding/handle_wrap.ts";
import { ownerSymbol } from "ext:deno_node/internal/async_hooks.ts";
import {
  AsyncWrap,
  providerType,
} from "ext:deno_node/internal_binding/async_wrap.ts";
import { codeMap } from "ext:deno_node/internal_binding/uv.ts";
import { _readWithCancelHandle } from "ext:deno_io/12_io.js";
import { NodeTypeError } from "ext:deno_node/internal/errors.ts";

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

function addIfWritable(
  target: object,
  key: "bytesRead" | "bytesWritten",
  delta: number,
) {
  try {
    const value = (target as { [k: string]: number })[key] ?? 0;
    (target as { [k: string]: number })[key] = value + delta;
  } catch {
    // Native-backed wrappers may expose getter-only fields.
  }
}

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
const kReadingField = Symbol("kReadingField");
const kBufferField = Symbol("kBufferField");

const SUGGESTED_SIZE = 64 * 1024;

export function initLibuvStreamWrapState(target: {
  [kReadingField]?: boolean;
  [kBufferField]?: Uint8Array;
  reading?: boolean;
}) {
  target[kReadingField] = false;
  target[kBufferField] = new Uint8Array(SUGGESTED_SIZE);
  target.reading = false;
}

export class LibuvStreamWrap extends HandleWrap {
  [kStreamBaseField]?: Reader & Writer & Closer & Ref;
  [kReadingField] = false;
  [kBufferField] = new Uint8Array(SUGGESTED_SIZE);

  reading = false;
  destroyed = false;
  writeQueueSize = 0;
  bytesRead = 0;
  bytesWritten = 0;

  onread!: (_arrayBuffer: Uint8Array, _nread: number) => Uint8Array | undefined;

  constructor(
    provider: providerType,
    stream?: Reader & Writer & Closer & Ref,
  ) {
    super(provider, null);
    initLibuvStreamWrapState(this);
    this.attachToObject(stream);
  }

  /**
   * Start the reading of the stream.
   * @return An error status code.
   */
  readStart(): number {
    if (!this[kReadingField]) {
      this[kReadingField] = true;
      this.reading = true;
      this._read();
    }

    return 0;
  }

  /**
   * Stop the reading of the stream.
   * @return An error status code.
   */
  readStop(): number {
    this[kReadingField] = false;
    this.reading = false;
    if (this.cancelHandle) {
      core.close(this.cancelHandle);
      this.cancelHandle = undefined;
    }

    return 0;
  }

  /**
   * Shutdown the stream.
   * @param req A shutdown request wrapper.
   * @return An error status code.
   */
  shutdown(req: ShutdownWrap<LibuvStreamWrap>): number {
    const status = this._onClose();

    try {
      req.oncomplete(status);
    } catch {
      // swallow callback error.
    }

    return 0;
  }

  /**
   * @param userBuf
   * @return An error status code.
   */
  useUserBuffer(_userBuf: unknown): number {
    // TODO(cmorten)
    notImplemented("LibuvStreamWrap.prototype.useUserBuffer");
  }

  /**
   * Write a buffer to the stream.
   * @param req A write request wrapper.
   * @param data The Uint8Array buffer to write to the stream.
   * @return An error status code.
   */
  writeBuffer(req: WriteWrap<LibuvStreamWrap>, data: Uint8Array): number {
    if (!ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, data)) {
      throw new NodeTypeError(
        "ERR_INVALID_ARG_TYPE",
        "Second argument must be a buffer",
      );
    }

    this._write(req, data);

    return 0;
  }

  /**
   * Write multiple chunks at once.
   * @param req A write request wrapper.
   * @param chunks
   * @param allBuffers
   * @return An error status code.
   */
  writev(
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
        const encoding: string = chunks[i * 2 + 1] as string;
        buffers[i] = Buffer.from(chunk as string, encoding);
      }
    } else {
      for (let i = 0; i < count; i++) {
        buffers[i] = chunks[i] as Buffer;
      }
    }

    // Ignoring primordial lint here since the static method `concat` is invoked
    // via the Node.js `Buffer` class instead of a JS builtin.
    // deno-lint-ignore prefer-primordials
    return this.writeBuffer(req, Buffer.concat(buffers));
  }

  /**
   * Write an ASCII string to the stream.
   * @return An error status code.
   */
  writeAsciiString(req: WriteWrap<LibuvStreamWrap>, data: string): number {
    const buffer = new TextEncoder().encode(data);

    return this.writeBuffer(req, buffer);
  }

  /**
   * Write an UTF8 string to the stream.
   * @return An error status code.
   */
  writeUtf8String(req: WriteWrap<LibuvStreamWrap>, data: string): number {
    const buffer = new TextEncoder().encode(data);

    return this.writeBuffer(req, buffer);
  }

  /**
   * Write an UCS2 string to the stream.
   * @return An error status code.
   */
  writeUcs2String(_req: WriteWrap<LibuvStreamWrap>, _data: string): number {
    notImplemented("LibuvStreamWrap.prototype.writeUcs2String");
  }

  /**
   * Write an LATIN1 string to the stream.
   * @return An error status code.
   */
  writeLatin1String(req: WriteWrap<LibuvStreamWrap>, data: string): number {
    const buffer = Buffer.from(data, "latin1");
    return this.writeBuffer(req, buffer);
  }

  override _onClose(): number {
    let status = 0;
    this[kReadingField] = false;
    this.reading = false;

    try {
      this[kStreamBaseField]?.close();
    } catch {
      status = MapPrototypeGet(codeMap, "ENOTCONN")!;
    }

    return status;
  }

  /**
   * Attaches the class to the underlying stream.
   * @param stream The stream to attach to.
   */
  protected attachToObject(stream?: Reader & Writer & Closer & Ref) {
    this[kStreamBaseField] = stream;
  }

  /** Internal method for reading from the attached stream. */
  protected async _read() {
    // Queue the read operation and allow TLS upgrades to complete.
    //
    // This is done to ensure that the resource is not locked up by
    // op_read.
    await PromiseResolve();

    const stream = this[kStreamBaseField];
    if (!stream) return;

    let buf = this[kBufferField];
    if (!buf) {
      buf = new Uint8Array(SUGGESTED_SIZE);
      this[kBufferField] = buf;
    }

    let nread: number | null;

    if (this.upgrading) {
      // Starting an upgrade, stop reading. Upgrading will resume reading.
      this.readStop();
      return;
    }

    const streamBefore = stream;
    try {
      const readWithCancelHandle = (stream as {
        [_readWithCancelHandle]?: (
          buffer: Uint8Array,
        ) => { cancelHandle?: number; nread: Promise<number | null> };
      })[_readWithCancelHandle];
      if (readWithCancelHandle) {
        const { cancelHandle, nread: p } = readWithCancelHandle(buf);
        if (cancelHandle) {
          this.cancelHandle = cancelHandle;
        }

        nread = await p;
      } else {
        nread = await stream.read(buf);
      }
    } catch (e) {
      // Try to read again if the underlying stream resource
      // changed. This can happen during TLS upgrades (eg. STARTTLS)
      if (streamBefore !== this[kStreamBaseField]) {
        return this._read();
      }

      if ((e as { message?: string }).message === "cancelled") return null;

      if (
        ObjectPrototypeIsPrototypeOf(Deno.errors.Interrupted.prototype, e) ||
        ObjectPrototypeIsPrototypeOf(Deno.errors.BadResource.prototype, e)
      ) {
        nread = MapPrototypeGet(codeMap, "EOF")!;
      } else if (
        ObjectPrototypeIsPrototypeOf(
          Deno.errors.ConnectionReset.prototype,
          e,
        ) ||
        ObjectPrototypeIsPrototypeOf(Deno.errors.ConnectionAborted.prototype, e)
      ) {
        nread = MapPrototypeGet(codeMap, "ECONNRESET")!;
      } else {
        this[ownerSymbol].destroy(e);
        return;
      }
    }

    nread ??= MapPrototypeGet(codeMap, "EOF")!;

    streamBaseState[kReadBytesOrError] = nread;

    if (nread > 0) {
      addIfWritable(this, "bytesRead", nread);
    }

    buf = TypedArrayPrototypeSlice(buf, 0, nread);

    streamBaseState[kArrayBufferOffset] = 0;

    try {
      this.onread!(buf, nread);
    } catch {
      // swallow callback errors.
    }

    if (nread >= 0 && this[kReadingField]) {
      this._read();
    }
  }

  /**
   * Internal method for writing to the attached stream.
   * @param req A write request wrapper.
   * @param data The Uint8Array buffer to write to the stream.
   */
  protected async _write(req: WriteWrap<LibuvStreamWrap>, data: Uint8Array) {
    const { byteLength } = data;

    const stream = this[kStreamBaseField];
    if (!stream) {
      try {
        req.oncomplete(MapPrototypeGet(codeMap, "EBADF")!);
      } catch {
        // swallow callback errors.
      }
      return;
    }
    const streamBefore = stream;

    if (this.upgrading) {
      // There is an upgrade in progress, queue the write request.
      await this.upgrading;
    }

    let nwritten = 0;
    try {
      // TODO(crowlKats): duplicate from runtime/js/13_buffer.js
      while (nwritten < data.length) {
        nwritten += await stream.write(
          data.subarray(nwritten),
        );
      }
    } catch (e) {
      // Try to read again if the underlying stream resource
      // changed. This can happen during TLS upgrades (eg. STARTTLS)
      if (streamBefore !== this[kStreamBaseField]) {
        return this._write(req, data.subarray(nwritten));
      }

      let status: number;
      // TODO(cmorten): map err to status codes
      if (
        ObjectPrototypeIsPrototypeOf(Deno.errors.BadResource.prototype, e) ||
        ObjectPrototypeIsPrototypeOf(Deno.errors.BrokenPipe.prototype, e)
      ) {
        status = MapPrototypeGet(codeMap, "EBADF")!;
      } else {
        status = MapPrototypeGet(codeMap, "UNKNOWN")!;
      }

      try {
        req.oncomplete(status);
      } catch {
        // swallow callback errors.
      }

      return;
    }

    streamBaseState[kBytesWritten] = byteLength;
    addIfWritable(this, "bytesWritten", byteLength);

    try {
      req.oncomplete(0);
    } catch {
      // swallow callback errors.
    }

    return;
  }
}
