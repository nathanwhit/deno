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
// - https://github.com/nodejs/node/blob/master/src/tcp_wrap.cc
// - https://github.com/nodejs/node/blob/master/src/tcp_wrap.h

import { primordials } from "ext:core/mod.js";
const { FunctionPrototypeCall } = primordials;
import {
  TCP as RustTCP,
  TCPConnectWrap as RustTCPConnectWrap,
} from "ext:core/ops";
import { Buffer } from "node:buffer";
import {
  initLibuvStreamWrapState,
  kStreamBaseField,
  LibuvStreamWrap,
} from "ext:deno_node/internal_binding/stream_wrap.ts";

export const constants = {
  SOCKET: 0,
  SERVER: 1,
  UV_TCP_IPV6ONLY: 1,
  UV_TCP_REUSEPORT: 2,
} as const;

const kManagedBySubclass = Symbol("kManagedBySubclass");
const kNativeUnrefed = Symbol("kNativeUnrefed");

type NativeLivenessState = unknown;

function syncNativeLivenessState(_state: NativeLivenessState) {}

function markNativeAlive(_state: NativeLivenessState) {}

function markNativeDead(_state: NativeLivenessState) {}

function writevCompat(
  self: { writeBuffer: (req: unknown, data: Uint8Array) => number },
  req: { oncomplete?: (...args: unknown[]) => unknown },
  chunks: unknown[],
  allBuffers: boolean,
): number {
  const count = allBuffers ? chunks.length : chunks.length >> 1;
  const buffers = new Array<Buffer>(count);

  if (allBuffers) {
    for (let i = 0; i < count; i++) {
      buffers[i] = chunks[i] as Buffer;
    }
  } else {
    for (let i = 0; i < count; i++) {
      const chunk = chunks[i * 2];
      if (Buffer.isBuffer(chunk)) {
        buffers[i] = chunk;
        continue;
      }
      buffers[i] = Buffer.from(
        chunk as string,
        chunks[i * 2 + 1] as BufferEncoding,
      );
    }
  }

  return self.writeBuffer(req, Buffer.concat(buffers));
}

const rustTcpPrototype = RustTCP.prototype as {
  close: (cb?: () => void) => void;
  connect: (
    req: { oncomplete?: (...args: unknown[]) => unknown },
    address: string,
    port: number,
  ) => number;
  connect6: (
    req: { oncomplete?: (...args: unknown[]) => unknown },
    address: string,
    port: number,
  ) => number;
  listen: (backlog: number) => number;
  readStart: () => number;
  ref: () => void;
  reset: (closeCallback?: () => void) => number;
  unref: () => void;
  _onClose: () => number;
  writev: (
    req: { oncomplete?: (...args: unknown[]) => unknown },
    chunks: unknown[],
    allBuffers: boolean,
  ) => number;
};

const rustClose = rustTcpPrototype.close;
const rustConnect = rustTcpPrototype.connect;
const rustConnect6 = rustTcpPrototype.connect6;
const rustListen = rustTcpPrototype.listen;
const rustReadStart = rustTcpPrototype.readStart;
const rustRef = rustTcpPrototype.ref;
const rustReset = rustTcpPrototype.reset;
const rustUnref = rustTcpPrototype.unref;
const rustOnClose = rustTcpPrototype._onClose;

rustTcpPrototype.close = function (
  this: NativeLivenessState,
  cb?: () => void,
) {
  if (!(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]) {
    markNativeDead(this);
  }
  rustClose.call(this as unknown as RustTCP, cb);
};

rustTcpPrototype.connect = function (
  this: NativeLivenessState,
  req,
  address,
  port,
) {
  const err = rustConnect.call(this as unknown as RustTCP, req, address, port);
  if (
    err === 0 &&
    !(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]
  ) {
    markNativeAlive(this);
  }
  return err;
};

rustTcpPrototype.connect6 = function (
  this: NativeLivenessState,
  req,
  address,
  port,
) {
  const err = rustConnect6.call(
    this as unknown as RustTCP,
    req,
    address,
    port,
  );
  if (
    err === 0 &&
    !(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]
  ) {
    markNativeAlive(this);
  }
  return err;
};

rustTcpPrototype.listen = function (this: NativeLivenessState, backlog: number) {
  const err = rustListen.call(this as unknown as RustTCP, backlog);
  if (
    err === 0 &&
    !(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]
  ) {
    markNativeAlive(this);
  }
  return err;
};

