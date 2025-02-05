use crate::node::NetworkGraph;
use crate::storage::MutinyStorage;
use crate::{error::MutinyError, fees::MutinyFeeEstimator};
use crate::{gossip, ldkstorage::PhantomChannelManager, logging::MutinyLogger};
use crate::{gossip::read_peer_info, node::PubkeyConnectionInfo};
use crate::{keymanager::PhantomKeysManager, node::ConnectionType};
use bitcoin::secp256k1::PublicKey;
use lightning::{
    ln::{msgs::NetAddress, peer_handler::SocketDescriptor as LdkSocketDescriptor},
    log_debug, log_trace,
};
use std::{net::SocketAddr, sync::atomic::AtomicBool};

use crate::networking::socket::{schedule_descriptor_read, MutinySocketDescriptor};
use crate::scb::message_handler::SCBMessageHandler;
use bitcoin::BlockHash;
use lightning::events::{MessageSendEvent, MessageSendEventsProvider};
use lightning::ln::features::{InitFeatures, NodeFeatures};
use lightning::ln::msgs;
use lightning::ln::msgs::{LightningError, RoutingMessageHandler};
use lightning::ln::peer_handler::PeerHandleError;
use lightning::ln::peer_handler::{IgnoringMessageHandler, PeerManager as LdkPeerManager};
use lightning::log_warn;
use lightning::routing::gossip::NodeId;
use lightning::routing::utxo::{UtxoLookup, UtxoLookupError, UtxoResult};
use lightning::util::logger::Logger;
use std::sync::Arc;

#[cfg(target_arch = "wasm32")]
use crate::networking::ws_socket::WsTcpSocketDescriptor;

#[cfg(target_arch = "wasm32")]
use crate::networking::proxy::WsProxy;

#[cfg(not(target_arch = "wasm32"))]
use tokio::time;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use tokio::net::TcpStream;

#[cfg(not(target_arch = "wasm32"))]
use crate::networking::tcp_socket::TcpSocketDescriptor;

pub trait PeerManager {
    fn get_peer_node_ids(&self) -> Vec<PublicKey>;

    fn new_outbound_connection(
        &self,
        their_node_id: PublicKey,
        descriptor: MutinySocketDescriptor,
        remote_network_address: Option<NetAddress>,
    ) -> Result<Vec<u8>, PeerHandleError>;

    fn new_inbound_connection(
        &self,
        descriptor: MutinySocketDescriptor,
        remote_network_address: Option<NetAddress>,
    ) -> Result<(), PeerHandleError>;

    fn write_buffer_space_avail(
        &self,
        descriptor: &mut MutinySocketDescriptor,
    ) -> Result<(), PeerHandleError>;

    fn read_event(
        &self,
        descriptor: &mut MutinySocketDescriptor,
        data: &[u8],
    ) -> Result<bool, PeerHandleError>;

    fn process_events(&self);

    fn socket_disconnected(&self, descriptor: &mut MutinySocketDescriptor);

    fn disconnect_by_node_id(&self, node_id: PublicKey);

    fn disconnect_all_peers(&self);

    fn timer_tick_occurred(&self);

    fn broadcast_node_announcement(
        &self,
        rgb: [u8; 3],
        alias: [u8; 32],
        addresses: Vec<NetAddress>,
    );
}

pub(crate) type PeerManagerImpl<S: MutinyStorage> = LdkPeerManager<
    MutinySocketDescriptor,
    Arc<PhantomChannelManager<S>>,
    Arc<GossipMessageHandler<S>>,
    Arc<IgnoringMessageHandler>,
    Arc<MutinyLogger>,
    Arc<SCBMessageHandler>,
    Arc<PhantomKeysManager<S>>,
>;

impl<S: MutinyStorage> PeerManager for PeerManagerImpl<S> {
    fn get_peer_node_ids(&self) -> Vec<PublicKey> {
        self.get_peer_node_ids().into_iter().map(|x| x.0).collect()
    }

    fn new_outbound_connection(
        &self,
        their_node_id: PublicKey,
        descriptor: MutinySocketDescriptor,
        remote_network_address: Option<NetAddress>,
    ) -> Result<Vec<u8>, PeerHandleError> {
        self.new_outbound_connection(their_node_id, descriptor, remote_network_address)
    }

    fn new_inbound_connection(
        &self,
        descriptor: MutinySocketDescriptor,
        remote_network_address: Option<NetAddress>,
    ) -> Result<(), PeerHandleError> {
        self.new_inbound_connection(descriptor, remote_network_address)
    }

