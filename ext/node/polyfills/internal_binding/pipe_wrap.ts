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
// OTHERWISE, ARISING FROM OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.

// This module ports:
// - https://github.com/nodejs/node/blob/master/src/pipe_wrap.cc
// - https://github.com/nodejs/node/blob/master/src/pipe_wrap.h

import { core, primordials } from "ext:core/mod.js";
import {
  Pipe as NativePipe,
  op_node_file_from_fd,
  op_pipe_connect,
  op_pipe_open,
  op_pipe_windows_wait,
} from "ext:core/ops";
import { PipeConn } from "ext:deno_net/01_net.js";

const { internalRidSymbol } = core;
import { notImplemented } from "ext:deno_node/_utils.ts";
// Side-effect import: connection_wrap.ts adds afterConnect to
// ConnectionWrap.prototype which the Rust Pipe/TCP inherit.
import "ext:deno_node/internal_binding/connection_wrap.ts";
import {
  AsyncWrap,
  providerType,
} from "ext:deno_node/internal_binding/async_wrap.ts";
import {
  codeMap,
  mapSysErrnoToUvErrno,
} from "ext:deno_node/internal_binding/uv.ts";
import { delay } from "ext:deno_node/_util/async.ts";
import { kStreamBaseField } from "ext:deno_node/internal_binding/stream_wrap.ts";
import { isWindows } from "ext:deno_node/_util/os.ts";

const {
  Error,
  ErrorPrototype,
  MapPrototypeGet,
  ObjectDefineProperty,
  ObjectPrototypeIsPrototypeOf,
  StringPrototypeIncludes,
  queueMicrotask,
} = primordials;

export enum socketType {
  SOCKET,
  SERVER,
  IPC,
}

/**
 * A wrapper for file-based streams (PTYs, pipes, etc.) that provides
 * the interface expected by LibuvStreamWrap.
 */
class FileStreamConn {
  #rid: number;
  #closed = false;

  constructor(rid: number) {
    this.#rid = rid;
    ObjectDefineProperty(this, internalRidSymbol, {
      __proto__: null,
      enumerable: false,
      value: rid,
    });
  }

