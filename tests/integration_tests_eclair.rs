// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use common::eclair::TestEclairNode;
use common::scenarios::{
	basic_channel_cycle_scenario, disconnect_during_payment_scenario,
	force_close_after_payment_scenario, keysend_scenario, run_interop_scenario, splice_in_scenario,
};
use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;

/// Unlock all UTXOs in the given bitcoind wallet via JSON-RPC.
async fn unlock_utxos(wallet_url: &str, user: &str, pass: &str) {
	let auth = BASE64_STANDARD.encode(format!("{}:{}", user, pass));
	let body = r#"{"jsonrpc":"1.0","method":"lockunspent","params":[true]}"#;
	let _ = bitreq::post(wallet_url)
		.with_header("Authorization", format!("Basic {}", auth))
		.with_header("Content-Type", "text/plain")
		.with_body(body)
		.with_timeout(5)
		.send_async()
		.await;
}

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestEclairNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443/wallet/ldk_node_test",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	// Unlock any UTXOs left locked by previous force-close tests.
	unlock_utxos("http://127.0.0.1:18443/wallet/eclair-a", "user", "pass").await;

	let eclair = TestEclairNode::from_env();
	(bitcoind, electrs, eclair)
}

async fn setup_clients_two_eclair(
) -> (BitcoindClient, ElectrumClient, TestEclairNode, TestEclairNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443/wallet/ldk_node_test",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	// Unlock UTXOs in both eclair bitcoind wallets in case prior force-close
	// tests left any locked.
	unlock_utxos("http://127.0.0.1:18443/wallet/eclair-a", "user", "pass").await;
	unlock_utxos("http://127.0.0.1:18443/wallet/eclair-b", "user", "pass").await;

	let eclair_a = TestEclairNode::from_env_with_prefix("ECLAIR");
	let eclair_b = TestEclairNode::from_env_with_prefix("ECLAIR_B");
	(bitcoind, electrs, eclair_a, eclair_b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_basic_channel_cycle() {
	run_interop_scenario(setup_clients(), basic_channel_cycle_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_keysend() {
	run_interop_scenario(setup_clients(), keysend_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_force_close_after_payment() {
	run_interop_scenario(setup_clients(), force_close_after_payment_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_disconnect_during_payment() {
	run_interop_scenario(setup_clients(), disconnect_during_payment_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "Eclair advertises splicing via custom bit 154 instead of BOLT bit 62/63; disjoint from LDK until Eclair migrates"]
async fn test_splice_in() {
	run_interop_scenario(setup_clients(), splice_in_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_trampoline_forward() {
	use common::scenarios::run_two_peer_interop_scenario;
	use common::scenarios::trampoline::trampoline_forward_scenario;
	run_two_peer_interop_scenario(setup_clients_two_eclair(), trampoline_forward_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_trampoline_forward_unknown_next() {
	use common::scenarios::run_two_peer_interop_scenario;
	use common::scenarios::trampoline::trampoline_forward_unknown_next_scenario;
	run_two_peer_interop_scenario(
		setup_clients_two_eclair(),
		trampoline_forward_unknown_next_scenario,
	)
	.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "Eclair relays the trampoline payment to LDK, but LDK rejects the final HTLC with \
            IncorrectOrUnknownPaymentDetails: a final-hop payload incompatibility between Eclair's \
            trampoline-to-legacy relay and LDK's claim requirements. ldk-node has no public API to \
            issue a trampoline-recipient-compatible invoice. See docs/trampoline-interop-report.md"]
async fn test_trampoline_receive() {
	use common::scenarios::run_two_peer_interop_scenario;
	use common::scenarios::trampoline::trampoline_receive_scenario;
	run_two_peer_interop_scenario(setup_clients_two_eclair(), trampoline_receive_scenario).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_trampoline_eclair_forward_unknown_next() {
	use common::scenarios::run_two_peer_interop_scenario;
	use common::scenarios::trampoline::trampoline_eclair_forward_unknown_next_scenario;
	run_two_peer_interop_scenario(
		setup_clients_two_eclair(),
		trampoline_eclair_forward_unknown_next_scenario,
	)
	.await;
}
