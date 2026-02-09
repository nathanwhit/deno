// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent and Node contributors. All rights reserved. MIT license.

import { core, primordials } from "ext:core/mod.js";
const {
  getAsyncContext,
  setAsyncContext,
} = core;
const {
  FunctionPrototypeBind,
  MathTrunc,
  NumberIsFinite,
  ReflectApply,
  SafeArrayIterator,
  SafeMap,
  MapPrototypeDelete,
  MapPrototypeGet,
  MapPrototypeSet,
  Symbol,
  SymbolToPrimitive,
} = primordials;
import {
  op_immediate_count,
  op_immediate_ref_count,
  op_immediate_set_has_outstanding,
  op_node_timer_now,
  op_node_timer_schedule,
  op_node_timer_setup,
  op_node_timer_toggle_ref,
} from "ext:core/ops";
import { inspect } from "ext:deno_node/internal/util/inspect.mjs";
import {
  validateFunction,
  validateNumber,
} from "ext:deno_node/internal/validators.mjs";
import { ERR_OUT_OF_RANGE } from "ext:deno_node/internal/errors.ts";
import { emitWarning } from "node:process";
import { runNextTicks } from "ext:deno_node/_next_tick.ts";
import L from "ext:deno_node/internal/linkedlist.js";
import PriorityQueue from "ext:deno_node/internal/priority_queue.js";

// Timeout values > TIMEOUT_MAX are set to 1.
export const TIMEOUT_MAX = 2 ** 31 - 1;

export const kDestroy = Symbol("destroy");
export const kTimerId = Symbol("timerId");
export const kTimeout = Symbol("timeout");
export const kRefed = Symbol("refed");

// ---------------------------------------------------------------------------
// TimersList -- groups timers with the same duration (msecs).
// Acts as the circular linked list sentinel for its timers.
// ---------------------------------------------------------------------------

let timerListId = -Number.MAX_SAFE_INTEGER;

class TimersList {
  constructor(expiry, msecs) {
    this._idleNext = this; // Circular list sentinel
    this._idlePrev = this;
    this.expiry = expiry;
    this.id = timerListId++;
    this.msecs = msecs;
    this.priorityQueuePosition = null;
  }
}

// Compare TimersList entries: first by expiry, then by id (insertion order).
function compareTimersLists(a, b) {
  const expiryDiff = a.expiry - b.expiry;
  if (expiryDiff === 0) {
    if (a.id < b.id) return -1;
    if (a.id > b.id) return 1;
    return 0;
  }
  return expiryDiff;
}

