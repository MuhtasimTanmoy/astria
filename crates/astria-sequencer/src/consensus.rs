use anyhow::anyhow;
use penumbra_storage::Storage;
use tendermint::abci::{
    request,
    response,
    ConsensusRequest,
    ConsensusResponse,
};
use tokio::sync::mpsc;
use tower_abci::BoxError;
use tower_actor::Message;

use crate::app::{
    App,
    GenesisState,
};

pub struct ConsensusService {
    queue: mpsc::Receiver<Message<ConsensusRequest, ConsensusResponse, tower::BoxError>>,
    storage: Storage,
    app: App,
}

impl ConsensusService {
    pub fn new(
        storage: Storage,
        app: App,
        queue: mpsc::Receiver<Message<ConsensusRequest, ConsensusResponse, tower::BoxError>>,
    ) -> Self {
        Self {
            queue,
            storage,
            app,
        }
    }

    pub async fn run(mut self) -> Result<(), tower::BoxError> {
        while let Some(Message {
            req,
            rsp_sender,
            span: _,
        }) = self.queue.recv().await
        {
            // The send only fails if the receiver was dropped, which happens
            // if the caller didn't propagate the message back to tendermint
            // for some reason -- but that's not our problem.
            let _ = rsp_sender.send(Ok(match req {
                ConsensusRequest::InitChain(init_chain) => ConsensusResponse::InitChain(
                    self.init_chain(init_chain)
                        .await
                        .expect("init_chain must succeed"),
                ),
                ConsensusRequest::BeginBlock(begin_block) => ConsensusResponse::BeginBlock(
                    self.begin_block(begin_block)
                        .await
                        .expect("begin_block must succeed"),
                ),
                ConsensusRequest::DeliverTx(deliver_tx) => ConsensusResponse::DeliverTx(
                    self.deliver_tx(deliver_tx)
                        .await?
                ),
                ConsensusRequest::EndBlock(end_block) => ConsensusResponse::EndBlock(
                    self.end_block(end_block)
                        .await
                        .expect("end_block must succeed"),
                ),
                ConsensusRequest::Commit => {
                    ConsensusResponse::Commit(self.commit().await.expect("commit must succeed"))
                }
            }));
        }
        Ok(())
    }

    async fn init_chain(
        &mut self,
        init_chain: request::InitChain,
    ) -> Result<response::InitChain, BoxError> {
        // the storage version is set to u64::MAX by default when first created
        if self.storage.latest_version() != u64::MAX {
            return Err(anyhow!("database already initialized").into());
        }

        let genesis_state: GenesisState = serde_json::from_slice(&init_chain.app_state_bytes)
            .expect("can parse app_state in genesis file");

        self.app.init_chain(&genesis_state).await?;

        // TODO: return the genesis app hash
        Ok(Default::default())
    }

    async fn begin_block(
        &mut self,
        begin_block: request::BeginBlock,
    ) -> Result<response::BeginBlock, BoxError> {
        let events = self.app.begin_block(&begin_block).await;
        Ok(response::BeginBlock {
            events,
        })
    }

    async fn deliver_tx(
        &mut self,
        deliver_tx: request::DeliverTx,
        // tx: bytes::Bytes,
    ) -> Result<response::DeliverTx, BoxError> {
        self.app.deliver_tx(&deliver_tx.tx).await?;
        Ok(Default::default())
    }

    async fn end_block(
        &mut self,
        end_block: request::EndBlock,
    ) -> Result<response::EndBlock, BoxError> {
        let events = self.app.end_block(&end_block).await;
        Ok(response::EndBlock {
            events,
            ..Default::default()
        })
    }

    async fn commit(&mut self) -> Result<response::Commit, BoxError> {
        let app_hash = self.app.commit(self.storage.clone()).await;
        Ok(response::Commit {
            data: app_hash.0.to_vec().into(),
            ..Default::default()
        })
    }
}
