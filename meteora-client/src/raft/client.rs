use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use bincode::serialize;
use grpcio::{ChannelBuilder, EnvBuilder};
use log::*;
use raft::eraftpb::{ConfChange, ConfChangeType};

use meteora_proto::proto::common::{NodeAddress, Null, State};
use meteora_proto::proto::raft_grpc::RaftServiceClient;

pub fn create_raft_client(address: String) -> RaftServiceClient {
    let env = Arc::new(EnvBuilder::new().build());
    let ch = ChannelBuilder::new(env).connect(&address);
    let client = RaftServiceClient::new(ch);
    client
}

pub struct RaftClient {
    leader_id: u64, // leader's index in server_ids
    clients: HashMap<u64, Arc<RaftServiceClient>>,
    addresses: HashMap<u64, String>,
}

impl RaftClient {
    pub fn new(address: &str) -> RaftClient {
        let raft_client = create_raft_client(address.to_string());

        let req = Null::new();
        let reply = raft_client.status(&req).unwrap();
        let leader_id = reply.leader_id;
        let addresses: HashMap<u64, String> = reply
            .address_map
            .iter()
            .map(|(node_id, node_address)| (node_id.clone(), node_address.raft_address.clone()))
            .collect();
        let node_id = reply
            .address_map
            .iter()
            .find_map(|(node_id, node_address)| {
                if &node_address.raft_address == address {
                    Some(node_id.clone())
                } else {
                    None
                }
            })
            .unwrap();

        let mut clients = HashMap::new();
        clients.insert(node_id, Arc::new(raft_client));
        for (i, a) in &addresses {
            if node_id != *i {
                let c = create_raft_client(a.to_string());
                clients.insert(*i, Arc::new(c));
            }
        }

        RaftClient {
            leader_id,
            clients,
            addresses,
        }
    }

    pub fn join(
        &mut self,
        node_id: u64,
        node_address: NodeAddress,
    ) -> Result<HashMap<u64, NodeAddress>, std::io::Error> {
        let mut req = ConfChange::new();
        req.set_node_id(node_id);
        req.set_change_type(ConfChangeType::AddNode);
        req.set_context(serialize(&node_address).unwrap());

        let max_retry = 10;
        let mut cnt_retry = 0;

        loop {
            if max_retry < cnt_retry {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("max retry count has been exceeded: max_retry={}", max_retry),
                ));
            }