function setPosition(node, pos) {
  node.priorityQueuePosition = pos;
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

let nextTimerId = 1;
let nextExpiry = Infinity;
let refCount = 0;
let setupDone = false;

// Cached "now" value. Updated lazily on first access per tick and set
// directly by processTimers. Avoids repeated op_node_timer_now() op calls
// (each is a JS->Rust boundary crossing) when multiple timers are
// created/refreshed in the same event loop tick.
let cachedNow = 0;

// Priority queue of TimersList objects, ordered by expiry.
const timerListQueue = new PriorityQueue(compareTimersLists, setPosition);

// Map from msecs -> TimersList. Object with null prototype for fast lookup.
const timerListMap = { __proto__: null };

// Map from timer ID -> Timeout, for clearTimeout/clearInterval lookup.
const timerById = new SafeMap();

/**
 * @param {number} id
 * @returns {Timeout | undefined}
 */
export function getActiveTimer(id) {
  return MapPrototypeGet(timerById, id);
}

function ensureSetup() {
  if (!setupDone) {
    setupDone = true;
    op_node_timer_setup(processTimers);
  }
}

function getTimerNow() {
  if (cachedNow === 0) {
    cachedNow = op_node_timer_now();
  }
  return cachedNow;
}

function incRefCount() {
  if (refCount === 0) {
    op_node_timer_toggle_ref(true);
  }
  refCount++;
}

function decRefCount() {
  refCount--;
  if (refCount === 0) {
    op_node_timer_toggle_ref(false);
  }
}

// ---------------------------------------------------------------------------
// processTimers -- called from the native timer callback.
//
// Returns the next expiry encoding:
//   0  = no more timers
//   >0 = next absolute expiry (has ref'd timers)
//   <0 = next absolute expiry (all unref'd timers)
// ---------------------------------------------------------------------------

function processTimers(now) {
  cachedNow = now;
  nextExpiry = Infinity;
  let list;
  let ranAtLeastOneList = false;
  while ((list = timerListQueue.peek()) != null) {
    if (list.expiry > now) {
      nextExpiry = list.expiry;
      return refCount > 0 ? nextExpiry : -nextExpiry;
    }
    if (ranAtLeastOneList) {
      runNextTicks();
    } else {
      ranAtLeastOneList = true;
    }
    listOnTimeout(list, now);
  }
  return 0;
}

function listOnTimeout(list, now) {
  const msecs = list.msecs;
  let ranAtLeastOneTimer = false;
  let timer;
  while ((timer = L.peek(list)) != null) {
    const diff = now - timer._idleStart;
    if (diff < msecs) {
      // Timer not yet expired. Update list expiry and re-sort in the queue.
      list.expiry = MathTrunc(timer._idleStart) + msecs;
      timerListQueue.percolateDown(1);
      return;
    }

    if (ranAtLeastOneTimer) {
      runNextTicks();
    } else {
      ranAtLeastOneTimer = true;
    }

    // Remove from the linked list.
    L.remove(timer);

    const callback = timer._onTimeout;
    if (!callback) {
      // Timer was cancelled/destroyed but not yet unlinked.
      if (!timer._destroyed) {
        timer._destroyed = true;
        if (timer[kRefed]) decRefCount();
      }
      continue;
    }

    const args = timer._timerArgs;

    if (timer._isRepeat) {
      // Repeating timer: re-insert with updated start time.
      timer._idleStart = now;
      insert(timer);
      try {
        FunctionPrototypeBind(callback, timer)(
          ...new SafeArrayIterator(args),
        );
      } catch (e) {
        globalThis.reportError(e);
      }
    } else {
      // One-shot timer: fire and check if it was refreshed/re-inserted.
      try {
        FunctionPrototypeBind(callback, timer)(
          ...new SafeArrayIterator(args),
        );
      } catch (e) {
        // If callback threw and timer wasn't re-inserted, destroy it.
        if (timer._idleNext === timer) {
          if (!timer._destroyed) {
            timer._destroyed = true;
            MapPrototypeDelete(timerById, timer[kTimerId]);
            if (timer[kRefed]) decRefCount();
          }
        }
        globalThis.reportError(e);
        continue;
      }
      // If the timer wasn't re-inserted by the callback (via refresh()),
      // mark it as destroyed.
      if (timer._idleNext === timer) {
        if (!timer._destroyed) {
          timer._destroyed = true;
          MapPrototypeDelete(timerById, timer[kTimerId]);
          if (timer[kRefed]) decRefCount();
        }
      }
    }
  }

  // List is empty -- clean up.
  // Only delete from the map if the entry still points to THIS list.
  // A timer callback may have created a new list for the same msecs.
  if (timerListMap[msecs] === list) {
    delete timerListMap[msecs];
  }
  // Only shift from queue if this list is still at the top.
  // kDestroy may have already removed it.
  if (list.priorityQueuePosition !== null) {
    timerListQueue.removeAt(list.priorityQueuePosition);
  }
}

// ---------------------------------------------------------------------------
// insert -- add a timer to the appropriate TimersList.
// ---------------------------------------------------------------------------

function insert(timer) {
  ensureSetup();
  const msecs = MathTrunc(timer._idleTimeout);
  const now = getTimerNow();
  timer._idleStart = now;

  let list = timerListMap[msecs];
  if (list === undefined) {
    const expiry = MathTrunc(now) + msecs;
    list = new TimersList(expiry, msecs);
    timerListMap[msecs] = list;
    timerListQueue.insert(list);

    if (expiry < nextExpiry) {
      nextExpiry = expiry;
      op_node_timer_schedule(expiry - now);
    }
  }

  L.append(list, timer);
}

// ---------------------------------------------------------------------------
// Timeout constructor
// ---------------------------------------------------------------------------

export function Timeout(callback, after, args, isRepeat, isRefed) {
  // Coerce to number, matching Node.js behavior:
  // NaN, undefined, null, booleans, objects, etc. become 1
  // Negative values become 1
  // Values > TIMEOUT_MAX become 1
  after *= 1;
  if (!(after >= 1 && after <= TIMEOUT_MAX)) {
    after = 1;
  }
  this._idleTimeout = after;
  this._onTimeout = callback;
  this._timerArgs = args;
  this._isRepeat = isRepeat;
  this._destroyed = false;
  this._idleStart = 0;
  this._idlePrev = this;
  this._idleNext = this;
  this[kRefed] = isRefed;
  this[kTimerId] = nextTimerId++;

  ensureSetup();
  MapPrototypeSet(timerById, this[kTimerId], this);
  if (isRefed) {
    incRefCount();
  }
  insert(this);
}

Timeout.prototype[kDestroy] = function () {
  if (this._destroyed) {
    return;
  }
  this._destroyed = true;

  // Unlink from the TimersList linked list.
  L.remove(this);

  const msecs = MathTrunc(this._idleTimeout);
  const list = timerListMap[msecs];
  if (list !== undefined && L.isEmpty(list)) {
    // The list is empty -- remove it from the queue and map.
    if (list.priorityQueuePosition !== null) {
      timerListQueue.removeAt(list.priorityQueuePosition);
    }
    delete timerListMap[msecs];
  }

  MapPrototypeDelete(timerById, this[kTimerId]);
  if (this[kRefed]) {
    decRefCount();
  }
  this._idleTimeout = -1;
};

// Make sure the linked list only shows the minimal necessary information.
Timeout.prototype[inspect.custom] = function (_, options) {
  return inspect(this, {
    ...options,
    // Only inspect one level.
    depth: 0,
    // It should not recurse.
    customInspect: false,
  });
};

Timeout.prototype.refresh = function () {
  if (this._idleTimeout < 0) return this;

  ensureSetup();

  const wasDestroyed = this._destroyed;
  this._destroyed = false;

  // If the timer was previously destroyed/fired, we need to re-register it.
  if (wasDestroyed) {
    MapPrototypeSet(timerById, this[kTimerId], this);
    if (this[kRefed]) {
      incRefCount();
    }
  }

  // insert() calls L.append which calls L.remove first, so this handles
  // both the case where the timer is in a list and where it isn't.
  insert(this);

  return this;
};

Timeout.prototype.unref = function () {
  if (this[kRefed]) {
    this[kRefed] = false;
    if (!this._destroyed) {
      decRefCount();
    }
  }
  return this;
};

Timeout.prototype.ref = function () {
  if (!this[kRefed]) {
    this[kRefed] = true;
    if (!this._destroyed) {
      incRefCount();
    }
  }
  return this;
};

Timeout.prototype.hasRef = function () {
  return this[kRefed];
};

Timeout.prototype[SymbolToPrimitive] = function () {
  return this[kTimerId];
};

/**
 * @param {number} msecs
 * @param {string} name
 * @returns
 */
export function getTimerDuration(msecs, name) {
  validateNumber(msecs, name);

  if (msecs < 0 || !NumberIsFinite(msecs)) {
    throw new ERR_OUT_OF_RANGE(name, "a non-negative finite number", msecs);
  }

  // Ensure that msecs fits into signed int32
  if (msecs > TIMEOUT_MAX) {
    emitWarning(
      `${msecs} does not fit into a 32-bit signed integer.` +
        `\nTimer duration was truncated to ${TIMEOUT_MAX}.`,
      "TimeoutOverflowWarning",
    );

    return TIMEOUT_MAX;
  }

  return msecs;
}

export function setUnrefTimeout(callback, timeout, ...args) {
  validateFunction(callback, "callback");
  return new Timeout(callback, timeout, args, false, false);
}

// This code was forked from Node.js
// Copyright Node.js contributors. All rights reserved.
//
// A linked list for storing `setImmediate()` requests
class ImmediateList {
  constructor() {
    this.head = null;
    this.tail = null;
  }

  // Appends an item to the end of the linked list, adjusting the current tail's
  // next pointer and the item's previous pointer where applicable
  append(item) {
    if (this.tail !== null) {
      this.tail._idleNext = item;
      item._idlePrev = this.tail;
    } else {
      this.head = item;
    }
    this.tail = item;
  }

  // Removes an item from the linked list, adjusting the pointers of adjacent
  // items and the linked list's head or tail pointers as necessary
  remove(item) {
    if (item._idleNext) {
      item._idleNext._idlePrev = item._idlePrev;
    }

    if (item._idlePrev) {
      item._idlePrev._idleNext = item._idleNext;
    }

    if (item === this.head) {
      this.head = item._idleNext;
    }
    if (item === this.tail) {
      this.tail = item._idlePrev;
    }

    item._idleNext = null;
    item._idlePrev = null;
  }
}

// Create a single linked list instance only once at startup
export const immediateQueue = new ImmediateList();
// If an uncaught exception was thrown during execution of immediateQueue,
// this queue will store all remaining Immediates that need to run upon
// resolution of all error handling (if process is still alive).
const outstandingQueue = new ImmediateList();

export function runImmediates() {
  const queue = outstandingQueue.head !== null
    ? outstandingQueue
    : immediateQueue;
  let immediate = queue.head;
  // Clear the linked list early in case new `setImmediate()`
  // calls occur while immediate callbacks are executed
  if (queue !== outstandingQueue) {
    queue.head = queue.tail = null;
    op_immediate_set_has_outstanding(true);
  }

  let prevImmediate;
  let ranAtLeastOneImmediate = false;
  while (immediate !== null) {
    if (ranAtLeastOneImmediate) {
      runNextTicks();
    } else {
      ranAtLeastOneImmediate = true;
    }

    // It's possible for this current Immediate to be cleared while executing
    // the next tick queue above, which means we need to use the previous
    // Immediate's _idleNext which is guaranteed to not have been cleared.
    if (immediate._destroyed) {
      outstandingQueue.head = immediate = prevImmediate._idleNext;
      continue;
    }

    immediate._destroyed = true;

    op_immediate_count(false);
    if (immediate[kRefed]) {
      op_immediate_ref_count(false);
    }
    immediate[kRefed] = null;

    prevImmediate = immediate;

    // TODO:
    // const priorContextFrame = AsyncContextFrame.exchange(
    // immediate[async_context_frame],
    // );

    // TODO:
    // const asyncId = immediate[async_id_symbol];
    // emitBefore(asyncId, immediate[trigger_async_id_symbol], immediate);

    try {
      const argv = immediate._argv;
      if (!argv) {
        immediate._onImmediate();
      } else {
        immediate._onImmediate(...new SafeArrayIterator(argv));
      }
    } finally {
      immediate._onImmediate = null;

      // TODO:
      // if (destroyHooksExist()) {
      // emitDestroy(asyncId);
      // }

      outstandingQueue.head = immediate = immediate._idleNext;
    }
    // emitAfter(asyncId);

    // TODO:
    // AsyncContextFrame.set(priorContextFrame);
  }

  if (queue === outstandingQueue) {
    outstandingQueue.head = null;
  }

  op_immediate_set_has_outstanding(false);
}

export class Immediate {
  constructor(unboundCallback, ...args) {
    const asyncContext = getAsyncContext();
    const callback = (...argv) => {
      const oldContext = getAsyncContext();
      try {
        setAsyncContext(asyncContext);
        return ReflectApply(unboundCallback, globalThis, argv);
      } finally {
        setAsyncContext(oldContext);
      }
    };

    this._idleNext = null;
    this._idlePrev = null;
    this._onImmediate = callback;
    this._argv = args;
    this._destroyed = false;
    this[kRefed] = false;

    // TODO:
    // initAsyncResource(this, "Immediate");

    this.ref();
    op_immediate_count(true);
    immediateQueue.append(this);
  }

  ref() {
    if (this[kRefed] === false) {
      this[kRefed] = true;
      op_immediate_ref_count(true);
    }
    return this;
  }

  unref() {
    if (this[kRefed] === true) {
      this[kRefed] = false;
      op_immediate_ref_count(false);
    }
    return this;
  }

  hasRef() {
    return !!this[kRefed];
  }

  [inspect.custom] = function (_, options) {
    return inspect(this, {
      ...options,
      // Only inspect one level.
      depth: 0,
      // It should not recurse.
      customInspect: false,
    });
  };
}

export default {
  getTimerDuration,
  kTimerId,
  kTimeout,
  setUnrefTimeout,
  Timeout,
  TIMEOUT_MAX,
};