rustTcpPrototype.readStart = function (this: NativeLivenessState) {
  const err = rustReadStart.call(this as unknown as RustTCP);
  if (
    err === 0 &&
    !(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]
  ) {
    markNativeAlive(this);
  }
  return err;
};

rustTcpPrototype.unref = function (this: NativeLivenessState) {
  if (!(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]) {
    this[kNativeUnrefed] = true;
    syncNativeLivenessState(this);
  }
  rustUnref.call(this as unknown as RustTCP);
};

rustTcpPrototype.ref = function (this: NativeLivenessState) {
  if (!(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]) {
    this[kNativeUnrefed] = false;
    syncNativeLivenessState(this);
  }
  rustRef.call(this as unknown as RustTCP);
};

rustTcpPrototype.reset = function (
  this: NativeLivenessState,
  closeCallback?: () => void,
) {
  if (!(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]) {
    markNativeDead(this);
  }
  return rustReset.call(this as unknown as RustTCP, closeCallback);
};

rustTcpPrototype._onClose = function (this: NativeLivenessState) {
  if (!(this as NativeLivenessState & { [kManagedBySubclass]?: boolean })[kManagedBySubclass]) {
    markNativeDead(this);
  }
  return rustOnClose.call(this as unknown as RustTCP);
};

rustTcpPrototype.writev = function (
  this: { writeBuffer: (req: unknown, data: Uint8Array) => number },
  req: { oncomplete?: (...args: unknown[]) => unknown },
  chunks: unknown[],
  allBuffers: boolean,
) {
  return writevCompat(this, req, chunks, allBuffers);
};

export class TCPConnectWrap extends RustTCPConnectWrap {
  oncomplete!: (
    status: number,
    handle: TCP,
    req: TCPConnectWrap,
    readable: boolean,
    writeable: boolean,
  ) => void;
  address!: string;
  port!: number;
  localAddress!: string;
  localPort!: number;
}

export class TCP extends RustTCP {
  #isUnrefed = false;

  constructor(type: number, conn?: Deno.Conn) {
    super(type);
    (
      this as NativeLivenessState & { [kManagedBySubclass]?: boolean }
    )[kManagedBySubclass] = true;
    initLibuvStreamWrapState(this as unknown as { reading?: boolean });
    if (conn) {
      (this as TCP & { [kStreamBaseField]?: Deno.Conn })[kStreamBaseField] =
        conn;
    }
  }