    fn write_buffer_space_avail(
        &self,
        descriptor: &mut MutinySocketDescriptor,
    ) -> Result<(), PeerHandleError> {
        self.write_buffer_space_avail(descriptor)
    }

    fn read_event(
        &self,
        peer_descriptor: &mut MutinySocketDescriptor,
        data: &[u8],
    ) -> Result<bool, PeerHandleError> {
        self.read_event(peer_descriptor, data)
    }

    fn process_events(&self) {
        self.process_events()
    }

    fn socket_disconnected(&self, descriptor: &mut MutinySocketDescriptor) {
        self.socket_disconnected(descriptor)
    }

    fn disconnect_by_node_id(&self, node_id: PublicKey) {
        self.disconnect_by_node_id(node_id)
    }

    fn disconnect_all_peers(&self) {
        self.disconnect_all_peers()
    }

    fn timer_tick_occurred(&self) {
        self.timer_tick_occurred()
    }

    fn broadcast_node_announcement(
        &self,
        rgb: [u8; 3],
        alias: [u8; 32],
        addresses: Vec<NetAddress>,
    ) {
        self.broadcast_node_announcement(rgb, alias, addresses)
    }
}

#[derive(Clone)]
pub struct GossipMessageHandler<S: MutinyStorage> {
    pub(crate) storage: S,
    pub(crate) network_graph: Arc<NetworkGraph>,
    pub(crate) logger: Arc<MutinyLogger>,
}

impl<S: MutinyStorage> MessageSendEventsProvider for GossipMessageHandler<S> {
    fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
        Vec::new()
    }
}

// I needed some type to implement RoutingMessageHandler, but I don't want to implement it
// we don't need to lookup UTXOs, so we can just return an error
// This should never actually be called because we are passing in None for the UTXO lookup
struct ErroringUtxoLookup {}
impl UtxoLookup for ErroringUtxoLookup {
    fn get_utxo(&self, _genesis_hash: &BlockHash, _short_channel_id: u64) -> UtxoResult {
        UtxoResult::Sync(Err(UtxoLookupError::UnknownTx))
    }
}

impl<S: MutinyStorage> RoutingMessageHandler for GossipMessageHandler<S> {
    fn handle_node_announcement(
        &self,
        msg: &msgs::NodeAnnouncement,
    ) -> Result<bool, LightningError> {
        // We use RGS to sync gossip, but we can save the node's metadata (alias and color)
        // we should only save it for relevant peers however (i.e. peers we have a channel with)
        let node_id = msg.contents.node_id;
        if read_peer_info(&self.storage, &node_id)
            .ok()
            .flatten()
            .is_some()
        {
            if let Err(e) = gossip::save_ln_peer_info(&self.storage, &node_id, &msg.clone().into())
            {
                log_warn!(
                    self.logger,
                    "Failed to save node announcement for {node_id}: {e}"
                );
            }
        }

        // because we got the announcement, may as well update our network graph
        self.network_graph
            .update_node_from_unsigned_announcement(&msg.contents)?;

        Ok(false)
    }

    fn handle_channel_announcement(
        &self,
        msg: &msgs::ChannelAnnouncement,
    ) -> Result<bool, LightningError> {
        // because we got the channel, may as well update our network graph
        self.network_graph
            .update_channel_from_announcement_no_lookup(msg)?;
        Ok(false)
    }

    fn handle_channel_update(&self, msg: &msgs::ChannelUpdate) -> Result<bool, LightningError> {
        // because we got the update, may as well update our network graph
        self.network_graph.update_channel_unsigned(&msg.contents)?;
        Ok(false)
    }

    fn get_next_channel_announcement(
        &self,
        _starting_point: u64,
    ) -> Option<(
        msgs::ChannelAnnouncement,
        Option<msgs::ChannelUpdate>,
        Option<msgs::ChannelUpdate>,
    )> {
        None
    }

    fn get_next_node_announcement(
        &self,
        _starting_point: Option<&NodeId>,
    ) -> Option<msgs::NodeAnnouncement> {
        None
    }

    fn peer_connected(
        &self,
        _their_node_id: &PublicKey,
        _init: &msgs::Init,
        _inbound: bool,
    ) -> Result<(), ()> {
        Ok(())
    }

    fn handle_reply_channel_range(
        &self,
        _their_node_id: &PublicKey,
        _msg: msgs::ReplyChannelRange,
    ) -> Result<(), LightningError> {
        Ok(())
    }

    fn handle_reply_short_channel_ids_end(
        &self,
        _their_node_id: &PublicKey,
        _msg: msgs::ReplyShortChannelIdsEnd,
    ) -> Result<(), LightningError> {
        Ok(())
    }

