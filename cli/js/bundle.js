import { core } from "ext:core/mod.js";

const {
  op_deno_bundle,
} = core.ops;

/** 
 * @typedef BundleFlags
 * @property {string} entrypoints
 * @property {string | undefined} output_path
 * @property {string | undefined} output_dir
 * @property {string[] | undefined} external
 * @property {string | undefined} format
 * @property {boolean | undefined} minify
 * @property {boolean | undefined} code_splitting
 * @property {boolean | undefined} one_file
 * @property {string | undefined} packages
 * */

/** @param {BundleFlags} flags */
async function bundle(flags) {
  const result = await op_deno_bundle(flags);
  return result;
}
globalThis.Deno.bundle = bundle;
