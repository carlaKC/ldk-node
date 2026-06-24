// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Trampoline-forward interop scenario.
//!
//! Topology: Eclair-A ↔ LDK ↔ Eclair-B with direct channels. Eclair-A pays a
//! BOLT11 invoice issued by Eclair-B, naming LDK as the trampoline node. We
//! assert both the sender-side `sent` status (via Eclair's REST polling) and
//! LDK's `PaymentForwarded` event.

use std::time::Duration;

use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning_invoice::{Bolt11InvoiceDescription, Description};

use crate::common::external_node::ExternalNode;
use crate::common::generate_blocks_and_wait;
use crate::common::scenarios::{channel, retry_until_ok, wait_for_htlcs_settled, Side};

/// Eclair-A → LDK → Eclair-B unblinded trampoline forward over BOLT11.
///
/// Requires both peers to advertise the BOLT #836 trampoline routing feature
/// bit. LDK does so unconditionally at the pinned carlaKC fork rev.
pub(crate) async fn trampoline_forward_scenario<E, NA, NB>(
	node: &Node, eclair_a: &NA, eclair_b: &NB, bitcoind: &BitcoindClient, electrs: &E,
) where
	E: ElectrumApi,
	NA: ExternalNode + ?Sized,
	NB: ExternalNode + ?Sized,
{
	// Eclair-A ↔ LDK (LDK pushes balance toward Eclair-A).
	let (a_ldk_user_ch, a_ldk_ext_ch) = channel::open_channel_to_external(
		node,
		eclair_a,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	// LDK ↔ Eclair-B (LDK pushes balance toward Eclair-B so the forward leg
	// has inbound liquidity at Eclair-B).
	let (ldk_b_user_ch, ldk_b_ext_ch) = channel::open_channel_to_external(
		node,
		eclair_b,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	let invoice = eclair_b
		.create_invoice(50_000_000, "trampoline-forward-test")
		.await
		.expect("eclair-b create_invoice failed");

	let ldk_node_id = node.node_id();

	eclair_a.pay_trampoline(&invoice, ldk_node_id).await.expect("eclair-a pay_trampoline failed");

	let event = tokio::time::timeout(
		std::time::Duration::from_secs(crate::common::INTEROP_TIMEOUT_SECS),
		node.next_event_async(),
	)
	.await
	.expect("timed out waiting for PaymentForwarded event");
	match event {
		Event::PaymentForwarded {
			ref next_htlcs, ref prev_htlcs, total_fee_earned_msat, ..
		} => {
			println!(
				"LDK got PaymentForwarded: {} prev, {} next, fee {:?} msat",
				prev_htlcs.len(),
				next_htlcs.len(),
				total_fee_earned_msat
			);
			assert!(!prev_htlcs.is_empty(), "PaymentForwarded had empty prev_htlcs");
			assert!(!next_htlcs.is_empty(), "PaymentForwarded had empty next_htlcs");
			node.event_handled().unwrap();
		},
		other => panic!("LDK got unexpected event waiting for PaymentForwarded: {:?}", other),
	}

	wait_for_htlcs_settled(eclair_a, &a_ldk_ext_ch).await;
	wait_for_htlcs_settled(eclair_b, &ldk_b_ext_ch).await;

	channel::cooperative_close(
		node,
		eclair_a,
		bitcoind,
		electrs,
		&a_ldk_user_ch,
		&a_ldk_ext_ch,
		Side::Ldk,
	)
	.await;
	channel::cooperative_close(
		node,
		eclair_b,
		bitcoind,
		electrs,
		&ldk_b_user_ch,
		&ldk_b_ext_ch,
		Side::Ldk,
	)
	.await;
}

/// Eclair-A → LDK → (unreachable Eclair-B): LDK as trampoline forwarder must
/// FAIL because it has no channel/route to the recipient named in the invoice.
///
/// Topology: only Eclair-A ↔ LDK is opened; LDK has no channel to Eclair-B. When
/// Eclair-A pays Eclair-B's invoice naming LDK as the trampoline, LDK peels the
/// trampoline onion, finds no route to Eclair-B, and fails the HTLC backwards
/// (`unknown_next_peer` wrapped in the trampoline error). We assert the payment
/// fails on the Eclair sender side and that LDK never emits `PaymentForwarded`.
pub(crate) async fn trampoline_forward_unknown_next_scenario<E, NA, NB>(
	node: &Node, eclair_a: &NA, eclair_b: &NB, bitcoind: &BitcoindClient, electrs: &E,
) where
	E: ElectrumApi,
	NA: ExternalNode + ?Sized,
	NB: ExternalNode + ?Sized,
{
	// Eclair-A ↔ LDK only. Deliberately leave LDK with no path to Eclair-B.
	let (a_ldk_user_ch, a_ldk_ext_ch) = channel::open_channel_to_external(
		node,
		eclair_a,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	let invoice = eclair_b
		.create_invoice(50_000_000, "trampoline-forward-fail-test")
		.await
		.expect("eclair-b create_invoice failed");

	let ldk_node_id = node.node_id();

	// The trampoline payment must fail: LDK cannot reach the recipient. Eclair
	// surfaces the terminal failure via /getsentinfo (pay_trampoline returns Err).
	let result = eclair_a.pay_trampoline(&invoice, ldk_node_id).await;
	assert!(
		result.is_err(),
		"expected trampoline payment to fail (LDK has no route to recipient), got Ok({:?})",
		result.ok()
	);
	println!("Eclair-A trampoline payment failed as expected: {:?}", result.err());

	// LDK must not report a successful forward. Drain events for a short window
	// and assert no `PaymentForwarded` surfaces (a failed forward produces no
	// such event).
	let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
	while tokio::time::Instant::now() < deadline {
		match tokio::time::timeout(Duration::from_secs(1), node.next_event_async()).await {
			Ok(Event::PaymentForwarded { .. }) => {
				panic!("LDK unexpectedly emitted PaymentForwarded for a forward that should fail")
			},
			Ok(_) => node.event_handled().unwrap(),
			Err(_) => {},
		}
	}

	// The failed HTLC is removed once the failure propagates back; make sure the
	// channel is clean before closing.
	wait_for_htlcs_settled(eclair_a, &a_ldk_ext_ch).await;

	channel::cooperative_close(
		node,
		eclair_a,
		bitcoind,
		electrs,
		&a_ldk_user_ch,
		&a_ldk_ext_ch,
		Side::Ldk,
	)
	.await;
}

/// Eclair-A → Eclair-T (trampoline) → LDK: LDK is the final RECIPIENT of a
/// trampoline payment relayed by Eclair.
///
/// Topology: Eclair-A ↔ Eclair-T (eclair-to-eclair) and Eclair-T ↔ LDK. LDK
/// issues a plain BOLT11 invoice — it does not (and need not) advertise the
/// trampoline feature, because the last trampoline (Eclair-T) reaches it as an
/// ordinary non-trampoline recipient using the invoice routing hints. Eclair-A
/// pays the invoice naming Eclair-T as the trampoline node; Eclair-T relays to
/// LDK. We assert LDK claims the payment (`PaymentReceived`).
///
/// This exercises Eclair as a trampoline forwarder and LDK as a (plain) payee
/// of a trampoline-originated payment — the only receive-side role reachable
/// through ldk-node's public API today.
pub(crate) async fn trampoline_receive_scenario<E, NA, NT>(
	node: &Node, eclair_a: &NA, eclair_t: &NT, bitcoind: &BitcoindClient, electrs: &E,
) where
	E: ElectrumApi,
	NA: ExternalNode + ?Sized,
	NT: ExternalNode + ?Sized,
{
	// Eclair-A ↔ Eclair-T: A funds, so A has outbound liquidity toward T.
	let (a_t_funder_ch, _a_t_fundee_ch) = channel::open_channel_between_externals(
		eclair_a, eclair_t, bitcoind, electrs, 1_000_000, None,
	)
	.await;

	// Eclair-T ↔ LDK: LDK opens and pushes balance to T, so T has the outbound
	// liquidity it needs to forward the trampoline payment on to LDK.
	let (t_ldk_user_ch, t_ldk_ext_ch) = channel::open_channel_to_external(
		node,
		eclair_t,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	// LDK issues an ordinary BOLT11 invoice (reached as a non-trampoline payee).
	let invoice = node
		.bolt11_payment()
		.receive(
			50_000_000,
			&Bolt11InvoiceDescription::Direct(
				Description::new("trampoline-receive-test".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	let invoice_str = invoice.to_string();

	let eclair_t_id = eclair_t.get_node_id().await.unwrap();

	// Eclair-A pays LDK's invoice via Eclair-T as trampoline. Retry to absorb
	// gossip/route-hint propagation delay.
	retry_until_ok(10, "pay_trampoline to LDK recipient", || {
		eclair_a.pay_trampoline(&invoice_str, eclair_t_id)
	})
	.await;

	expect_payment_received_event!(node, 50_000_000);

	wait_for_htlcs_settled(eclair_t, &t_ldk_ext_ch).await;
	wait_for_htlcs_settled(eclair_a, &a_t_funder_ch).await;

	channel::cooperative_close(
		node,
		eclair_t,
		bitcoind,
		electrs,
		&t_ldk_user_ch,
		&t_ldk_ext_ch,
		Side::Ldk,
	)
	.await;

	// Close the eclair-to-eclair channel from the funder side and confirm it.
	eclair_a.close_channel(&a_t_funder_ch).await.expect("eclair-a close A↔T failed");
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
}

/// Eclair-A → Eclair-T (trampoline) → (unreachable LDK): Eclair as a trampoline
/// forwarder must FAIL because it has no channel/route to the LDK recipient.
///
/// Topology: Eclair-A ↔ Eclair-T only; Eclair-T has no channel to LDK and LDK
/// has no channels at all, so its invoice carries no usable route. When Eclair-A
/// pays naming Eclair-T as trampoline, Eclair-T cannot reach LDK and returns a
/// terminal failure. We assert the payment fails on the Eclair sender side.
pub(crate) async fn trampoline_eclair_forward_unknown_next_scenario<E, NA, NT>(
	node: &Node, eclair_a: &NA, eclair_t: &NT, bitcoind: &BitcoindClient, electrs: &E,
) where
	E: ElectrumApi,
	NA: ExternalNode + ?Sized,
	NT: ExternalNode + ?Sized,
{
	// Eclair-A ↔ Eclair-T only. Deliberately leave Eclair-T with no path to LDK.
	let (a_t_funder_ch, _a_t_fundee_ch) = channel::open_channel_between_externals(
		eclair_a, eclair_t, bitcoind, electrs, 1_000_000, None,
	)
	.await;

	let invoice = node
		.bolt11_payment()
		.receive(
			50_000_000,
			&Bolt11InvoiceDescription::Direct(
				Description::new("trampoline-eclair-forward-fail-test".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	let invoice_str = invoice.to_string();

	let eclair_t_id = eclair_t.get_node_id().await.unwrap();

	let result = eclair_a.pay_trampoline(&invoice_str, eclair_t_id).await;
	assert!(
		result.is_err(),
		"expected trampoline payment to fail (Eclair-T has no route to LDK), got Ok({:?})",
		result.ok()
	);
	println!("Eclair-A → Eclair-T trampoline payment failed as expected: {:?}", result.err());

	wait_for_htlcs_settled(eclair_a, &a_t_funder_ch).await;

	eclair_a.close_channel(&a_t_funder_ch).await.expect("eclair-a close A↔T failed");
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
}
