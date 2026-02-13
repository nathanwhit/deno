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

import {
  TCP as RustTCP,
  TCPConnectWrap as RustTCPConnectWrap,
} from "ext:core/ops";
import {
  clearInterval as nodeClearInterval,
  setInterval as nodeSetInterval,
} from "node:timers";

export const constants = {
  SOCKET: 0,
  SERVER: 1,
  UV_TCP_IPV6ONLY: 1,
  UV_TCP_REUSEPORT: 2,
} as const;

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
  #livenessTimer?: ReturnType<typeof nodeSetInterval>;
  #isUnrefed = false;
  #listening = false;
  #pendingOps = 0;

  constructor(type: number, _conn?: Deno.Conn) {
    super(type);
  }

  #syncLivenessTimer() {
    const shouldLive = this.#listening || this.#pendingOps > 0;
    if (shouldLive) {
      if (this.#livenessTimer === undefined) {
        this.#livenessTimer = nodeSetInterval(() => {}, 1 << 30);
      }
      if (this.#isUnrefed) {
        this.#livenessTimer.unref?.();
      } else {
        this.#livenessTimer.ref?.();
      }
      return;
    }
    if (this.#livenessTimer !== undefined) {
      nodeClearInterval(this.#livenessTimer);
      this.#livenessTimer = undefined;
    }
  }

  #runReqOp<R extends { oncomplete?: (...args: unknown[]) => unknown }>(
    req: R,
    invoke: () => number,
  ): number {
    this.#pendingOps++;
    this.#syncLivenessTimer();
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      this.#pendingOps = Math.max(0, this.#pendingOps - 1);
      this.#syncLivenessTimer();
    };
    const original = req.oncomplete;
    req.oncomplete = (...args: unknown[]) => {
      try {
        original?.apply(req, args);
      } finally {
        finish();
      }
    };
    const err = invoke();
    if (err !== 0) {
      finish();
    }
    return err;
  }

  override listen(backlog: number): number {
    const err = super.listen(backlog);
    if (err === 0) {
      this.#listening = true;
      this.#syncLivenessTimer();
      (this as TCP & { startListen?: () => number }).startListen?.();
    }
    return err;
  }

  override connect(req: TCPConnectWrap, address: string, port: number): number {
    return this.#runReqOp(
      req,
      () => super.connect(req, address, port),
    );
  }

  override connect6(req: TCPConnectWrap, address: string, port: number): number {
    return this.#runReqOp(
      req,
      () => super.connect6(req, address, port),
    );
  }

  override shutdown(req: { oncomplete?: (...args: unknown[]) => unknown }): number {
    return this.#runReqOp(req, () => super.shutdown(req));
  }

  override writeBuffer(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: Uint8Array,
  ): number {
    return this.#runReqOp(req, () => super.writeBuffer(req, data));
  }

  override writev(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    chunks: unknown[],
    allBuffers: boolean,
  ): number {
    return this.#runReqOp(req, () => super.writev(req, chunks, allBuffers));
  }

  override writeAsciiString(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOp(req, () => super.writeAsciiString(req, data));
  }

  override writeUtf8String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOp(req, () => super.writeUtf8String(req, data));
  }

  override writeUcs2String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOp(req, () => super.writeUcs2String(req, data));
  }

  override writeLatin1String(
    req: { oncomplete?: (...args: unknown[]) => unknown },
    data: string,
  ): number {
    return this.#runReqOp(req, () => super.writeLatin1String(req, data));
  }

  override readStart(): number {
    return (
      (this as TCP & { readStartNative?: () => number }).readStartNative?.() ??
      0
    );
  }

  override readStop(): number {
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
    this.#syncLivenessTimer();
  }

  override ref(): void {
    this.#isUnrefed = false;
    (this as TCP & { markRefed?: () => void }).markRefed?.();
    try {
      super.ref();
    } catch {
      // Some native wrappers fail strict receiver checks.
    }
    this.#syncLivenessTimer();
  }

  override reset(closeCallback?: () => void): number {
    this.#listening = false;
    this.#pendingOps = 0;
    this.#syncLivenessTimer();
    return super.reset(closeCallback);
  }

  override _onClose(): number {
    this.#listening = false;
    this.#pendingOps = 0;
    this.#syncLivenessTimer();
    return super._onClose();
  }
}
