// Copyright 2020 The Chubao Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied. See the License for the specific language governing
// permissions and limitations under the License.
use crate::client::meta_client::MetaClient;
use crate::pserver::raft::{
    raft::{JimRaftServer, RaftEngine},
    state_machine::{MemberChange, WriteRaftCallback},
};
use crate::pserver::simba::simba::Simba;
use crate::pserverpb::*;
use crate::util::{coding, config, entity::*, error::*};
use log::{error, info};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering::SeqCst},
    mpsc,
    mpsc::{Receiver, Sender},
    Arc, Mutex, RwLock,
};
use std::thread;

enum Store {
    Leader(Arc<RaftEngine>, Arc<Simba>),
    Member(Arc<RaftEngine>),
    //TODO: READ Only
}

impl Store {
    fn is_leader_type(&self) -> bool {
        match self {
            Self::Leader(_, _) => true,
            _ => false,
        }
    }

    fn simba(&self) -> ASResult<Arc<Simba>> {
        match self {
            Self::Leader(_, simba) => Ok(simba.clone()),
            _ => Err(err_code_str_box(
                PARTITION_NOT_LEADER,
                "simba partition not leader",
            )),
        }
    }

    fn raft(&self) -> ASResult<Arc<RaftEngine>> {
        match self {
            Self::Leader(raft, _) => Ok(raft.clone()),
            _ => Err(err_code_str_box(
                PARTITION_NOT_LEADER,
                "raft partition not leader",
            )),
            Self::Member(raft) => Ok(raft.clone()),
        }

        //Err(err_box(format!("can not take to memeber , it may be readoly")))
    }
}

pub struct PartitionService {
    pub server_id: AtomicU64,
    simba_map: RwLock<HashMap<(u32, u32), Arc<Store>>>,
    pub conf: Arc<config::Config>,
    pub lock: Mutex<usize>,
    meta_client: Arc<MetaClient>,
    sender: Mutex<Sender<MemberChange>>,
}

impl PartitionService {
    pub fn new(conf: Arc<config::Config>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<MemberChange>();
        let ps = Arc::new(PartitionService {
            server_id: AtomicU64::new(0),
            simba_map: RwLock::new(HashMap::new()),
            conf: conf.clone(),
            lock: Mutex::new(0),
            meta_client: Arc::new(MetaClient::new(conf)),
            sender: Mutex::new(tx),
        });

        let arc_ps = ps.clone();
        thread::spawn(move || {
            for mc in rx {
                info!("recevie change member:{:?}", mc);
                arc_ps.member_change(mc);
            }
        });

        ps
    }

    pub async fn init(&self) -> ASResult<()> {
        let ps = match self
            .meta_client
            .register(
                self.conf.ps.zone_id as u32,
                None,
                self.conf.global.ip.as_str(),
                self.conf.ps.rpc_port as u32,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let e = cast_to_err(e);
                PServer::new(
                    self.conf.ps.zone_id,
                    None,
                    format!("{}:{}", self.conf.global.ip.as_str(), self.conf.ps.rpc_port),
                )
            }
        };

        match ps.id {
            Some(id) => self.server_id.store(id as u64, SeqCst),
            None => {
                return Err(err_box(format!(
                    "got id for master has err got:{:?} ",
                    ps.id
                )));
            }
        }

        info!("get_server line:{:?}", ps);

        for wp in ps.write_partitions {
            if let Err(e) = self
                .init_partition(wp.collection_id, wp.id, wp.replicas, false, wp.version)
                .await
            {
                error!("init partition has err:{}", e.to_string());
            };
        }

        self.take_heartbeat().await?;