    fn handle_query_channel_range(
        &self,
        _their_node_id: &PublicKey,
        _msg: msgs::QueryChannelRange,
    ) -> Result<(), LightningError> {
        Ok(())
    }

    fn handle_query_short_channel_ids(
        &self,
        _their_node_id: &PublicKey,
        _msg: msgs::QueryShortChannelIds,
    ) -> Result<(), LightningError> {
        Ok(())
    }

    fn processing_queue_high(&self) -> bool {
        false
    }

    fn provided_node_features(&self) -> NodeFeatures {
        NodeFeatures::empty()
    }

    fn provided_init_features(&self, _their_node_id: &PublicKey) -> InitFeatures {
        InitFeatures::empty()
    }
}

pub(crate) async fn connect_peer_if_necessary<S: MutinyStorage>(
    #[cfg(target_arch = "wasm32")] websocket_proxy_addr: &str,
    peer_connection_info: &PubkeyConnectionInfo,
    logger: Arc<MutinyLogger>,
    peer_manager: Arc<dyn PeerManager>,
    fee_estimator: Arc<MutinyFeeEstimator<S>>,
    stop: Arc<AtomicBool>,
) -> Result<(), MutinyError> {
    if peer_manager
        .get_peer_node_ids()
        .contains(&peer_connection_info.pubkey)
    {
        Ok(())
    } else {
        // first check to see if the fee rate is mostly up to date
        // if not, we need to have updated fees or force closures
        // could occur due to UpdateFee message conflicts.
        fee_estimator.update_fee_estimates_if_necessary().await?;

        connect_peer(
            #[cfg(target_arch = "wasm32")]
            websocket_proxy_addr,
            peer_connection_info,
            logger,
            peer_manager,
            stop,
        )
        .await
    }
}

async fn connect_peer(
    #[cfg(target_arch = "wasm32")] websocket_proxy_addr: &str,
    peer_connection_info: &PubkeyConnectionInfo,
    logger: Arc<MutinyLogger>,
    peer_manager: Arc<dyn PeerManager>,
    stop: Arc<AtomicBool>,
) -> Result<(), MutinyError> {
    let (mut descriptor, socket_addr_opt) = match peer_connection_info.connection_type {
        ConnectionType::Tcp(ref t) => {
            #[cfg(target_arch = "wasm32")]
            {
                let proxy = WsProxy::new(
                    websocket_proxy_addr,
                    peer_connection_info.clone(),
                    logger.clone(),
                )
                .await?;
                let (_, net_addr) = try_parse_addr_string(t);
                (
                    MutinySocketDescriptor::Tcp(WsTcpSocketDescriptor::new(Arc::new(proxy))),
                    net_addr,
                )
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let (socket_addr, net_addr) = try_parse_addr_string(t);
                let socket_addr = socket_addr.ok_or(MutinyError::ConnectionFailed)?;

                let stream =
                    time::timeout(Duration::from_secs(10), TcpStream::connect(&socket_addr))
                        .await
                        .map_err(|_| MutinyError::ConnectionFailed)?
                        .map_err(|_| MutinyError::ConnectionFailed)?;

                let stream = stream.into_std().unwrap();
                (
                    MutinySocketDescriptor::Native(TcpSocketDescriptor::new(Arc::new(
                        tokio::sync::Mutex::new(stream),
                    ))),
                    net_addr,
                )
            }
        }
    };

    // then give that connection to the peer manager
    let initial_bytes = peer_manager.new_outbound_connection(
        peer_connection_info.pubkey,
        descriptor.clone(),
        socket_addr_opt,
    )?;

    log_debug!(logger, "connected to peer: {:?}", peer_connection_info);

    let sent_bytes = descriptor.send_data(&initial_bytes, true);
    log_trace!(
        logger,
        "sent {sent_bytes} to node: {}",
        peer_connection_info.pubkey
    );

    // schedule a reader on the connection
    schedule_descriptor_read(
        descriptor,
        peer_manager.clone(),
        logger.clone(),
        stop.clone(),
    );

    Ok(())
}

fn try_parse_addr_string(addr: &str) -> (Option<SocketAddr>, Option<NetAddress>) {
    let socket_addr = addr.parse::<SocketAddr>().ok();
    let net_addr = socket_addr.map(|socket_addr| match socket_addr {
        SocketAddr::V4(sockaddr) => NetAddress::IPv4 {
            addr: sockaddr.ip().octets(),
            port: sockaddr.port(),
        },
        SocketAddr::V6(sockaddr) => NetAddress::IPv6 {
            addr: sockaddr.ip().octets(),
            port: sockaddr.port(),
        },
    });
    (socket_addr, net_addr)
}