  #hasStreamBaseField(): boolean {
    return (
      (this as TCP & { [kStreamBaseField]?: unknown })[kStreamBaseField] != null
    );
  }

  protected _read(): Promise<void> {
    return FunctionPrototypeCall(LibuvStreamWrap.prototype._read, this);
  }

  protected _write(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: Uint8Array,
  ): Promise<void> {
    return FunctionPrototypeCall(
      LibuvStreamWrap.prototype._write,
      this,
      req,
      data,
    );
  }

  #runReqOp<R extends { oncomplete?: (...args: unknown[]) => unknown }>(
    req: R,
    invoke: () => number,
    onComplete?: (status: number) => void,
  ): number {
    const original = req.oncomplete;
    req.oncomplete = (...args: unknown[]) => {
      const status = typeof args[0] === "number" ? args[0] as number : 0;
      onComplete?.(status);
      original?.apply(req, args);
    };
    return invoke();
  }

  #runReqOpWithUpgradeWait<
    R extends { oncomplete?: (...args: unknown[]) => unknown },
  >(
    req: R,
    invoke: () => number,
    onComplete?: (status: number) => void,
  ): number {
    const maybeUpgrading = (this as TCP & { upgrading?: unknown }).upgrading;
    if (
      maybeUpgrading &&
      typeof (maybeUpgrading as PromiseLike<unknown>).then === "function"
    ) {
      return this.#runReqOp(
        req,
        () => {
          (maybeUpgrading as PromiseLike<unknown>).then(
            () => {
              const err = invoke();
              if (err !== 0) {
                req.oncomplete?.(err);
              }
            },
            () => {
              req.oncomplete?.(-1);
            },
          );
          return 0;
        },
        onComplete,
      );
    }
    return this.#runReqOp(req, invoke, onComplete);
  }

  override listen(backlog: number): number {
    const err = super.listen(backlog);
    if (err === 0) {
      (this as TCP & { startListen?: () => number }).startListen?.();
    }
    return err;
  }

  override bind(address: string, port?: number): number {
    return super.bind(address, port ?? 0);
  }

  override bind6(address: string, port?: number, flags?: number): number {
    return super.bind6(address, port ?? 0, flags);
  }

  override connect(req: TCPConnectWrap, address: string, port: number): number {
    return this.#runReqOp(req, () => super.connect(req, address, port));
  }

  override connect6(req: TCPConnectWrap, address: string, port: number): number {
    return this.#runReqOp(req, () => super.connect6(req, address, port));
  }

  override shutdown(req: { oncomplete?: (...args: unknown[]) => unknown }): number {
    return this.#runReqOpWithUpgradeWait(req, () => {
      if (this.#hasStreamBaseField()) {
        return FunctionPrototypeCall(
          LibuvStreamWrap.prototype.shutdown,
          this,
          req,
        );
      }
      return super.shutdown(req);
    });
  }

  override writeBuffer(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: Uint8Array,
  ): number {
    return this.#runReqOpWithUpgradeWait(
      req,
      () => {
        if (this.#hasStreamBaseField()) {
          return FunctionPrototypeCall(
            LibuvStreamWrap.prototype.writeBuffer,
            this,
            req,
            data,
          );
        }
        return super.writeBuffer(req, data);
      },
    );
  }

  override writev(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    chunks: unknown[],
    allBuffers: boolean,
  ): number {
    return writevCompat(this, req, chunks, allBuffers);
  }

  override writeAsciiString(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOpWithUpgradeWait(
      req,
      () => {
        if (this.#hasStreamBaseField()) {
          return FunctionPrototypeCall(
            LibuvStreamWrap.prototype.writeAsciiString,
            this,
            req,
            data,
          );
        }
        return super.writeAsciiString(req, data);
      },
    );
  }

  override writeUtf8String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOpWithUpgradeWait(
      req,
      () => {
        if (this.#hasStreamBaseField()) {
          return FunctionPrototypeCall(
            LibuvStreamWrap.prototype.writeUtf8String,
            this,
            req,
            data,
          );
        }
        return super.writeUtf8String(req, data);
      },
    );
  }

  override writeUcs2String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOpWithUpgradeWait(
      req,
      () => {
        if (this.#hasStreamBaseField()) {
          return FunctionPrototypeCall(
            LibuvStreamWrap.prototype.writeUcs2String,
            this,
            req,
            data,
          );
        }
        return super.writeUcs2String(req, data);
      },
    );
  }

  override writeLatin1String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOpWithUpgradeWait(
      req,
      () => {
        if (this.#hasStreamBaseField()) {
          return FunctionPrototypeCall(
            LibuvStreamWrap.prototype.writeLatin1String,
            this,
            req,
            data,
          );
        }
        return super.writeLatin1String(req, data);
      },
    );
  }

  override readStart(): number {
    if (this.#hasStreamBaseField()) {
      return FunctionPrototypeCall(LibuvStreamWrap.prototype.readStart, this);
    }
    return (
      (this as TCP & { readStartNative?: () => number }).readStartNative?.() ??
      0
    );
  }

  override readStop(): number {
    if (this.#hasStreamBaseField()) {
      return FunctionPrototypeCall(LibuvStreamWrap.prototype.readStop, this);
    }
    return (
      (this as TCP & { readStopNative?: () => number }).readStopNative?.() ?? 0
    );
  }

  override unref(): void {
    this.#isUnrefed = true;
    (this as TCP & { markUnrefed?: () => void }).markUnrefed?.();
    try {
      super.unref();
    } catch {
      // Some native wrappers fail strict receiver checks.
    }
  }

  override hasRef(): boolean {
    return !this.#isUnrefed;
  }

  override ref(): void {
    this.#isUnrefed = false;
    (this as TCP & { markRefed?: () => void }).markRefed?.();
    try {
      super.ref();
    } catch {
      // Some native wrappers fail strict receiver checks.
    }
  }

  override close(cb?: () => void): void {
    if (this.#hasStreamBaseField()) {
      FunctionPrototypeCall(LibuvStreamWrap.prototype._onClose, this);
      cb?.();
      return;
    }
    super.close(cb);
  }

  override reset(closeCallback?: () => void): number {
    if (this.#hasStreamBaseField()) {
      const status = FunctionPrototypeCall(
        LibuvStreamWrap.prototype._onClose,
        this,
      );
      closeCallback?.();
      return status;
    }
    return super.reset(closeCallback);
  }

  override _onClose(): number {
    if (this.#hasStreamBaseField()) {
      return FunctionPrototypeCall(LibuvStreamWrap.prototype._onClose, this);
    }
    return super._onClose();
  }
}