  async read(buf: Uint8Array): Promise<number | null> {
    while (!this.#closed) {
      try {
        const nread = await core.read(this.#rid, buf);
        return nread === 0 ? null : nread;
      } catch (e) {
        if (
          ObjectPrototypeIsPrototypeOf(ErrorPrototype, e) &&
          ((e as Error).name === "WouldBlock" ||
            (e as { code?: string }).code === "EAGAIN")
        ) {
          await delay(10);
          continue;
        }
        throw e;
      }
    }
    return null;
  }

  async write(data: Uint8Array): Promise<number> {
    return await core.write(this.#rid, data);
  }

  close(): void {
    this.#closed = true;
    core.tryClose(this.#rid);
  }
}

// Wrap the Rust Pipe class to handle:
// 1. The optional `conn` constructor argument (child process stdio)
// 2. The open() file-based fallback for non-socket fds
// 3. Windows-specific connect/listen/accept (Rust only handles Unix)
// 4. Cleanup of the JS-side kStreamBaseField on close
const Pipe = class Pipe extends NativePipe {
  // JS-side closed flag for Windows accept loop.
  _jsClosed = false;
  // Windows-only state (mirrors Rust fields for JS-side access).
  _jsAddress?: string;
  _jsServerPipeRid?: number;
  _jsPendingInstances = 4;

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

// Override open() to handle file-based fallback for non-socket fds.
const _nativeOpen = NativePipe.prototype.open;
Pipe.prototype.open = function (fd: number): number {
  if (isWindows) {
    notImplemented("Pipe.prototype.open on Windows");
  }
  const result = _nativeOpen.call(this, fd);
  if (result === 0) {
    return 0;
  }
  // Rust couldn't open as Unix socket (e.g. PTY, regular pipe).
  // Try file-based I/O fallback.
  try {
    const rid = op_node_file_from_fd(fd);
    this[kStreamBaseField] = new FileStreamConn(rid);
    this.attachResource(rid);
    return 0;
  } catch {
    return result;
  }
};

// Override _onClose to clean up JS-side state.
const _nativeOnClose = NativePipe.prototype._onClose;
Pipe.prototype._onClose = function (): number {
  this._jsClosed = true;
  const result = _nativeOnClose.call(this);
  // Close the JS-side connection object if present.
  const stream = this[kStreamBaseField];
  if (stream) {
    try {
      stream.close();
    } catch {
      // already closed
    }
    this[kStreamBaseField] = undefined;
  }
  return result;
};

if (isWindows) {
  // On Windows, also mirror bind/setPendingInstances to JS-side state
  // so the JS connect/listen/accept paths can access them.
  const _nativeBind = NativePipe.prototype.bind;
  Pipe.prototype.bind = function (name: string): number {
    this._jsAddress = name;
    return _nativeBind.call(this, name);
  };

  const _nativeSetPendingInstances = NativePipe.prototype.setPendingInstances;
  Pipe.prototype.setPendingInstances = function (instances: number): void {
    this._jsPendingInstances = instances;
    _nativeSetPendingInstances.call(this, instances);
  };

  // On Windows, connect and listen are handled in JS since the Rust
  // implementations are #[cfg(unix)] only.
  Pipe.prototype.connect = function (
    req: PipeConnectWrap,
    address: string,
  ): number {
    try {
      const rid = op_pipe_connect(
        address,
        true,
        true,
        "net.createConnection()",
      );
      this[kStreamBaseField] = new PipeConn(rid);
      this.attachResource(rid);
      this._jsAddress = req.address = address;

      queueMicrotask(() => {
        try {
          this.afterConnect(req, 0);
        } catch {
          // swallow callback errors.
        }
      });
    } catch (e: unknown) {
      let code;
      const err = e as {
        code?: string;
        message?: string;
        rawOsError?: number;
        cause?: { rawOsError?: number };
      };
      if (err.code !== undefined) {
        code = MapPrototypeGet(codeMap, err.code) ??
          MapPrototypeGet(codeMap, "UNKNOWN")!;
      } else {
        const msg = err.message ?? "";
        if (StringPrototypeIncludes(msg, "ENOTSOCK")) {
          code = MapPrototypeGet(codeMap, "ENOTSOCK")!;
        } else if (
          StringPrototypeIncludes(msg, "ENOENT") ||
          StringPrototypeIncludes(msg, "NotFound")
        ) {
          code = MapPrototypeGet(codeMap, "ENOENT")!;
        } else {
          const rawOsError = err.rawOsError ?? err.cause?.rawOsError;
          if (rawOsError !== undefined) {
            code = mapSysErrnoToUvErrno(rawOsError);
          } else {
            code = MapPrototypeGet(codeMap, "UNKNOWN")!;
          }
        }
      }

      queueMicrotask(() => {
        try {
          this.afterConnect(req, code);
        } catch {
          // swallow callback errors.
        }
      });
    }

    return 0;
  };

  Pipe.prototype.listen = function (backlog: number): number {
    try {
      const rid = op_pipe_open(
        this._jsAddress!,
        this._jsPendingInstances,
        false,
        true,
        true,
        "net.Server.listen()",
      );

      this._jsServerPipeRid = rid;
      _acceptWindows(this);

      return 0;
    } catch (e) {
      if (ObjectPrototypeIsPrototypeOf(Deno.errors.NotCapable.prototype, e)) {
        throw e;
      }
      return MapPrototypeGet(codeMap, e.code ?? "UNKNOWN") ??
        MapPrototypeGet(codeMap, "UNKNOWN")!;
    }
  };

  async function _acceptWindows(pipe: InstanceType<typeof Pipe>): Promise<void> {
    const INITIAL_BACKOFF = 5;
    const MAX_BACKOFF = 1000;
    let backoffDelay: number | undefined;

    while (!pipe._jsClosed) {
      try {
        await op_pipe_windows_wait(pipe._jsServerPipeRid!);

        const connectionHandle = new Pipe(socketType.SOCKET);
        connectionHandle[kStreamBaseField] = new PipeConn(
          pipe._jsServerPipeRid!,
        );
        connectionHandle.attachResource(pipe._jsServerPipeRid!);

        try {
          pipe.onconnection!(0, connectionHandle);
        } catch {
          // swallow callback errors.
        }

        backoffDelay = undefined;

        const newRid = op_pipe_open(
          pipe._jsAddress!,
          pipe._jsPendingInstances,
          false,
          true,
          true,
          "net.Server.listen()",
        );

        pipe._jsServerPipeRid = newRid;
      } catch {
        if (pipe._jsClosed) {
          return;
        }

        try {
          pipe.onconnection!(
            MapPrototypeGet(codeMap, "UNKNOWN")!,
            undefined,
          );
        } catch {
          // swallow callback errors.
        }

        const d = backoffDelay ?? INITIAL_BACKOFF;
        await delay(d);
        backoffDelay = Math.min((d) * 2, MAX_BACKOFF);
      }
    }
  }
}

export { Pipe };

export class PipeConnectWrap extends AsyncWrap {
  oncomplete!: (
    status: number,
    handle: unknown,
    req: PipeConnectWrap,
    readable: boolean,
    writeable: boolean,
  ) => void;
  address!: string;

  constructor() {
    super(providerType.PIPECONNECTWRAP);
  }
}

export enum constants {
  SOCKET = socketType.SOCKET,
  SERVER = socketType.SERVER,
  IPC = socketType.IPC,
  UV_READABLE = 1,
  UV_WRITABLE = 2,
}
