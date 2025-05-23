// deno-fmt-ignore-file
// deno-lint-ignore-file

// Copyright Joyent and Node contributors. All rights reserved. MIT license.
// Taken from Node 23.9.0
// This file is automatically generated by `tests/node_compat/runner/setup.ts`. Do not modify this file manually.

'use strict';
// Refs: https://github.com/nodejs/node/issues/10223

require('../common');
const vm = require('vm');
const assert = require('assert');

const context = vm.createContext({});

let code = `
   Object.defineProperty(this, 'foo', {value: 5});
   Object.getOwnPropertyDescriptor(this, 'foo');
`;

let desc = vm.runInContext(code, context);

assert.strictEqual(desc.writable, false);

// Check that interceptors work for symbols.
code = `
   const bar = Symbol('bar');
   Object.defineProperty(this, bar, {value: 6});
   Object.getOwnPropertyDescriptor(this, bar);
`;

desc = vm.runInContext(code, context);

assert.strictEqual(desc.value, 6);
