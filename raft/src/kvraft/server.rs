use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::{Arc, RwLock};

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use futures::channel::oneshot;
use futures::executor::ThreadPool;
use futures::task::SpawnExt;
use futures::StreamExt;

use crate::proto::kvraftpb::*;
use crate::raft::{self, ApplyMsg};

const OP_PUT: i32 = 1;
const OP_APPEND: i32 = 2;
const OP_TYPE_GET: &str = "Get";
const OP_TYPE_PUT: &str = "Put";
const OP_TYPE_APPEND: &str = "Append";

#[allow(unused_macros)]
macro_rules! kvinfo {
    ($kv:expr, $($args:tt)+) => {
        info!("kv [me: {}] [term: {}] [is_leader: {:?}], {}",
              $kv.me,
              $kv.rf.term(),
              $kv.rf.is_leader(),
              format_args!($($args)+));
    };
}

#[allow(unused_macros)]
macro_rules! kvdebug {
    ($kv:expr, $($args:tt)+) => {
        debug!("kv [me: {}] [term: {}] [is_leader: {:?}], {}",
              $kv.me,
              $kv.rf.term(),
              $kv.rf.is_leader(),
              format_args!($($args)+));
    };
}

#[allow(unused_macros)]
macro_rules! kvpanic {
    ($kv:expr, $($args:tt)+) => {
        error!("kv [me: {}] [term: {}] [is_leader: {:?}], {}",
              $kv.me,
              $kv.rf.term(),
              $kv.rf.is_leader(),
              format_args!($($args)+));
        panic!();
    };
}

#[allow(unused_macros)]
macro_rules! kvwarn {
    ($kv:expr, $($args:tt)+) => {
        warn!("kv [me: {}] [term: {}] [is_leader: {:?}], {}",
              $kv.me,
              $kv.rf.term(),
              $kv.rf.is_leader(),
              format_args!($($args)+));
    };
}

impl TryFrom<GetRequest> for Op {
    type Error = ();
    fn try_from(value: GetRequest) -> Result<Self, Self::Error> {
        Ok(Op {
            key: value.key,
            value: String::from(""),
            op_type: OP_TYPE_GET.to_string(),
            name: value.name,
            reqno: value.reqno,
        })
    }
}

impl TryFrom<PutAppendRequest> for Op {
    type Error = ();
    fn try_from(value: PutAppendRequest) -> Result<Self, Self::Error> {
        let op_type = match value.op {
            OP_PUT => OP_TYPE_PUT,
            OP_APPEND => OP_TYPE_APPEND,
            _ => panic!("unknown putappend request"),
        };

        Ok(Op {
            key: value.key,
            value: value.value,
            op_type: op_type.to_string(),
            name: value.name,
            reqno: value.reqno,
        })
    }
}

impl From<OpReply> for GetReply {
    fn from(reply: OpReply) -> Self {
        Self {
            wrong_leader: reply.wrong_leader,
            err: reply.err,
            value: reply.value,
        }
    }
}

impl From<OpReply> for PutAppendReply {
    fn from(reply: OpReply) -> Self {
        Self {
            wrong_leader: reply.wrong_leader,
            err: reply.err,
        }
    }
}

/// the term is the available term for this sender
struct SenderWithTerm {
    term: u64,
    sender: oneshot::Sender<OpReply>,
}

pub struct KvServer {
    pub rf: raft::Node,
    me: usize,
    // snapshot if log grows this big
    maxraftstate: Option<usize>,
    // Your definitions here.
    apply_rx: Option<UnboundedReceiver<ApplyMsg>>,
    _last_applied_index: u64, // snapshot usage

    kv_store: HashMap<String, String>,
    max_reqno_map: HashMap<String, u64>, // <name -> max_reqno>

    // event notifying when command done by raft layer
    // logic: Node start polling from apply_rx, and apply() anything from it
    //        the KvServer apply() the command and generate corresponding results to a channel
    //        the channel was polled by RPC handler by node, and returns to client
    event_signal_map: HashMap<u64, SenderWithTerm>, // <index -> receiver>
}

