// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Shared interop test scenarios, generic over `ExternalNode`.
//!
//! - `channel` / `payment` / `connectivity` -- composable building blocks
//! - `interop_tests!` macro -- emits one `#[tokio::test]` per scenario

#[cfg(feature = "_test_utils")]
pub(crate) mod blinded_trampoline;
pub(crate) mod channel;
pub(crate) mod connectivity;
pub(crate) mod payment;
pub(crate) mod trampoline;

use std::future::Future;
use std::time::Duration;

use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};

use super::external_node::ExternalNode;
use super::{generate_blocks_and_wait, premine_and_distribute_funds};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Side {
	Ldk,
	External,
}

/// Retry an async operation with 1s delay; used for ops that may fail due to gossip delay.
pub(crate) async fn retry_until_ok<F, Fut, T, E>(max_attempts: u32, operation: &str, mut f: F) -> T
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<T, E>>,
	E: std::fmt::Display,
{
	for attempt in 1..=max_attempts {
		match f().await {
			Ok(val) => return val,
			Err(e) => {
				if attempt == max_attempts {
					panic!("{} failed after {} attempts: {}", operation, max_attempts, e);
				}
				tokio::time::sleep(Duration::from_secs(1)).await;
			},
		}
	}
	unreachable!()
}

/// Sync wallets, retrying on `WalletOperationTimeout`.
pub(crate) async fn sync_wallets_with_retry(node: &Node) {
	for attempt in 0..3 {
		match node.sync_wallets() {
			Ok(()) => return,
			Err(ldk_node::NodeError::WalletOperationTimeout) if attempt < 2 => {
				tokio::time::sleep(Duration::from_secs(5)).await;
			},
			Err(e) => panic!("sync_wallets failed: {:?}", e),
		}
	}
}

