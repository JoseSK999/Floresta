// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression tests for peer scheduling, shared latency, and timeout handling.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    use bitcoin::Network;
    use bitcoin::bip158::BlockFilter;
    use bitcoin::p2p::ServiceFlags;
    use floresta_chain::ChainState;
    use floresta_chain::FlatChainStore;
    use floresta_chain::pruned_utreexo::BlockchainInterface;
    use floresta_common::Ema;

    use crate::node::ConnectionKind;
    use crate::node::InflightRequests;
    use crate::node::PeerStatus;
    use crate::node::UtreexoNode;
    use crate::node::running_ctx::RunningNode;
    use crate::node::sync_ctx::SyncNode;
    use crate::node_context::NodeContext;
    use crate::p2p_wire::error::WireError;
    use crate::p2p_wire::peer::PeerMessages;
    use crate::p2p_wire::tests::utils::PeerData;
    use crate::p2p_wire::tests::utils::SetupNodeArgs;
    use crate::p2p_wire::tests::utils::setup_node;
    use crate::p2p_wire::tests::utils::signet_blocks;

    const NUM_BLOCKS: usize = 9;

    /// Creates two simulated peers with known initial latency samples.
    fn latency_node<T: 'static + Default + NodeContext>()
    -> UtreexoNode<Arc<ChainState<FlatChainStore>>, T> {
        let peer = PeerData::new(Vec::new(), signet_blocks(), HashMap::new());
        let args = SetupNodeArgs::new(
            vec![peer; 2],
            false,
            Network::Signet,
            format!("./tmp-db/{}.sync_latency", rand::random::<u32>()),
            NUM_BLOCKS,
        );
        let mut node = setup_node::<T>(args);
        for peer in node.peers.values_mut() {
            peer.services |= ServiceFlags::NETWORK;
            peer.message_times.add(1.0);
        }
        node.inflight.clear();
        node
    }

    #[tokio::test]
    async fn test_block_batches_balance_outstanding_work_and_refill_faster_peers() {
        let mut node = latency_node::<SyncNode>();
        node.peers.get_mut(&1).unwrap().message_times.add(10_000.0);
        let hashes = (1..=6)
            .map(|height| node.chain.get_block_hash(height).unwrap())
            .collect::<Vec<_>>();

        // Empty queues tie, so the lower general latency gets the first batch.
        node.request_blocks(hashes[..2].to_vec()).unwrap();
        for hash in &hashes[..2] {
            assert_eq!(node.inflight[&InflightRequests::Blocks(*hash)].0, 0);
        }

        // Outstanding blocks take priority over that much faster latency score.
        node.request_blocks(hashes[2..4].to_vec()).unwrap();
        for hash in &hashes[2..4] {
            assert_eq!(node.inflight[&InflightRequests::Blocks(*hash)].0, 1);
        }

        // Finishing first earns replacement work without increasing the slow peer's queue.
        for hash in &hashes[..2] {
            node.inflight.remove(&InflightRequests::Blocks(*hash));
        }
        node.request_blocks(hashes[4..].to_vec()).unwrap();
        for hash in &hashes[4..] {
            assert_eq!(node.inflight[&InflightRequests::Blocks(*hash)].0, 0);
        }
        assert_eq!(node.inflight.len(), 4);
    }

    #[tokio::test]
    async fn test_block_scheduling_ignores_other_outstanding_requests() {
        let mut node = latency_node::<SyncNode>();
        node.peers.get_mut(&1).unwrap().message_times.add(10_000.0);
        let hash = node.chain.get_block_hash(1).unwrap();
        for request in [
            InflightRequests::Headers,
            InflightRequests::GetFilters,
            InflightRequests::UtreexoState(0),
            InflightRequests::UtreexoProof(hash),
        ] {
            node.inflight.insert(request, (0, Instant::now()));
        }

        // Both block queues are empty, even though peer 0 has other work outstanding.
        node.request_blocks(vec![hash]).unwrap();
        assert_eq!(node.inflight[&InflightRequests::Blocks(hash)].0, 0);
    }

    #[tokio::test]
    async fn test_block_scheduling_needs_ready_service_peers_but_no_latency_samples() {
        let mut node = latency_node::<SyncNode>();
        node.peers.get_mut(&0).unwrap().message_times = Ema::with_half_life_50();
        node.peers.get_mut(&1).unwrap().state = PeerStatus::Awaiting;
        let mut wrong_service = node.peers[&0].clone();
        wrong_service.services = ServiceFlags::NONE;
        node.peers.insert(2, wrong_service);
        for (id, kind) in [(3, ConnectionKind::Feeler), (4, ConnectionKind::Extra)] {
            let mut peer = node.peers[&0].clone();
            peer.kind = kind;
            node.peers.insert(id, peer);
        }
        let mut banned = node.peers[&0].clone();
        banned.state = PeerStatus::Banned;
        node.peers.insert(5, banned);

        for (height, kind) in [
            (1, ConnectionKind::Regular(ServiceFlags::NETWORK)),
            (2, ConnectionKind::Manual),
        ] {
            node.peers.get_mut(&0).unwrap().kind = kind;
            let hash = node.chain.get_block_hash(height).unwrap();
            node.request_blocks(vec![hash]).unwrap();
            assert_eq!(node.inflight[&InflightRequests::Blocks(hash)].0, 0);
        }
        assert_eq!(node.peers[&0].message_times.value(), None);

        node.peers.get_mut(&0).unwrap().state = PeerStatus::Awaiting;
        let hash = node.chain.get_block_hash(3).unwrap();
        assert!(matches!(
            node.request_blocks(vec![hash]),
            Err(WireError::NoPeersAvailable)
        ));
        assert!(!node.inflight.contains_key(&InflightRequests::Blocks(hash)));
    }

    #[tokio::test]
    async fn test_failed_retries_preserve_requests_and_repeat_timeout_samples() {
        let mut node = latency_node::<SyncNode>();
        // No peer can accept a retry, regardless of the requested service.
        for peer in node.peers.values_mut() {
            peer.state = PeerStatus::Awaiting;
        }
        let expired = Instant::now() - Duration::from_secs(SyncNode::REQUEST_TIMEOUT + 1);
        for request in [
            InflightRequests::Blocks(node.chain.get_block_hash(1).unwrap()),
            InflightRequests::Headers,
            InflightRequests::GetFilters,
            InflightRequests::UtreexoState(0),
        ] {
            node.inflight.insert(request, (0, expired));
        }
        let original = node.inflight.clone();
        let mut expected = node.peers[&0].message_times.clone();
        for check in 1..=2 {
            assert!(node.check_for_timeout().is_err());
            assert_eq!(node.inflight, original);
            // Still-expired requests are retried and penalized again on the next check.
            for _ in 0..original.len() {
                expected.add(SyncNode::REQUEST_TIMEOUT as f64 * 1_000.0);
            }
            assert_eq!(node.peers[&0].message_times.value(), expected.value());
            assert_eq!(node.peers[&0].banscore, check * 4);
        }
    }

    #[tokio::test]
    async fn test_reply_latency_uses_only_the_current_peer_and_timestamp() {
        let mut node = latency_node::<SyncNode>();
        let hash = node.chain.get_block_hash(1).unwrap();
        let block = signet_blocks().remove(&hash).unwrap();
        for (request, message) in [
            (InflightRequests::Blocks(hash), PeerMessages::Block(block)),
            (InflightRequests::Headers, PeerMessages::Headers(Vec::new())),
            (
                InflightRequests::GetFilters,
                PeerMessages::BlockFilter((hash, BlockFilter::new(&[]))),
            ),
            (
                InflightRequests::UtreexoState(1),
                PeerMessages::UtreexoState(Vec::new()),
            ),
        ] {
            let sent_at = Instant::now();
            node.inflight.insert(request, (1, sent_at));
            let mut expected = node.peers[&1].message_times.clone();
            assert_eq!(
                node.register_message_time(&message, 0, sent_at + Duration::from_secs(5)),
                None
            );
            assert_eq!(
                node.register_message_time(&message, 1, sent_at - Duration::from_millis(1)),
                None
            );
            assert_eq!(node.peers[&0].message_times.value(), Some(1.0));
            assert_eq!(node.peers[&1].message_times.value(), expected.value());

            expected.add(5_000.0);
            assert_eq!(
                node.register_message_time(&message, 1, sent_at + Duration::from_secs(5)),
                Some(())
            );
            assert_eq!(node.peers[&1].message_times.value(), expected.value());
            node.inflight.clear();
        }
    }

    #[tokio::test]
    async fn test_running_node_scores_timeouts_and_accepts_same_peer_retry_samples() {
        let mut node = latency_node::<RunningNode>();
        node.peers.get_mut(&1).unwrap().state = PeerStatus::Awaiting;
        let hash = node.chain.get_block_hash(1).unwrap();
        let request = InflightRequests::Blocks(hash);
        let expired = Instant::now() - Duration::from_secs(RunningNode::REQUEST_TIMEOUT + 1);
        node.inflight.insert(request.clone(), (0, expired));
        let mut expected = node.peers[&0].message_times.clone();
        expected.add(RunningNode::REQUEST_TIMEOUT as f64 * 1_000.0);

        node.check_for_timeout().unwrap();
        let (peer, retried_at) = node.inflight[&request];
        assert_eq!(peer, 0);
        assert!(retried_at > expired);
        assert_eq!(node.peers[&0].message_times.value(), expected.value());
        assert_eq!(node.peers[&0].banscore, 1);

        // The reply could be from either attempt; accepting it keeps the timeout sample too.
        let message = PeerMessages::Block(signet_blocks().remove(&hash).unwrap());
        expected.add(1.0);
        assert_eq!(
            node.register_message_time(&message, peer, retried_at + Duration::from_millis(1)),
            Some(())
        );
        assert_eq!(node.peers[&0].message_times.value(), expected.value());
    }
}