impl KvServer {
    pub fn new(
        servers: Vec<crate::proto::raftpb::RaftClient>,
        me: usize,
        persister: Box<dyn raft::persister::Persister>,
        maxraftstate: Option<usize>,
    ) -> KvServer {
        // You may need initialization code here.

        let (apply_tx, apply_rx) = unbounded();
        let rf = raft::Raft::new(servers, me, persister, apply_tx);

        KvServer {
            rf: raft::Node::new(rf),
            me,
            maxraftstate,
            apply_rx: Some(apply_rx),
            max_reqno_map: HashMap::new(),
            kv_store: HashMap::new(),
            _last_applied_index: 0,
            event_signal_map: HashMap::new(),
        }
    }

    /// Apply this to state machine
    /// It does following things
    ///  - if it is a duplicate, just respond to Get
    ///  - else, apply this msg to the state machine and construct the OpReply
    ///  - fill the event_signal with the op_reply
    ///  - NOTE THAT: it is possible that I am not leader now, so it needs to be double checked
    fn apply(&mut self, msg: ApplyMsg) {
        let mut reply = OpReply {
            wrong_leader: false,
            err: String::from(""),
            value: String::from(""),
        };
        match msg {
            ApplyMsg::Command { data, index } => {
                // data is some type of op
                let op: Op = labcodec::decode(&data).unwrap();
                let is_dup = self.is_dup(&op);
                // double check leader
                if !self.rf.is_leader() {
                    reply.wrong_leader = true;
                }

                // operate the Op
                match op.op_type.as_str() {
                    OP_TYPE_PUT => {
                        if !is_dup {
                            kvinfo!(
                                self,
                                "[index: {}], putting [k: {}, v: {}]",
                                index,
                                op.key,
                                op.value
                            );
                            self.kv_store.insert(op.key.clone(), op.value);
                            kvinfo!(
                                self,
                                "After inserting, k: {}, value: {}",
                                op.key,
                                self.value_of(&op.key)
                            );
                        }
                    }

                    OP_TYPE_APPEND => {
                        if !is_dup {
                            kvinfo!(
                                self,
                                "[index: {}], appending [k: {}, v: {}]",
                                index,
                                op.key,
                                op.value
                            );
                            self.kv_store
                                .entry(op.key.clone())
                                .or_default()
                                .push_str(&op.value);
                            kvinfo!(
                                self,
                                "After append, k: {}, value: {}",
                                op.key,
                                self.value_of(&op.key)
                            );
                        }
                    }

                    OP_TYPE_GET => {
                        let value = self.kv_store.get(&op.key).cloned().unwrap_or_default();
                        kvinfo!(
                            self,
                            "[index: {}] getting [k = {}] [v = {}]",
                            index,
                            op.key,
                            value
                        );
                        reply.value = value;
                    }

                    _ => unreachable!(),
                }

                // put the reply into the event_signal_map
                if let Some(SenderWithTerm { term, sender }) = self.event_signal_map.remove(&index)
                {
                    // stale signal?
                    if term != self.rf.term() {
                        reply.wrong_leader = true;
                        reply.err = "STALE TERM".to_string();
                    }

                    sender.send(reply).unwrap();
                } // else i am not leader any more

                // todo: try_snapshot
            }

            ApplyMsg::Snapshot {
                data: _,
                term: _,
                index: _,
            } => {
                unimplemented!();
            }
        }
    }
}

// utils
impl KvServer {
    fn value_of(&self, key: &str) -> String {
        self.kv_store.get(key).cloned().unwrap_or_default()
    }
    /// return whether an Op is a duplicate or stale
    fn is_dup(&mut self, op: &Op) -> bool {
        let largest_reqno = self.max_reqno_map.entry(op.name.clone()).or_insert(0);
        // update if larger
        if op.reqno > *largest_reqno {
            *largest_reqno = op.reqno;
            false
        } else {
            true
        }
    }

    // replicate the Op and insert the sender
    fn replicate(&mut self, op: Op) -> labrpc::Result<oneshot::Receiver<OpReply>> {
        let (index, term) = self
            .rf
            .start(&op)
            .map_err(|_| labrpc::Error::Other("NOTLEADER".to_string()))?;
        let (get_tx, get_rx) = oneshot::channel();
        assert!(self
            .event_signal_map
            .insert(
                index,
                SenderWithTerm {
                    term,
                    sender: get_tx,
                }
            )
            .is_none());
        Ok(get_rx)
    }
}

