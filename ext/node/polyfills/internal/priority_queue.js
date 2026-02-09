// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent and Node contributors. All rights reserved. MIT license.
//
// Ported from Node.js lib/internal/priority_queue.js
// Binary min-heap with custom comparator and position tracking.

export default class PriorityQueue {
  #compare;
  #setPosition;
  #heap;
  #size;

  constructor(comparator, setPosition) {
    this.#compare = comparator;
    this.#setPosition = setPosition;
    // 1-indexed heap array. Index 0 is unused.
    this.#heap = new Array(64);
    this.#size = 0;
  }

  insert(value) {
    const pos = ++this.#size;
    this.#heap[pos] = value;
    if (this.#setPosition) this.#setPosition(value, pos);
    this.#percolateUp(pos);
  }

  peek() {
    return this.#size > 0 ? this.#heap[1] : undefined;
  }

  shift() {
    if (this.#size === 0) return undefined;
    const top = this.#heap[1];
    if (this.#setPosition) this.#setPosition(top, null);
    if (this.#size === 1) {
      this.#heap[1] = undefined;
      this.#size = 0;
      return top;
    }
    this.#heap[1] = this.#heap[this.#size];
    this.#heap[this.#size] = undefined;
    this.#size--;
    if (this.#setPosition) this.#setPosition(this.#heap[1], 1);
    this.#percolateDown(1);
    return top;
  }

  removeAt(pos) {
    if (pos < 1 || pos > this.#size) return;
    const item = this.#heap[pos];
    if (this.#setPosition) this.#setPosition(item, null);
    if (pos === this.#size) {
      this.#heap[this.#size] = undefined;
      this.#size--;
      return;
    }
    this.#heap[pos] = this.#heap[this.#size];
    this.#heap[this.#size] = undefined;
    this.#size--;
    if (this.#setPosition) this.#setPosition(this.#heap[pos], pos);
    // The moved element may need to go up or down.
    if (pos > 1 && this.#compare(this.#heap[pos], this.#heap[pos >> 1]) < 0) {
      this.#percolateUp(pos);
    } else {
      this.#percolateDown(pos);
    }
  }

  percolateDown(pos) {
    this.#percolateDown(pos);
  }

  #percolateUp(pos) {
    const heap = this.#heap;
    const item = heap[pos];
    while (pos > 1) {
      const parent = pos >> 1;
      if (this.#compare(item, heap[parent]) >= 0) break;
      heap[pos] = heap[parent];
      if (this.#setPosition) this.#setPosition(heap[pos], pos);
      pos = parent;
    }
    heap[pos] = item;
    if (this.#setPosition) this.#setPosition(item, pos);
  }

  #percolateDown(pos) {
    const heap = this.#heap;
    const size = this.#size;
    const item = heap[pos];
    while (true) {
      let child = pos * 2;
      if (child > size) break;
      const right = child + 1;
      if (right <= size && this.#compare(heap[right], heap[child]) < 0) {
        child = right;
      }
      if (this.#compare(heap[child], item) >= 0) break;
      heap[pos] = heap[child];
      if (this.#setPosition) this.#setPosition(heap[pos], pos);
      pos = child;
    }
    heap[pos] = item;
    if (this.#setPosition) this.#setPosition(item, pos);
  }
}
