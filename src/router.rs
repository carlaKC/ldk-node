// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! A [`Router`] wrapper that can build a caller-specified blinded trampoline path.

use std::sync::{Arc, Mutex};

use bitcoin::secp256k1::{self, PublicKey, Secp256k1};

use lightning::blinded_path::payment::{
	BlindedPaymentPath, ForwardNode, PaymentConstraints, PaymentRelay, ReceiveTlvs,
	TrampolineForwardTlvs,
};
use lightning::ln::channel_state::ChannelDetails;
use lightning::ln::channelmanager::{PaymentId, MIN_FINAL_CLTV_EXPIRY_DELTA};
use lightning::routing::router::{DefaultRouter, InFlightHtlcs, Route, RouteParameters, Router};
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::sign::ReceiveAuthKey;
use lightning_types::features::BlindedHopFeatures;
use lightning_types::payment::PaymentHash;

use crate::logger::Logger;
use crate::types::{Graph, KeysManager, Scorer};

/// A CLTV expiry delta used for each fabricated trampoline hop's [`PaymentRelay`].
///
/// This must be consistent with the CLTV expiry delta configured on the real channels that the
/// trampoline route traverses, otherwise the payment will fail at forward time even though path
/// construction succeeds.
const TRAMPOLINE_HOP_CLTV_EXPIRY_DELTA: u16 = 144;

/// A [`Router`] that delegates every method to an inner [`DefaultRouter`], EXCEPT
/// [`Router::create_blinded_payment_paths`] when a trampoline path has been configured.
///
/// When an ordered intermediate-node list has been set via [`Self::set_trampoline_path`], the
/// router builds exactly one trampoline [`BlindedPaymentPath`] (via
/// [`BlindedPaymentPath::new_for_trampoline`]) at invoice-build time instead of delegating. When
/// unset, behavior is identical to the inner [`DefaultRouter`].
pub(crate) struct TrampolineAwareRouter {
	inner: DefaultRouter<
		Arc<Graph>,
		Arc<Logger>,
		Arc<KeysManager>,
		Arc<Mutex<Scorer>>,
		ProbabilisticScoringFeeParameters,
		Scorer,
	>,
	entropy_source: Arc<KeysManager>,
	trampoline_path: Mutex<Option<Vec<PublicKey>>>,
}

impl TrampolineAwareRouter {
	pub(crate) fn new(
		inner: DefaultRouter<
			Arc<Graph>,
			Arc<Logger>,
			Arc<KeysManager>,
			Arc<Mutex<Scorer>>,
			ProbabilisticScoringFeeParameters,
			Scorer,
		>,
		entropy_source: Arc<KeysManager>,
	) -> Self {
		Self { inner, entropy_source, trampoline_path: Mutex::new(None) }
	}

	/// Sets (or clears) the ordered list of intermediate nodes that should be encoded into a
	/// trampoline blinded payment path when the next BOLT12 invoice is built.
	///
	/// The list is ordered introduction-node first, followed by each subsequent relay. The final
	/// configured hop forwards to the recipient (our own node).
	///
	/// Only reachable via the test-only `Node::set_trampoline_blinded_path` helper, so it is
	/// otherwise dead code in production builds.
	#[cfg_attr(not(feature = "_test_utils"), allow(dead_code))]
	pub(crate) fn set_trampoline_path(&self, nodes: Option<Vec<PublicKey>>) {
		*self.trampoline_path.lock().unwrap() = nodes;
	}
}

impl Router for TrampolineAwareRouter {
	fn find_route(
		&self, payer: &PublicKey, route_params: &RouteParameters,
		first_hops: Option<&[&ChannelDetails]>, inflight_htlcs: InFlightHtlcs,
	) -> Result<Route, &'static str> {
		self.inner.find_route(payer, route_params, first_hops, inflight_htlcs)
	}

	fn find_route_with_id(
		&self, payer: &PublicKey, route_params: &RouteParameters,
		first_hops: Option<&[&ChannelDetails]>, inflight_htlcs: InFlightHtlcs,
		payment_hash: PaymentHash, payment_id: PaymentId,
	) -> Result<Route, &'static str> {
		self.inner.find_route_with_id(
			payer,
			route_params,
			first_hops,
			inflight_htlcs,
			payment_hash,
			payment_id,
		)
	}

	fn create_blinded_payment_paths<T: secp256k1::Signing + secp256k1::Verification>(
		&self, recipient: PublicKey, local_node_receive_key: ReceiveAuthKey,
		first_hops: Vec<ChannelDetails>, tlvs: ReceiveTlvs, amount_msats: Option<u64>,
		secp_ctx: &Secp256k1<T>,
	) -> Result<Vec<BlindedPaymentPath>, ()> {
		let trampoline_path = self.trampoline_path.lock().unwrap().clone();

		let nodes = match trampoline_path {
			None => {
				return self.inner.create_blinded_payment_paths(
					recipient,
					local_node_receive_key,
					first_hops,
					tlvs,
					amount_msats,
					secp_ctx,
				);
			},
			Some(nodes) => nodes,
		};

		// Build the ordered list of trampoline forward nodes. For hop `i`, the next trampoline node
		// is hop `i + 1`'s node id; for the last configured hop the next trampoline node is the
		// recipient (our own node id).
		let htlc_maximum_msat = amount_msats.unwrap_or(u64::MAX);
		let forward_nodes: Vec<ForwardNode<TrampolineForwardTlvs>> = nodes
			.iter()
			.enumerate()
			.map(|(i, node_id)| {
				let next_trampoline = nodes.get(i + 1).copied().unwrap_or(recipient);
				ForwardNode {
					node_id: *node_id,
					tlvs: TrampolineForwardTlvs {
						next_trampoline,
						payment_relay: PaymentRelay {
							cltv_expiry_delta: TRAMPOLINE_HOP_CLTV_EXPIRY_DELTA,
							fee_proportional_millionths: 0,
							fee_base_msat: 0,
						},
						payment_constraints: PaymentConstraints {
							max_cltv_expiry: tlvs.payment_constraints.max_cltv_expiry,
							htlc_minimum_msat: 0,
						},
						features: BlindedHopFeatures::empty(),
						next_blinding_override: None,
					},
					htlc_maximum_msat,
				}
			})
			.collect();

		let path = BlindedPaymentPath::new_for_trampoline(
			&forward_nodes,
			recipient,
			local_node_receive_key,
			tlvs.clone(),
			u64::MAX,
			MIN_FINAL_CLTV_EXPIRY_DELTA,
			&self.entropy_source,
			secp_ctx,
		)?;

		Ok(vec![path])
	}
}