        Ok(())
    }

    pub async fn init_partition(
        &self,
        collection_id: u32,
        partition_id: u32,
        replicas: Vec<Replica>,
        readonly: bool,
        version: u64,
    ) -> ASResult<()> {
        info!(
            "to load partition:{} partition:{} exisit:{}",
            collection_id,
            partition_id,
            self.simba_map
                .read()
                .unwrap()
                .contains_key(&(collection_id, partition_id))
        );

        let _ = self.lock.lock().unwrap();
        info!("Start init_partition");

        if self
            .simba_map
            .read()
            .unwrap()
            .get(&(collection_id, partition_id))
            .is_some()
        {
            return Ok(());
        }

        let collection = Arc::new(self.meta_client.get_collection_by_id(collection_id).await?);

        if version > 0 {
            self.check_partition_version(collection_id, partition_id, version)
                .await?;
        }

        let partition = Arc::new(Partition {
            id: partition_id,
            collection_id: collection_id,
            replicas: replicas,
            leader: format!("{}:{}", self.conf.global.ip, self.conf.ps.rpc_port), //TODO: first need set leader.
            version: version + 1,
        });

        //first group raft
        let raft_server =
            JimRaftServer::get_instance(self.conf.clone(), self.server_id.load(SeqCst));

        let raft = raft_server.create_raft(partition.clone())?;

        self.simba_map.write().unwrap().insert(
            (collection_id, partition_id),
            Arc::new(Store::Member(Arc::new(RaftEngine::new(
                collection, partition, raft,
            )))),
        );

        Ok(())
    }

    async fn check_partition_version(&self, cid: u32, pid: u32, version: u64) -> ASResult<()> {
        let partition = self.meta_client.get_partition(cid, pid).await?;

        if partition.version > version {
            return Err(err_code_box(
                VERSION_ERR,
                format!(
                    "the collection:{} partition:{} version not right expected:{} found:{}",
                    cid, pid, version, partition.version
                ),
            ));
        }
        Ok(())
    }

    //type MemberChange = (u64, u64, u64);
    pub fn member_change(&self, mc: MemberChange) -> ASResult<()> {
        let (cid, pid, leader_id) = mc;
        let store = match self.simba_map.read().unwrap().get(&(cid, pid)) {
            Some(store) => store.clone(),
            None => {
                return Err(err_box(format!(
                    "not found partition_id:{} collection_id:{} in server",
                    cid, pid
                )));
            }
        };

        if self.server_id.load(SeqCst) == leader_id {
            if store.is_leader_type() {
                return Ok(());
            }

            let raft = store.raft()?;

            let simba = Simba::new(
                self.conf.clone(),
                Some(raft.clone()),
                raft.collection.clone(),
                raft.partition.clone(),
            )?;

            let reader = raft.raft.begin_read_log(simba.get_raft_index())?;

            loop {
                match reader.next_log() {
                    Ok((_, raft_index, line, flag)) => {
                        if !flag {
                            break;
                        }
                        if let Err(e) = simba.do_write(raft_index, &line) {
                            error!("init raft log has err:{:?} line:{:?}", e, line);
                        }
                    }
                    Err(e) => {
                        error!(
                            "collection:{} partition:{} got log from raft has err:{:?}",
                            cid, pid, e
                        );
                    }
                }
            }

            let store = Store::Leader(store.raft()?, simba);
            self.simba_map
                .write()
                .unwrap()
                .insert((cid, pid), Arc::new(store));
        } else {
            if !store.is_leader_type() {
                return Ok(());
            }
            let store = Store::Member(store.raft()?);
            self.simba_map
                .write()
                .unwrap()
                .insert((cid, pid), Arc::new(store));
        }

        return Ok(());
    }

    //offload partition , if partition not exist , it will return success
    pub fn offload_partition(&self, req: PartitionRequest) -> ASResult<GeneralResponse> {
        info!(
            "to offload partition:{} partition:{} exisit:{}",
            req.collection_id,
            req.partition_id,
            self.simba_map
                .read()
                .unwrap()
                .contains_key(&(req.collection_id, req.partition_id))
        );
        if let Some(store) = self
            .simba_map
            .write()
            .unwrap()
            .remove(&(req.collection_id, req.partition_id))
        {
            store.simba()?.stop();
            crate::sleep!(300);
            while Arc::strong_count(&store) > 1 {
                info!(
                    "wait release collection:{} partition:{} now is :{}",
                    req.collection_id,
                    req.partition_id,
                    Arc::strong_count(&store)
                );
                crate::sleep!(300);
            }
            store.simba()?.release();
        }
        make_general_success()
    }

    pub async fn take_heartbeat(&self) -> ASResult<()> {
        let _ = self.lock.lock().unwrap();

        let wps = self
            .simba_map
            .read()
            .unwrap()
            .values()
            .filter(|s| !s.is_leader_type())
            .map(|s| Partition::clone(&*s.simba().unwrap().base.partition))
            .collect::<Vec<Partition>>();

        self.meta_client
            .put_pserver(&PServer {
                id: Some(self.server_id.load(SeqCst) as u32),
                addr: format!("{}:{}", self.conf.global.ip.as_str(), self.conf.ps.rpc_port),
                write_partitions: wps,
                zone_id: self.conf.ps.zone_id,
                modify_time: 0,
            })
            .await
    }

    pub async fn write(&self, req: WriteDocumentRequest) -> ASResult<GeneralResponse> {
        let simba = if let Some(store) = self
            .simba_map
            .read()
            .unwrap()
            .get(&(req.collection_id, req.partition_id))
        {
            store.simba()?.clone()
        } else {
            return Err(make_not_found_err(req.collection_id, req.partition_id)?);
        };

        let (tx, rx) = mpsc::channel::<GenericError>();
        let callback = WriteRaftCallback::new(tx, simba.clone());

        simba.write(req, callback)?;

        match rx.recv() {
            Ok(e) => {
                if e.0 == SUCCESS {
                    return make_general_success();
                } else {
                    return Err(err_code_box(e.0, e.1));
                }
            }
            Err(e) => return Err(err_box(e.to_string())),
        }
    }

    pub fn get(&self, req: GetDocumentRequest) -> ASResult<DocumentResponse> {
        let store = if let Some(store) = self
            .simba_map
            .read()
            .unwrap()
            .get(&(req.collection_id, req.partition_id))
        {
            store.clone()
        } else {
            make_not_found_err(req.collection_id, req.partition_id)?
        };

        Ok(DocumentResponse {
            code: SUCCESS as i32,
            message: String::from("success"),
            doc: store.simba()?.get(req.id.as_str(), req.sort_key.as_str())?,
        })
    }

    pub async fn count(&self, req: CountDocumentRequest) -> ASResult<CountDocumentResponse> {
        let mut cdr = CountDocumentResponse {
            code: SUCCESS as i32,
            estimate_count: 0,
            index_count: 0,
            message: String::default(),
        };

        for collection_partition_id in req.cpids.iter() {
            let cpid = coding::split_u32(*collection_partition_id);
            let simba = if let Some(store) = self.simba_map.read().unwrap().get(&cpid) {
                store.simba()?.clone()
            } else {
                return make_not_found_err(cpid.0, cpid.1);
            };

            match simba.count() {
                Ok(v) => {
                    cdr.estimate_count += v.0;
                    cdr.index_count += v.1;
                }
                Err(e) => {
                    let e = cast_to_err(e);
                    cdr.code = e.0 as i32;
                    cdr.message.push_str(&format!(
                        "collection_partition_id:{} has err:{}  ",
                        collection_partition_id, e.1
                    ));
                }
            }
        }

        return Ok(cdr);
    }

    pub async fn search(&self, sdreq: SearchDocumentRequest) -> ASResult<SearchDocumentResponse> {
        assert_ne!(sdreq.cpids.len(), 0);
        let (tx, rx) = mpsc::channel();

        let sdreq = Arc::new(sdreq);

        for cpid in sdreq.cpids.iter() {
            let cpid = coding::split_u32(*cpid);
            if let Some(store) = self.simba_map.read().unwrap().get(&cpid) {
                if let Ok(simba) = store.simba() {
                    let simba = simba.clone();
                    let tx = tx.clone();
                    let sdreq = sdreq.clone();
                    thread::spawn(move || {
                        tx.send(simba.search(sdreq)).unwrap();
                    });
                } else {
                    return make_not_found_err(cpid.0, cpid.1);
                }
            } else {
                return make_not_found_err(cpid.0, cpid.1);
            }
        }

        empty(tx);

        let mut dist = rx.recv()?;
        for src in rx {
            dist = merge_search_document_response(dist, src);
        }
        dist.hits.sort_by(|v1, v2| {
            if v1.score >= v2.score {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });

        if dist.hits.len() > sdreq.size as usize {
            unsafe {
                dist.hits.set_len(sdreq.size as usize);
            }
        }

        Ok(dist)
    }

    pub fn status(&self, _request: GeneralRequest) -> ASResult<GeneralResponse> {
        Ok(GeneralResponse {
            code: SUCCESS as i32,
            message: String::from("ok"),
        })
    }
}

