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

// TODO(petamoriken): enable prefer-primordials for node polyfills
// deno-lint-ignore-file prefer-primordials

// Re-export the Rust-backed TCP cppgc class.
import { TCP as NativeTCP } from "ext:core/ops";
import { core } from "ext:core/mod.js";
const { internalRidSymbol } = core;
import { notImplemented } from "ext:deno_node/_utils.ts";
import {
  AsyncWrap,
  providerType,
} from "ext:deno_node/internal_binding/async_wrap.ts";
import { kStreamBaseField } from "ext:deno_node/internal_binding/stream_wrap.ts";

/** The type of TCP socket. */
enum socketType {
  SOCKET,
  SERVER,
}

export class TCPConnectWrap extends AsyncWrap {
  oncomplete!: (
    status: number,
    handle: unknown,
    req: TCPConnectWrap,
    readable: boolean,
    writeable: boolean,
  ) => void;
  address!: string;
  port!: number;
  localAddress!: string;
  localPort!: number;

  constructor() {
    super(providerType.TCPCONNECTWRAP);
  }
}

export enum constants {
  SOCKET = socketType.SOCKET,
  SERVER = socketType.SERVER,
  UV_TCP_IPV6ONLY,
}

// Wrap the Rust TCP class to handle the optional `conn` second argument.
// When a Deno connection is passed (e.g., TLS server accept), store it
// in kStreamBaseField and attach its resource to the Rust stream backend.
const TCP = class TCP extends NativeTCP {
  constructor(type: number, conn?: any) {
    super(type);
    if (conn) {
      this[kStreamBaseField] = conn;
      const rid = conn.rid ?? conn[internalRidSymbol];
      if (typeof rid === "number") {
        this.attachResource(rid);
      }
    }
  }
};

TCP.prototype.open = (_fd: number): number => {
  notImplemented("TCP.prototype.open");
};

TCP.prototype.setSimultaneousAccepts = (_enable: boolean) => {
  notImplemented("TCP.prototype.setSimultaneousAccepts");
};

TCP.prototype.setNetPermToken = (
  _netPermToken: object | undefined,
) => {
  // Permission checking is handled by the Deno runtime.
};

export { TCP };
