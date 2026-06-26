// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Blinded-trampoline Eclair↔LDK matrix interop scenario.
//!
//! LDK (L1) serves a BOLT12 offer whose invoice advertises a blinded trampoline
//! path. An Eclair sender (E1) pays the offer through its own trampoline node,
//! which trampoline-routes to the blinded path's introduction node and onward to
//! L1. We exercise three (intro, relay) combos against a single provisioned pool
//! and channel union, reconfiguring only L1's served blinded path between combos.

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::Node;

use crate::common::expect_payment_received_event;
use crate::common::external_node::ExternalNode;
use ldk_node::Event;

/// Run a single blinded-trampoline combo against the already-provisioned pool.
///
/// The only state that changes between combos is L1's served blinded trampoline
/// path (`intro`-first, then `relay`) and a freshly-minted offer. `intro` and
/// `relay` describe the blinded path L1 advertises; `trampoline_node_id` is E1's
/// OWN direct trampoline peer (NOT necessarily `intro`) -- Eclair trampoline-
/// routes from it to `intro`.
async fn run_blinded_trampoline_combo<E>(
	l1: &Node, e1_sender: &E, intro: PublicKey, relay: PublicKey, trampoline_node_id: PublicKey,
	amount_msat: u64,
) where
	E: ExternalNode + ?Sized,
{
	// 1. Reconfigure L1's served blinded trampoline path. Order: intro-first.
	l1.set_trampoline_blinded_path(Some(vec![intro, relay]));

	// 2. Re-create the offer AFTER setting the path (clean reset; fresh offer id
	//    avoids any Eclair invoice/offer dedup across combos).
	let offer = l1
		.bolt12_payment()
		.receive(amount_msat, "blinded-trampoline-combo", Some(3600), None)
		.unwrap();
	let offer_str = offer.to_string();

	// 3. Eclair sender pays; trampolineNodeId is E1's own peer, NOT `intro`.
	e1_sender
		.pay_offer_trampoline(&offer_str, amount_msat, trampoline_node_id)
		.await
		.expect("eclair pay_offer_trampoline failed");

	// 4. Assert L1 (receiver) gets the payment.
	expect_payment_received_event!(l1, amount_msat);

	// 5. Reset for the next combo.
	l1.set_trampoline_blinded_path(None);
}

/// Loop the three blinded-trampoline combos against the single channel union.
///
/// Receives the provisioned pool from `run_blinded_trampoline_scenario`. The
/// channel union is opened ONCE in the runner before this closure runs; here we
/// only flip L1's blinded path and re-pay per combo.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn blinded_trampoline_pool_scenario<E1, E2, E3, BC, EL>(
	l1: &Node, _l2: &Node, e1: &E1, e2: &E2, e3: &E3, _bitcoind: &BC, _electrs: &EL,
) where
	E1: ExternalNode + ?Sized,
	E2: ExternalNode + ?Sized,
	E3: ExternalNode + ?Sized,
{
	let e2_id = e2.get_node_id().await.unwrap();
	let e3_id = e3.get_node_id().await.unwrap();
	// L2 is the second LDK node; the runner connects it into the union, so we
	// resolve its id here for the per-combo trampoline-node selection.
	let l2_id = _l2.node_id();

	// combos: (intro, relay, trampoline_node_id)
	let combos = [
		(e2_id, e3_id, l2_id), // combo1: intro=E2 relay=E3, tramp via L2
		(e2_id, l2_id, l2_id), // combo2: intro=E2 relay=L2, tramp via L2
		(l2_id, e2_id, e2_id), // combo3: intro=L2 relay=E2, tramp via E2
	];
	for (intro, relay, tramp) in combos {
		run_blinded_trampoline_combo(l1, e1, intro, relay, tramp, 50_000_000).await;
	}
}