impl PartitionService {
    pub fn command(&self, command: CommandRequest) -> ASResult<Vec<u8>> {
        let value: Value = serde_json::from_slice(command.body.as_slice())?;

        match value["method"].as_str().unwrap() {
            "file_info" => self._file_info(value),
            _ => Err(err_box(format!("not found method:{}", value["method"]))),
        }
    }

    fn _file_info(&self, value: Value) -> ASResult<Vec<u8>> {
        let path = value["path"].as_str().unwrap().to_string();

        let mut result = Vec::new();

        for entry in std::fs::read_dir(path)? {
            let file = convert(entry)?;
            let meta = file.metadata()?;
            result.push(json!({
                "path": file.file_name().into_string(),
                "len":meta.len(),
                "modified": meta.modified().unwrap(),
            }));
        }

        convert(serde_json::to_vec(&result))
    }
}

fn empty(_: mpsc::Sender<SearchDocumentResponse>) {}

fn make_not_found_err<T>(cid: u32, pid: u32) -> ASResult<T> {
    Err(err_code_box(
        NOT_FOUND,
        format!("not found collection:{}  partition by id:{}", cid, pid),
    ))
}

fn make_general_success() -> ASResult<GeneralResponse> {
    Ok(GeneralResponse {
        code: SUCCESS as i32,
        message: String::from("success"),
    })
}
