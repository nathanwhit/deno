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
// - https://github.com/nodejs/node/blob/master/src/connection_wrap.cc
// - https://github.com/nodejs/node/blob/master/src/connection_wrap.h

import { primordials } from "ext:core/mod.js";
const { FunctionPrototypeCall } = primordials;

import { ConnectionWrap as RustConnectionWrap } from "ext:core/ops";
import {
  initLibuvStreamWrapState,
  kStreamBaseField,
  LibuvStreamWrap,
} from "ext:deno_node/internal_binding/stream_wrap.ts";

export class ConnectionWrap extends RustConnectionWrap {
  constructor(provider: number, object?: unknown) {
    super(provider, null);
    initLibuvStreamWrapState(this as unknown as { reading?: boolean });
    const self = this as unknown as {
      [kStreamBaseField]?: unknown;
    };
    self[kStreamBaseField] = object;
  }

  readStart(): number {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype.readStart, this);
  }

  readStop(): number {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype.readStop, this);
  }

  shutdown(req: unknown): number {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype.shutdown, this, req);
  }

  useUserBuffer(userBuf: unknown): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.useUserBuffer,
      this,
      userBuf,
    );
  }

  writeBuffer(req: unknown, data: Uint8Array): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writeBuffer,
      this,
      req,
      data,
    );
  }

  protected _read(): Promise<void> {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype._read, this);
  }

  protected _write(req: unknown, data: Uint8Array): Promise<void> {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype._write,
      this,
      req,
      data,
    );
  }

  writev(
    req: unknown,
    chunks: unknown[],
    allBuffers: boolean,
  ): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writev,
      this,
      req,
      chunks,
      allBuffers,
    );
  }

  writeAsciiString(req: unknown, data: string): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writeAsciiString,
      this,
      req,
      data,
    );
  }

  writeUtf8String(req: unknown, data: string): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writeUtf8String,
      this,
      req,
      data,
    );
  }

  writeUcs2String(req: unknown, data: string): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writeUcs2String,
      this,
      req,
      data,
    );
  }

  writeLatin1String(req: unknown, data: string): number {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype.writeLatin1String,
      this,
      req,
      data,
    );
  }

  override _onClose(): number {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype._onClose, this);
  }
}