/// Wait until the peer reports 0 pending HTLCs on the channel; required before close because
/// `PaymentSuccessful` fires one round-trip before the HTLC is removed from peer commitment.
pub(crate) async fn wait_for_htlcs_settled(
	peer: &(impl ExternalNode + ?Sized), ext_channel_id: &str,
) {
	for _ in 0..30 {
		let channels = tokio::time::timeout(Duration::from_secs(5), peer.list_channels())
			.await
			.ok()
			.and_then(|r| r.ok());
		if let Some(channels) = channels {
			if let Some(ch) = channels.iter().find(|c| c.channel_id == ext_channel_id) {
				if ch.pending_htlcs_count == 0 {
					return;
				}
			}
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
	panic!("HTLCs did not settle on {} channel {} within 15s", peer.name(), ext_channel_id);
}

/// Build a fresh LDK node configured for interop tests. Uses electrum at the
/// docker-compose default port and bumps sync timeouts for combo stress.
pub(crate) fn setup_ldk_node() -> Node {
	let config = crate::common::random_config(true);
	let mut builder = ldk_node::Builder::from_config(config.node_config);
	let mut sync_config = ldk_node::config::ElectrumSyncConfig::default();
	sync_config.timeouts_config.onchain_wallet_sync_timeout_secs = 180;
	sync_config.timeouts_config.lightning_wallet_sync_timeout_secs = 120;
	builder.set_chain_source_electrum("tcp://127.0.0.1:50001".to_string(), Some(sync_config));
	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();
	node
}

/// Fund both LDK node and external node, connect them.
pub(crate) async fn setup_interop_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let ldk_address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(50_000_000);
	premine_and_distribute_funds(bitcoind, electrs, vec![ldk_address], premine_amount).await;

	// Fund the peer via the ldk_node_test wallet loaded by premine_and_distribute_funds.
	let ext_funding_addr_str = peer.get_funding_address().await.unwrap();
	let ext_amount = Amount::from_sat(50_000_000);
	let amounts_json = serde_json::json!({&ext_funding_addr_str: ext_amount.to_btc()});
	let empty_account = serde_json::json!("");
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external node");
	generate_blocks_and_wait(bitcoind, electrs, 1).await;

	// Block until the peer indexes the funding tx, else channel opens time out.
	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	peer.wait_for_block_sync(chain_height).await.unwrap();

	sync_wallets_with_retry(node).await;

	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();
	node.connect(ext_node_id, ext_addr, true).unwrap();
}

/// Drive a scenario end-to-end: fund LDK + peer, run the scenario, stop the node.
/// Each `#[tokio::test]` in the integration-test files calls this with the
/// per-impl `setup_clients` future and a scenario fn.
pub(crate) async fn run_interop_scenario<N, E, F>(
	setup_fut: impl Future<Output = (BitcoindClient, E, N)>, scenario: F,
) where
	N: ExternalNode,
	E: ElectrumApi,
	F: AsyncFnOnce(&Node, &N, &BitcoindClient, &E),
{
	let (bitcoind, electrs, ext) = setup_fut.await;
	let node = setup_ldk_node();
	setup_interop_test(&node, &ext, &bitcoind, &electrs).await;
	scenario(&node, &ext, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

/// Drive a scenario with two external peers and one LDK node. Premines once
/// and funds LDK + both peers in a single sendmany so block sync is
/// deterministic, then connects LDK to both peers before invoking the
/// scenario.
pub(crate) async fn run_two_peer_interop_scenario<NA, NB, E, F>(
	setup_fut: impl Future<Output = (BitcoindClient, E, NA, NB)>, scenario: F,
) where
	NA: ExternalNode,
	NB: ExternalNode,
	E: ElectrumApi,
	F: AsyncFnOnce(&Node, &NA, &NB, &BitcoindClient, &E),
{
	let (bitcoind, electrs, peer_a, peer_b) = setup_fut.await;
	let node = setup_ldk_node();

	let ldk_address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(50_000_000);
	premine_and_distribute_funds(&bitcoind, &electrs, vec![ldk_address], premine_amount).await;

	let ext_amount = Amount::from_sat(50_000_000);
	let addr_a = peer_a.get_funding_address().await.unwrap();
	let addr_b = peer_b.get_funding_address().await.unwrap();
	let amounts_json = serde_json::json!({
		&addr_a: ext_amount.to_btc(),
		&addr_b: ext_amount.to_btc(),
	});
	let empty_account = serde_json::json!("");
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external nodes");
	generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	peer_a.wait_for_block_sync(chain_height).await.unwrap();
	peer_b.wait_for_block_sync(chain_height).await.unwrap();

	sync_wallets_with_retry(&node).await;

	let id_a = peer_a.get_node_id().await.unwrap();
	let id_b = peer_b.get_node_id().await.unwrap();
	let addr_a_p2p = peer_a.get_listening_address().await.unwrap();
	let addr_b_p2p = peer_b.get_listening_address().await.unwrap();
	node.connect(id_a, addr_a_p2p, true).unwrap();
	node.connect(id_b, addr_b_p2p, true).unwrap();

	scenario(&node, &peer_a, &peer_b, &bitcoind, &electrs).await;
	node.stop().unwrap();
}

/// Open a channel between two LDK nodes (opener funds; `push_msat` flows toward
/// the peer). Mines 10 blocks and waits for both sides' `ChannelReady`. Used for
/// the L1<->L2 leg of the blinded-trampoline channel union.
#[cfg(feature = "_test_utils")]
async fn open_ldk_to_ldk_channel<E: ElectrumApi>(
	opener: &Node, peer: &Node, bitcoind: &BitcoindClient, electrs: &E, funding_amount_sat: u64,
	push_msat: Option<u64>,
) {
	let peer_id = peer.node_id();
	let peer_addr = peer.listening_addresses().unwrap().first().unwrap().clone();
	opener.open_channel(peer_id, peer_addr, funding_amount_sat, push_msat, None).unwrap();

	let funding_txo = expect_channel_pending_event!(opener, peer_id);
	// The fundee also emits its own ChannelPending; consume it now so the later
	// ChannelReady wait on `peer` does not trip over the lingering event (the
	// expect_* macros panic on any non-matching next event).
	let _ = expect_channel_pending_event!(peer, opener.node_id());
	super::wait_for_tx(electrs, funding_txo.txid).await;
	generate_blocks_and_wait(bitcoind, electrs, 10).await;
	sync_wallets_with_retry(opener).await;
	sync_wallets_with_retry(peer).await;
	expect_channel_ready_event!(opener, peer_id);
	expect_channel_ready_event!(peer, opener.node_id());
}

/// Poll until the opener Eclair node reports an active (NORMAL) channel to
/// `target`. Eclair-opened links emit no LDK `ChannelReady`, so we poll
/// `list_channels()` (whose `is_active` is set from `state == "NORMAL"`) to avoid
/// racing channel activation before the combos run.
#[cfg(feature = "_test_utils")]
async fn wait_for_eclair_channel_active(
	opener: &(impl ExternalNode + ?Sized), target: ldk_node::bitcoin::secp256k1::PublicKey,
) {
	let deadline =
		std::time::Instant::now() + Duration::from_secs(crate::common::INTEROP_TIMEOUT_SECS);
	loop {
		if let Ok(channels) = opener.list_channels().await {
			if channels.iter().any(|c| c.peer_id == target && c.is_active) {
				return;
			}
		}
		if std::time::Instant::now() >= deadline {
			panic!(
				"{} channel to {} did not become active within {}s",
				opener.name(),
				target,
				crate::common::INTEROP_TIMEOUT_SECS
			);
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
}

/// Drive a scenario with three external (Eclair) peers and two LDK nodes,
/// provisioning the whole pool exactly once: a single premine, a single
/// `sendmany` funding all three Eclair nodes, a single channel-union open, and a
/// single mesh connect. The scenario closure then loops its combos against this
/// shared pool without re-provisioning.
///
/// Channel union opened (directional `push_msat` chosen so liquidity flows in the
/// payment direction sender→…→L1):
/// `{E1-E2, E2-E3, E3-L1, E2-L2, L2-L1, E1-L2, E2-L1}`.
#[cfg(feature = "_test_utils")]
pub(crate) async fn run_blinded_trampoline_scenario<E1, E2, E3, EL, F>(
	setup_fut: impl Future<Output = (BitcoindClient, EL, E1, E2, E3)>, scenario: F,
) where
	E1: ExternalNode,
	E2: ExternalNode,
	E3: ExternalNode,
	EL: ElectrumApi,
	F: AsyncFnOnce(&Node, &Node, &E1, &E2, &E3, &BitcoindClient, &EL),
{
	let (bitcoind, electrs, e1, e2, e3) = setup_fut.await;
	let l1 = setup_ldk_node();
	let l2 = setup_ldk_node();

	// Premine once: fund both LDK nodes.
	let l1_addr = l1.onchain_payment().new_address().unwrap();
	let l2_addr = l2.onchain_payment().new_address().unwrap();
	premine_and_distribute_funds(
		&bitcoind,
		&electrs,
		vec![l1_addr, l2_addr],
		Amount::from_sat(50_000_000),
	)
	.await;

	// Fund all three Eclair nodes in ONE sendmany so block sync is deterministic.
	let ext_amount = Amount::from_sat(50_000_000);
	let addr_e1 = e1.get_funding_address().await.unwrap();
	let addr_e2 = e2.get_funding_address().await.unwrap();
	let addr_e3 = e3.get_funding_address().await.unwrap();
	let amounts_json = serde_json::json!({
		&addr_e1: ext_amount.to_btc(),
		&addr_e2: ext_amount.to_btc(),
		&addr_e3: ext_amount.to_btc(),
	});
	let empty_account = serde_json::json!("");
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external nodes");
	generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	e1.wait_for_block_sync(chain_height).await.unwrap();
	e2.wait_for_block_sync(chain_height).await.unwrap();
	e3.wait_for_block_sync(chain_height).await.unwrap();
	sync_wallets_with_retry(&l1).await;
	sync_wallets_with_retry(&l2).await;

	// Resolve ids + addrs for the mesh. E1 only ever originates connections, so
	// its own id/addr are not needed here.
	let e2_id = e2.get_node_id().await.unwrap();
	let e3_id = e3.get_node_id().await.unwrap();
	let e2_addr = e2.get_listening_address().await.unwrap();
	let e3_addr = e3.get_listening_address().await.unwrap();
	let l2_id = l2.node_id();
	let l2_addr = l2.listening_addresses().unwrap().first().unwrap().clone();

	// Connect the mesh. LDK side connects to each Eclair neighbor; Eclair-Eclair
	// and Eclair-LDK(l2) links via Eclair's /connect.
	l1.connect(e2_id, e2_addr.clone(), true).unwrap();
	l1.connect(e3_id, e3_addr.clone(), true).unwrap();
	l1.connect(l2_id, l2_addr.clone(), true).unwrap();
	l2.connect(e2_id, e2_addr.clone(), true).unwrap();
	e1.connect_peer(e2_id, e2_addr.clone()).await.unwrap();
	e2.connect_peer(e3_id, e3_addr.clone()).await.unwrap();
	e1.connect_peer(l2_id, l2_addr.clone()).await.unwrap();

	// Open the channel union ONCE, before the scenario closure.
	//
	// LDK-opened legs (emit ChannelReady via open_channel_to_external):
	//   E3-L1: L1 opens, push toward E3 -> E3 outbound to L1 = L1 inbound (recv leg).
	//   L2-L1: L1 opens, push toward L2 -> L2 outbound to L1 (relay leg in combos 2/3).
	//   E2-L1: L1 opens, push toward E2 -> E2 outbound to L1 (recv leg via E2).
	//   E2-L2: L2 opens, push toward E2.
	channel::open_channel_to_external(&l1, &e3, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
		.await;
	// L2-L1 is LDK<->LDK; open directly (push toward L2 so L2 has outbound to L1).
	open_ldk_to_ldk_channel(&l1, &l2, &bitcoind, &electrs, 1_000_000, Some(500_000_000)).await;
	channel::open_channel_to_external(&l1, &e2, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
		.await;
	channel::open_channel_to_external(&l2, &e2, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
		.await;

	// Eclair-opened legs (no LDK ChannelReady; poll for NORMAL afterwards):
	//   E1-E2: E1 needs outbound to E2 (trampolineNodeId=E2 in combo3).
	//   E2-E3: E2 (intro) needs outbound to E3 (relay in combo1).
	//   E1-L2: E1 needs outbound to L2 (trampolineNodeId=L2 in combos 1&2).
	e1.open_channel(e2_id, e2_addr.clone(), 1_000_000, Some(500_000_000)).await.unwrap();
	e2.open_channel(e3_id, e3_addr.clone(), 1_000_000, Some(500_000_000)).await.unwrap();
	e1.open_channel(l2_id, l2_addr.clone(), 1_000_000, Some(500_000_000)).await.unwrap();

	// Mine + sync so the Eclair-opened funding txs confirm to NORMAL.
	generate_blocks_and_wait(&bitcoind, &electrs, 10).await;
	sync_wallets_with_retry(&l1).await;
	sync_wallets_with_retry(&l2).await;
	let chain_height: u64 = bitcoind.get_blockchain_info().unwrap().blocks.try_into().unwrap();
	e1.wait_for_block_sync(chain_height).await.unwrap();
	e2.wait_for_block_sync(chain_height).await.unwrap();
	e3.wait_for_block_sync(chain_height).await.unwrap();

	wait_for_eclair_channel_active(&e1, e2_id).await;
	wait_for_eclair_channel_active(&e2, e3_id).await;
	wait_for_eclair_channel_active(&e1, l2_id).await;

	scenario(&l1, &l2, &e1, &e2, &e3, &bitcoind, &electrs).await;
	l1.stop().unwrap();
	l2.stop().unwrap();
}

/// Open a channel, send a BOLT11 payment in each direction, then cooperatively close.
pub(crate) async fn basic_channel_cycle_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	payment::send_bolt11_to_peer(node, peer, 10_000_000, "basic-send").await;
	payment::receive_bolt11_payment(node, peer, 10_000_000).await;

	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Plain (non-trampoline) BOLT12 control: LDK serves an ordinary offer and the
/// external peer (Eclair) pays it via its offer-payment endpoint, exercising the
/// BOLT12 invoice_request -> invoice exchange over onion messages plus payment
/// over LDK's blinded payment path. Used to determine whether a BOLT12 interop
/// failure is general or specific to trampoline blinded paths.
pub(crate) async fn bolt12_offer_payment_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	// LDK opens to the peer, pushing balance so LDK has inbound liquidity to
	// receive, and the peer becomes the blinded-path introduction node.
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;

	let offer = node
		.bolt12_payment()
		.receive(50_000_000, "bolt12-offer-interop", Some(3600), None)
		.expect("LDK create offer failed");
	let offer_str = offer.to_string();

	peer.pay_offer(&offer_str, 50_000_000).await.expect("eclair pay_offer failed");

	expect_payment_received_event!(node, 50_000_000);

	wait_for_htlcs_settled(peer, &ext_ch).await;
	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, send keysend in both directions, then cooperatively close.
pub(crate) async fn keysend_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	payment::send_keysend_to_peer(node, peer, 5_000_000).await;
	payment::receive_keysend_payment(node, peer, 5_000_000).await;
	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, send a payment, then force-close from the LDK side.
pub(crate) async fn force_close_after_payment_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	payment::send_bolt11_to_peer(node, peer, 5_000_000, "force-close").await;
	wait_for_htlcs_settled(peer, &ext_ch).await;
	channel::force_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, dispatch a payment with a mid-flight disconnect+reconnect,
/// then cooperatively close.
pub(crate) async fn disconnect_during_payment_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	connectivity::disconnect_during_payment(node, peer, &Side::Ldk).await;
	wait_for_htlcs_settled(peer, &ext_ch).await;
	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}

/// Open a channel, splice-in additional funds, send a post-splice payment, then close.
pub(crate) async fn splice_in_scenario<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	let (user_ch, ext_ch) = channel::open_channel_to_external(
		node,
		peer,
		bitcoind,
		electrs,
		1_000_000,
		Some(500_000_000),
	)
	.await;
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.splice_in(&user_ch, ext_node_id, 500_000).unwrap();
	expect_splice_pending_event!(node, ext_node_id);
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	sync_wallets_with_retry(node).await;
	expect_channel_ready_event!(node, ext_node_id);

	payment::send_bolt11_to_peer(node, peer, 5_000_000, "post-splice").await;

	channel::cooperative_close(node, peer, bitcoind, electrs, &user_ch, &ext_ch, Side::Ldk).await;
}