impl KvServer {
    /// Only for suppressing deadcode warnings.
    #[doc(hidden)]
    pub fn __suppress_deadcode(&mut self) {
        let _ = &self.me;
        let _ = &self.maxraftstate;
    }
}

// Choose concurrency paradigm.
//
// You can either drive the kv server by the rpc framework,
//
// ```rust
// struct Node { server: Arc<Mutex<KvServer>> }
// ```
//
// or spawn a new thread runs the kv server and communicate via
// a channel.
//
// ```rust
// struct Node { sender: Sender<Msg> }
// ```
#[derive(Clone)]
pub struct Node {
    // Your definitions here.
    kv: Arc<RwLock<KvServer>>,
    tp: ThreadPool,
}

impl Node {
    pub fn new(mut kv: KvServer) -> Node {
        let apply_rx = kv.apply_rx.take().unwrap();

        let mut me = Node {
            kv: Arc::new(RwLock::new(kv)),
            tp: ThreadPool::new().unwrap(),
        };

        me.poll(apply_rx);

        me
    }

    pub fn poll(&mut self, mut apply_rx: UnboundedReceiver<ApplyMsg>) {
        let kv = Arc::clone(&self.kv);

        self.tp
            .spawn(async move {
                loop {
                    while let Some(msg) = apply_rx.next().await {
                        kv.write().unwrap().apply(msg);
                    }
                }
            })
            .unwrap();
    }

    /// the tester calls kill() when a KVServer instance won't
    /// be needed again. you are not required to do anything
    /// in kill(), but it might be convenient to (for example)
    /// turn off debug output from this instance.
    pub fn kill(&self) {
        // If you want to free some resources by `raft::Node::kill` method,
        // you should call `raft::Node::kill` here also to prevent resource leaking.
        // Since the test framework will call kvraft::Node::kill only.
        // self.server.kill();

        // Your code here, if desired.
        self.kv.read().unwrap().rf.kill();
    }

    /// The current term of this peer.
    pub fn term(&self) -> u64 {
        self.get_state().term()
    }

    /// Whether this peer believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.get_state().is_leader()
    }

    pub fn get_state(&self) -> raft::State {
        // Your code here.
        self.kv.read().unwrap().rf.get_state()
    }

    async fn generic_op_handler(kv: Arc<RwLock<KvServer>>, op: Op) -> OpReply {
        let mut reply = OpReply {
            wrong_leader: false,
            err: String::from(""),
            value: String::from(""),
        };

        kvinfo!(kv.read().unwrap(), "recv op: {:?}", op);

        let r = kv.write().unwrap().replicate(op);
        match r {
            Ok(rx) => {
                kvinfo!(kv.read().unwrap(), "waiting on rx");
                reply = rx.await.unwrap();
            }
            Err(e) => {
                reply.wrong_leader = true;
                reply.err = e.to_string()
            }
        }

        reply
    }
}

#[async_trait::async_trait]
impl KvService for Node {
    // CAVEATS: Please avoid locking or sleeping here, it may jam the network.
    async fn get(&self, arg: GetRequest) -> labrpc::Result<GetReply> {
        // Your code here.
        let op = Op::try_from(arg.clone()).unwrap();
        let kv = self.kv.clone();
        kvinfo!(
            kv.read().unwrap(),
            "Before [arg: {:?}], After [op: {:?}]",
            arg,
            op
        );
        Ok(Self::generic_op_handler(kv, op).await.into())
    }

    // CAVEATS: Please avoid locking or sleeping here, it may jam the network.
    async fn put_append(&self, arg: PutAppendRequest) -> labrpc::Result<PutAppendReply> {
        // Your code here.
        let op = Op::try_from(arg.clone()).unwrap();
        let kv = self.kv.clone();
        kvinfo!(
            kv.read().unwrap(),
            "Before [arg: {:?}], After [op: {:?}]",
            arg,
            op
        );
        Ok(Self::generic_op_handler(kv, op).await.into())
    }
}