            let client = match self.clients.get(&self.leader_id) {
                Some(c) => c,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("failed to get client for node: id={}", self.leader_id),
                    ));
                }
            };

            let reply = match client.change_config(&req) {
                Ok(r) => r,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "failed to join node to the cluster: id={}",
                            req.get_node_id()
                        ),
                    ));
                }
            };

            // update address list and clients
            // add new ids
            for (id, address) in reply.get_address_map() {
                if let Some(grpc_address) = self.addresses.get(&id) {
                    if grpc_address == address.raft_address.as_str() {
                        debug!(
                            "node has not been changed: id={}, address={}",
                            id, grpc_address
                        );
                    } else {
                        debug!("update node: id={}, address={}", id, address.raft_address);
                        self.addresses
                            .insert(id.clone(), address.raft_address.clone());
                        self.clients.insert(
                            id.clone(),
                            Arc::new(create_raft_client(address.raft_address.clone())),
                        );
                    }
                } else {
                    debug!("add node: id={}, address={}", id, address.raft_address);
                    self.addresses
                        .insert(id.clone(), address.raft_address.clone());
                    self.clients.insert(
                        id.clone(),
                        Arc::new(create_raft_client(address.raft_address.clone())),
                    );
                }
            }

            // remove unused ids
            for (id, address) in &self.addresses.clone() {
                if reply.get_address_map().contains_key(&id) {
                    debug!("node is in use: id={}, address={}", id, address);
                } else {
                    debug!("node is not in use: id={}, address={}", id, address);
                    self.addresses.remove(id);
                    self.clients.remove(id);
                }
            }

            debug!("addresses={:?}", self.addresses);

            match reply.get_state() {
                State::OK => {
                    return Ok(reply.get_address_map().clone());
                }
                State::WRONG_LEADER => {
                    warn!(
                        "upddate leader id: current={}, new={}",
                        self.leader_id,
                        reply.get_leader_id()
                    );
                    self.leader_id = reply.get_leader_id();
                    cnt_retry += 1;
                    warn!("retry with a new leader: id={}", self.leader_id);
                    continue;
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "failed to join node to the cluster: id={}",
                            req.get_node_id()
                        ),
                    ));
                }
            };
        }
    }

    pub fn leave(&mut self, id: u64) -> Result<HashMap<u64, NodeAddress>, std::io::Error> {
        let mut req = ConfChange::new();
        req.set_node_id(id);
        req.set_change_type(ConfChangeType::RemoveNode);
        req.set_context(vec![]);

        let max_retry = 10;
        let mut cnt_retry = 0;

        loop {
            if max_retry < cnt_retry {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("max retry count has been exceeded: max_retry={}", max_retry),
                ));
            }

            let client = match self.clients.get(&self.leader_id) {
                Some(c) => c,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("failed to get client for node: id={}", self.leader_id),
                    ));
                }
            };

            let reply = match client.change_config(&req) {
                Ok(r) => r,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "failed to leave node from the cluster: id={}",
                            req.get_node_id()
                        ),
                    ));
                }
            };

            // update address list and clients
            // add new ids
            for (id, address) in reply.get_address_map() {
                if let Some(grpc_address) = self.addresses.get(&id) {
                    if grpc_address == address.raft_address.as_str() {
                        debug!(
                            "node has not been changed: id={}, address={}",
                            id, grpc_address
                        );
                    } else {
                        debug!("update node: id={}, address={}", id, address.raft_address);
                        self.addresses
                            .insert(id.clone(), address.raft_address.clone());
                        self.clients.insert(
                            id.clone(),
                            Arc::new(create_raft_client(address.raft_address.clone())),
                        );
                    }
                } else {
                    debug!("add node: id={}, address={}", id, address.raft_address);
                    self.addresses
                        .insert(id.clone(), address.raft_address.clone());
                    self.clients.insert(
                        id.clone(),
                        Arc::new(create_raft_client(address.raft_address.clone())),
                    );
                }
            }

            // remove unused ids
            for (id, address) in &self.addresses.clone() {
                if reply.get_address_map().contains_key(&id) {
                    debug!("node is in use: id={}, address={}", id, address);
                } else {
                    debug!("node is not in use: id={}, address={}", id, address);
                    self.addresses.remove(id);
                    self.clients.remove(id);
                }
            }

            debug!("addresses={:?}", self.addresses);

            match reply.get_state() {
                State::OK => {
                    return Ok(reply.get_address_map().clone());
                }
                State::WRONG_LEADER => {
                    warn!(
                        "upddate leader id: current={}, new={}",
                        self.leader_id,
                        reply.get_leader_id()
                    );
                    self.leader_id = reply.get_leader_id();
                    cnt_retry += 1;
                    warn!("retry with a new leader: id={}", self.leader_id);
                    continue;
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "failed to leave node from the cluster: id={}",
                            req.get_node_id()
                        ),
                    ));
                }
            };
        }
    }

    pub fn status(&mut self) -> Result<HashMap<u64, NodeAddress>, std::io::Error> {
        let req = Null::new();

        let max_retry = 10;
        let mut cnt_retry = 0;

        loop {
            if max_retry < cnt_retry {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("max retry count has been exceeded: max_retry={}", max_retry),
                ));
            }

            let client = match self.clients.get(&self.leader_id) {
                Some(c) => c,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("failed to get client for node: id={}", self.leader_id),
                    ));
                }
            };

            let reply = match client.status(&req) {
                Ok(r) => r,
                _ => {
                    return Err(Error::new(ErrorKind::Other, "failed to get status"));
                }
            };

            // update address list and clients
            // add new ids
            for (id, address) in reply.get_address_map() {
                if let Some(grpc_address) = self.addresses.get(&id) {
                    if grpc_address == address.raft_address.as_str() {
                        debug!(
                            "node has not been changed: id={}, address={}",
                            id, grpc_address
                        );
                    } else {
                        debug!("update node: id={}, address={}", id, address.raft_address);
                        self.addresses
                            .insert(id.clone(), address.raft_address.clone());
                        self.clients.insert(
                            id.clone(),
                            Arc::new(create_raft_client(address.raft_address.clone())),
                        );
                    }
                } else {
                    debug!("add node: id={}, address={}", id, address.raft_address);
                    self.addresses
                        .insert(id.clone(), address.raft_address.clone());
                    self.clients.insert(
                        id.clone(),
                        Arc::new(create_raft_client(address.raft_address.clone())),
                    );
                }
            }

            // remove unused ids
            for (id, address) in &self.addresses.clone() {
                if reply.get_address_map().contains_key(&id) {
                    debug!("node is in use: id={}, address={}", id, address);
                } else {
                    debug!("node is not in use: id={}, address={}", id, address);
                    self.addresses.remove(id);
                    self.clients.remove(id);
                }
            }

            debug!("addresses={:?}", self.addresses);

            match reply.get_state() {
                State::OK => {
                    return Ok(reply.get_address_map().clone());
                }
                State::WRONG_LEADER => {
                    warn!(
                        "upddate leader id: current={}, new={}",
                        self.leader_id,
                        reply.get_leader_id()
                    );
                    self.leader_id = reply.get_leader_id();
                    cnt_retry += 1;
                    warn!("retry with a new leader: id={}", self.leader_id);
                    continue;
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "failed to leave node from the cluster: id={}",
                    ));
                }
            };
        }
    }
}
