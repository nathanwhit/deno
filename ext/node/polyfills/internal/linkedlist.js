// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent and Node contributors. All rights reserved. MIT license.
//
// Ported from Node.js lib/internal/linkedlist.js
// Circular doubly-linked list where the sentinel's _idleNext points to the
// first item and _idlePrev points to the last item.

function init(list) {
  list._idleNext = list;
  list._idlePrev = list;
}

function peek(list) {
  if (list._idleNext === list) return null;
  return list._idleNext;
}

function remove(item) {
  if (item._idleNext) {
    item._idleNext._idlePrev = item._idlePrev;
  }
  if (item._idlePrev) {
    item._idlePrev._idleNext = item._idleNext;
  }
  item._idleNext = item;
  item._idlePrev = item;
}

function append(list, item) {
  // If the item is already in a list, remove it first.
  if (item._idleNext !== item) {
    remove(item);
  }
  // Insert at the end (before the sentinel).
  item._idleNext = list;
  item._idlePrev = list._idlePrev;
  list._idlePrev._idleNext = item;
  list._idlePrev = item;
}

function isEmpty(list) {
  return list._idleNext === list;
}

export default {
  init,
  peek,
  remove,
  append,
  isEmpty,
};
