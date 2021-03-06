use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures::Future;
use grpcio::{RpcContext, UnarySink};
use log::*;
use raft::storage::MemStorage;
use rocksdb::DB;
use serde::{Deserialize, Serialize};

use meteora_proto::proto::common::{NodeAddress, State};
use meteora_proto::proto::kv::{DeleteReply, DeleteReq, GetReply, GetReq, PutReply, PutReq};
use meteora_proto::proto::kv_grpc::KvService;

use crate::raft::config;
use crate::raft::server::RaftServer;

#[derive(Clone)]
pub struct KVServer {
    db: Arc<DB>,
    sender: Sender<config::Msg>,
    seq: u64,
    node_id: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum Op {
    Put { key: Vec<u8>, val: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl KVServer {
    pub fn new(
        db_path: String,
        raft_storage: MemStorage,
        node_id: u64,
        node_address: NodeAddress,
        addresses: HashMap<u64, NodeAddress>,
    ) -> (KVServer, RaftServer) {
        let db = DB::open_default(&db_path).unwrap();

        let (rs, rr) = mpsc::channel();
        let (apply_s, apply_r) = mpsc::channel();
        thread::spawn(move || {
            config::init_and_run(raft_storage, rr, apply_s, node_id, node_address, addresses);
        });

        let kv_server = KVServer {
            db: Arc::new(db),
            sender: rs.clone(),
            seq: 0,
            node_id,
        };
        let raft_server = RaftServer::new(rs, node_id);

        let db = kv_server.db.clone();
        thread::spawn(move || {
            apply_daemon(apply_r, db);
        });

        return (kv_server, raft_server);
    }
}

impl KvService for KVServer {
    fn get(&mut self, ctx: RpcContext, req: GetReq, sink: UnarySink<GetReply>) {
        let (s1, r1) = mpsc::channel();
        let db = Arc::clone(&self.db);
        let sender = self.sender.clone();
        let node_id = self.node_id;

        self.seq += 1;

        sender
            .send(config::Msg::Read {
                cb: Box::new(
                    move |leader_id: i32, addresses: HashMap<u64, NodeAddress>| {
                        // Get
                        let mut reply = GetReply::new();
                        let (state, value) = match db.get(req.get_key()) {
                            Ok(Some(v)) => (State::OK, v),
                            Ok(None) => (State::NOT_FOUND, Vec::new()),
                            Err(e) => {
                                error!("failed to get value: {:?}", e);
                                (State::IO_ERROR, Vec::new())
                            }
                        };
                        reply.set_state(state);
                        if leader_id >= 0 {
                            // follower
                            reply.set_leader_id(leader_id as u64);
                        } else {
                            // leader
                            reply.set_leader_id(node_id);
                        }
                        reply.set_value(value);
                        reply.set_address_map(addresses);
                        s1.send(reply).expect("callback channel closed");
                    },
                ),
            })
            .unwrap();

        let reply = match r1.recv_timeout(Duration::from_secs(2)) {
            Ok(r) => r,
            Err(_e) => {
                let mut r = GetReply::new();
                r.set_state(State::IO_ERROR);
                r
            }
        };

        let f = sink
            .success(reply.clone())
            .map_err(move |err| error!("failed to reply: {:?}", err));
        ctx.spawn(f);
    }

    fn put(&mut self, ctx: RpcContext, req: PutReq, sink: UnarySink<PutReply>) {
        let (s1, r1) = mpsc::channel();
        let sender = self.sender.clone();
        let op = Op::Put {
            key: req.get_key().to_vec(),
            val: req.get_value().to_vec(),
        };
        let seq = self.seq;
        let node_id = self.node_id;

        self.seq += 1;

        sender
            .send(config::Msg::Propose {
                seq,
                op,
                cb: Box::new(
                    move |leader_id: i32, addresses: HashMap<u64, NodeAddress>| {
                        let mut reply = PutReply::new();
                        if leader_id >= 0 {
                            // follower
                            reply.set_state(State::WRONG_LEADER);
                            reply.set_leader_id(leader_id as u64);
                        } else {
                            // leader
                            reply.set_state(State::OK);
                            reply.set_leader_id(node_id);
                        }
                        reply.set_address_map(addresses);
                        s1.send(reply).expect("callback channel closed");
                    },
                ),
            })
            .unwrap();

        let reply = match r1.recv_timeout(Duration::from_secs(2)) {
            Ok(r) => r,
            Err(_e) => {
                let mut r = PutReply::new();
                r.set_state(State::IO_ERROR);
                r
            }
        };

        let f = sink
            .success(reply.clone())
            .map_err(move |err| error!("failed to reply: {:?}", err));
        ctx.spawn(f);
    }

    fn delete(&mut self, ctx: RpcContext, req: DeleteReq, sink: UnarySink<DeleteReply>) {
        let (s1, r1) = mpsc::channel();
        let sender = self.sender.clone();
        let op = Op::Delete {
            key: req.get_key().to_vec(),
        };
        let seq = self.seq;
        let node_id = self.node_id;

        self.seq += 1;

        sender
            .send(config::Msg::Propose {
                seq,
                op,
                cb: Box::new(
                    move |leader_id: i32, addresses: HashMap<u64, NodeAddress>| {
                        let mut reply = DeleteReply::new();
                        if leader_id >= 0 {
                            // follower
                            reply.set_state(State::WRONG_LEADER);
                            reply.set_leader_id(leader_id as u64);
                        } else {
                            // leader
                            reply.set_state(State::OK);
                            reply.set_leader_id(node_id);
                        }
                        reply.set_address_map(addresses);
                        s1.send(reply).expect("callback channel closed");
                    },
                ),
            })
            .unwrap();

        let reply = match r1.recv_timeout(Duration::from_secs(2)) {
            Ok(r) => r,
            Err(_e) => {
                let mut r = DeleteReply::new();
                r.set_state(State::IO_ERROR);
                r
            }
        };

        let f = sink
            .success(reply.clone())
            .map_err(move |err| error!("failed to reply: {:?}", err));
        ctx.spawn(f);
    }
}

fn apply_daemon(receiver: Receiver<Op>, db: Arc<DB>) {
    loop {
        let op = match receiver.recv() {
            Ok(o) => o,
            _ => {
                debug!("exit the apply daemon");
                return;
            }
        };
        match op {
            Op::Put { key, val } => {
                db.put(key.as_slice(), val.as_slice()).unwrap();
            }
            Op::Delete { key } => {
                db.delete(key.as_slice()).unwrap();
            }
        }
    }
}
