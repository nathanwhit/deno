use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::op2;
use deno_core::OpState;

use crate::args::BundleFlags;
use crate::args::DenoSubcommand;
use crate::args::Flags;

deno_core::extension!(deno_bundle, ops = [op_deno_bundle], options = {
  flags: Arc<Flags>,
}, state = |state, options| {
  state.put(options.flags);
});

#[op2(async)]
async fn op_deno_bundle(
  op_state: Rc<RefCell<OpState>>,
  #[serde] bundle_flags: BundleFlags,
) {
  let state = op_state.borrow();
  let flags = state.borrow::<Arc<Flags>>();
  let mut flags = flags.as_ref().clone();

  flags.subcommand = DenoSubcommand::Bundle(bundle_flags.clone());
  let flags = Arc::new(flags);
  crate::tools::bundle::bundle(flags, bundle_flags)
    .await
    .unwrap();

  // let factory = CliFactory::new
}
