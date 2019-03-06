//! The `gossip_service` module implements the network control plane.

use crate::bank_forks::BankForks;
use crate::blocktree::Blocktree;
use crate::cluster_info::{ClusterInfo, Node, NodeInfo};
use crate::service::Service;
use crate::streamer;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::thread::sleep;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct GossipService {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl GossipService {
    pub fn new(
        cluster_info: &Arc<RwLock<ClusterInfo>>,
        blocktree: Option<Arc<Blocktree>>,
        bank_forks: Option<Arc<RwLock<BankForks>>>,
        gossip_socket: UdpSocket,
        exit: &Arc<AtomicBool>,
    ) -> Self {
        let (request_sender, request_receiver) = channel();
        let gossip_socket = Arc::new(gossip_socket);
        trace!(
            "GossipService: id: {}, listening on: {:?}",
            &cluster_info.read().unwrap().my_data().id,
            gossip_socket.local_addr().unwrap()
        );
        let t_receiver = streamer::blob_receiver(gossip_socket.clone(), &exit, request_sender);
        let (response_sender, response_receiver) = channel();
        let t_responder = streamer::responder("gossip", gossip_socket, response_receiver);
        let t_listen = ClusterInfo::listen(
            cluster_info.clone(),
            blocktree,
            request_receiver,
            response_sender.clone(),
            exit,
        );
        let t_gossip = ClusterInfo::gossip(cluster_info.clone(), bank_forks, response_sender, exit);
        let thread_hdls = vec![t_receiver, t_responder, t_listen, t_gossip];
        Self { thread_hdls }
    }
}

pub fn make_listening_node(
    leader: &NodeInfo,
) -> (GossipService, Arc<RwLock<ClusterInfo>>, Node, Pubkey) {
    let keypair = Keypair::new();
    let exit = Arc::new(AtomicBool::new(false));
    let new_node = Node::new_localhost_with_pubkey(keypair.pubkey());
    let new_node_info = new_node.info.clone();
    let id = new_node.info.id;
    let mut new_node_cluster_info = ClusterInfo::new(new_node_info, Arc::new(keypair));
    new_node_cluster_info.insert_info(leader.clone());
    new_node_cluster_info.set_leader(leader.id);
    let new_node_cluster_info_ref = Arc::new(RwLock::new(new_node_cluster_info));
    let gossip_service = GossipService::new(
        &new_node_cluster_info_ref,
        None,
        None,
        new_node
            .sockets
            .gossip
            .try_clone()
            .expect("Failed to clone gossip"),
        &exit,
    );

    (gossip_service, new_node_cluster_info_ref, new_node, id)
}

pub fn discover(entry_point_info: &NodeInfo, num_nodes: usize) -> Vec<NodeInfo> {
    info!("Wait for convergence with {} nodes", num_nodes);

    let exit = Arc::new(AtomicBool::new(false));
    let (gossip_service, spy_ref, id) = make_spy_node(entry_point_info, &exit);
    trace!(
        "discover: spy_node {} looking for at least {} nodes",
        id,
        num_nodes
    );

    // Wait for the cluster to converge
    for _ in 0..15 {
        let rpc_peers = spy_ref.read().unwrap().rpc_peers();
        info!(
            "discover: spy_node {} found {}/{} nodes",
            id,
            rpc_peers.len(),
            num_nodes,
        );
        if rpc_peers.len() >= num_nodes {
            exit.store(true, Ordering::Relaxed);
            gossip_service.join().unwrap();
            return rpc_peers;
        }
        debug!(
            "discover: expecting an additional {} nodes",
            num_nodes - rpc_peers.len()
        );
        sleep(Duration::new(1, 0));
    }
    panic!("Failed to converge");
}

fn make_spy_node(
    leader: &NodeInfo,
    exit: &Arc<AtomicBool>,
) -> (GossipService, Arc<RwLock<ClusterInfo>>) {
    let keypair = Arc::new(Keypair::new());
    let (node, gossip_socket) = ClusterInfo::spy_node(&keypair.pubkey());

    let mut cluster_info = ClusterInfo::new(node, keypair);
    cluster_info.insert_info(entry_point.clone());

    let cluster_info = Arc::new(RwLock::new(cluster_info));
    let gossip_service =
        GossipService::new(&spy_cluster_info_ref, None, None, spy.sockets.gossip, exit);

    (gossip_service, spy_cluster_info_ref, id)
}

impl Service for GossipService {
    type JoinReturnType = ();

    fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_info::{ClusterInfo, Node};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, RwLock};

    #[test]
    #[ignore]
    // test that stage will exit when flag is set
    fn test_exit() {
        let exit = Arc::new(AtomicBool::new(false));
        let tn = Node::new_localhost();
        let cluster_info = ClusterInfo::new_with_invalid_keypair(tn.info.clone());
        let c = Arc::new(RwLock::new(cluster_info));
        let d = GossipService::new(&c, None, None, tn.sockets.gossip, &exit);
        exit.store(true, Ordering::Relaxed);
        d.join().unwrap();
    }
}
