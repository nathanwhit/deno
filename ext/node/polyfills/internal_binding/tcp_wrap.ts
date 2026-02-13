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
  #listenKeepAlive?: ReturnType<typeof setInterval>;

  constructor(type: number, _conn?: Deno.Conn) {
    super(type);
  }

  override listen(backlog: number): number {
    const err = super.listen(backlog);
    if (err === 0) {
      if (this.#listenKeepAlive === undefined) {
        // Keep the event loop alive while a native listener is active.
        this.#listenKeepAlive = setInterval(() => {}, 1 << 30);
      }
      (this as TCP & { startListen?: () => number }).startListen?.();
    }
    return err;
  }

  override reset(closeCallback?: () => void): number {
    if (this.#listenKeepAlive !== undefined) {
      clearInterval(this.#listenKeepAlive);
      this.#listenKeepAlive = undefined;
    }
    return super.reset(closeCallback);
  }

  override _onClose(): number {
    if (this.#listenKeepAlive !== undefined) {
      clearInterval(this.#listenKeepAlive);
      this.#listenKeepAlive = undefined;
    }
    return super._onClose();
  }
}
