use std::{
    net::SocketAddr,
    time::Duration,
};

use astria_sequencer_relayer::{
    config::Config,
    telemetry,
    SequencerRelayer,
};
use astria_sequencer_validation::MerkleTree;
use celestia_client::celestia_types::{
    blob::SubmitOptions,
    Blob,
};
use ed25519_consensus::SigningKey;
use jsonrpsee::{
    core::SubscriptionResult,
    proc_macros::rpc,
    PendingSubscriptionSink,
};
use once_cell::sync::Lazy;
use proto::native::sequencer::v1alpha1::{
    SequenceAction,
    UnsignedTransaction,
};
use serde::Deserialize;
use tempfile::NamedTempFile;
use tendermint_config::PrivValidatorKey;
use tokio::{
    sync::{
        broadcast::{
            channel,
            Sender,
        },
        mpsc,
        oneshot,
    },
    task::JoinHandle,
};
use tracing::info;

static TELEMETRY: Lazy<()> = Lazy::new(|| {
    if std::env::var_os("TEST_LOG").is_some() {
        let filter_directives = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
        telemetry::init(std::io::stdout, &filter_directives).unwrap();
    } else {
        telemetry::init(std::io::sink, "").unwrap();
    }
});

/// Copied verbatim from
/// [tendermint-rs](https://github.com/informalsystems/tendermint-rs/blob/main/config/tests/support/config/priv_validator_key.json)
const PRIVATE_VALIDATOR_KEY: &str = r#"
{
  "address": "AD7DAE5FEC609CF02F9BDE7D81D0C3CD66141563",
  "pub_key": {
    "type": "tendermint/PubKeyEd25519",
    "value": "8mv0sqLoTOt6U8PxrndAh3myAGR4L7rb3w42WVnuRTQ="
  },
  "priv_key": {
    "type": "tendermint/PrivKeyEd25519",
    "value": "skHDGUYe2pOhwfSrXZQ6KeKnmKgTOn+f++Vmj4OOqIHya/SyouhM63pTw/Gud0CHebIAZHgvutvfDjZZWe5FNA=="
  }
}
"#;

pub(crate) struct TestSequencerRelayer {
    /// The socket address that sequencer relayer is serving its API endpoint on
    ///
    /// This is useful for checking if it's healthy, ready, or how many p2p peers
    /// are subscribed to it.
    api_address: SocketAddr,

    /// The mocked celestia node jsonrpc server
    pub(crate) celestia: MockCelestia,

    /// The mocked sequencer service (provides websockets for subscribing to new blocks)
    sequencer: MockSequencer,

    sequencer_relayer: JoinHandle<()>,

    config: Config,

    signing_key: SigningKey,
    account: tendermint::account::Id,

    _keyfile: NamedTempFile,

    block_time: tokio::time::Duration,
}

impl TestSequencerRelayer {
    pub(crate) async fn advance_by_block_time(&self) {
        tokio::time::advance(self.block_time + tokio::time::Duration::from_millis(100)).await;
    }

    pub(crate) async fn mount_block_response(&self, height: u32) {
        let block_response = create_block_response(&self.signing_key, self.account, height);
        self.sequencer.block_tx.send(block_response).unwrap();
    }
}

pub(crate) enum CelestiaMode {
    Immediate,
    Delayed(u64),
}

pub(crate) async fn spawn_sequencer_relayer(celestia_mode: CelestiaMode) -> TestSequencerRelayer {
    Lazy::force(&TELEMETRY);
    let block_time = 1000;

    let mut celestia = MockCelestia::start(block_time, celestia_mode).await;
    let celestia_addr = (&mut celestia.addr_rx).await.unwrap();

    let _keyfile = tokio::task::spawn_blocking(|| {
        use std::io::Write as _;

        let keyfile = NamedTempFile::new().unwrap();
        (&keyfile)
            .write_all(PRIVATE_VALIDATOR_KEY.as_bytes())
            .unwrap();
        keyfile
    })
    .await
    .unwrap();
    let PrivValidatorKey {
        address,
        priv_key,
        ..
    } = PrivValidatorKey::parse_json(PRIVATE_VALIDATOR_KEY).unwrap();
    let signing_key = priv_key
        .ed25519_signing_key()
        .cloned()
        .unwrap()
        .try_into()
        .unwrap();

    let sequencer = MockSequencer::spawn().await;

    let config = Config {
        sequencer_endpoint: sequencer.local_addr(),
        celestia_endpoint: format!("http://{celestia_addr}"),
        celestia_bearer_token: "".into(),
        gas_limit: 100000,
        validator_key_file: _keyfile.path().to_string_lossy().to_string(),
        rpc_port: 0,
        log: "".into(),
    };

    println!("sequencer endpoint: {}", config.sequencer_endpoint);

    info!(config = serde_json::to_string(&config).unwrap());
    let config_clone = config.clone();
    let sequencer_relayer = tokio::task::spawn_blocking(|| SequencerRelayer::new(config_clone))
        .await
        .unwrap()
        .await
        .unwrap();
    let api_address = sequencer_relayer.local_addr();
    let sequencer_relayer = tokio::task::spawn(sequencer_relayer.run());

    loop_until_sequencer_relayer_is_ready(api_address).await;

    tokio::time::sleep(Duration::from_millis(2000)).await;

    TestSequencerRelayer {
        api_address,
        celestia,
        config,
        sequencer,
        sequencer_relayer,
        block_time: Duration::from_millis(block_time),
        signing_key,
        account: address,
        _keyfile,
    }
}

async fn loop_until_sequencer_relayer_is_ready(addr: SocketAddr) {
    #[derive(Debug, serde::Deserialize)]
    struct Readyz {
        status: String,
    }
    loop {
        let readyz = reqwest::get(format!("http://{addr}/readyz"))
            .await
            .unwrap()
            .json::<Readyz>()
            .await
            .unwrap();
        if readyz.status.to_lowercase() == "ok" {
            break;
        }
    }
}

use celestia_mock::{
    BlobServer,
    HeaderServer,
};
use jsonrpsee::{
    core::async_trait,
    server::ServerHandle,
    types::ErrorObjectOwned,
};

pub struct MockCelestia {
    pub addr_rx: oneshot::Receiver<SocketAddr>,
    pub state_rpc_confirmed_rx: mpsc::UnboundedReceiver<Vec<Blob>>,
    pub _server_handle: ServerHandle,
}

impl MockCelestia {
    async fn start(sequencer_block_time_ms: u64, mode: CelestiaMode) -> Self {
        use jsonrpsee::server::ServerBuilder;
        let (addr_tx, addr_rx) = oneshot::channel();
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        addr_tx.send(addr).unwrap();
        let (state_rpc_confirmed_tx, state_rpc_confirmed_rx) = mpsc::unbounded_channel();
        let state_celestia = BlobServerImpl {
            sequencer_block_time_ms,
            mode,
            rpc_confirmed_tx: state_rpc_confirmed_tx,
        };
        let header_celestia = HeaderServerImpl;
        let mut merged_celestia = state_celestia.into_rpc();
        merged_celestia.merge(header_celestia.into_rpc()).unwrap();
        let _server_handle = server.start(merged_celestia);
        Self {
            addr_rx,
            state_rpc_confirmed_rx,
            _server_handle,
        }
    }
}

struct HeaderServerImpl;

#[async_trait]
impl HeaderServer for HeaderServerImpl {
    async fn header_network_head(
        &self,
    ) -> Result<celestia_client::celestia_types::ExtendedHeader, ErrorObjectOwned> {
        use celestia_client::{
            celestia_tendermint::{
                block::{
                    header::Header,
                    Commit,
                },
                validator,
            },
            celestia_types::{
                DataAvailabilityHeader,
                ExtendedHeader,
            },
        };
        let header = ExtendedHeader {
            header: Header {
                height: 42u32.into(),
                ..make_celestia_tendermint_header()
            },
            commit: Commit {
                height: 42u32.into(),
                ..Commit::default()
            },
            validator_set: validator::Set::without_proposer(vec![]),
            dah: DataAvailabilityHeader {
                row_roots: vec![],
                column_roots: vec![],
            },
        };
        Ok(header)
    }
}

struct BlobServerImpl {
    sequencer_block_time_ms: u64,
    mode: CelestiaMode,
    rpc_confirmed_tx: mpsc::UnboundedSender<Vec<Blob>>,
}

#[async_trait]
impl BlobServer for BlobServerImpl {
    async fn blob_submit(
        &self,
        blobs: Vec<Blob>,
        _opts: SubmitOptions,
    ) -> Result<u64, ErrorObjectOwned> {
        self.rpc_confirmed_tx.send(blobs).unwrap();
        if let CelestiaMode::Delayed(n) = self.mode {
            tokio::time::sleep(Duration::from_millis(n * self.sequencer_block_time_ms)).await;
        }
        Ok(100)
    }
}

fn create_block_response(
    signing_key: &SigningKey,
    account: tendermint::account::Id,
    height: u32,
) -> tendermint::Block {
    use proto::Message as _;
    use sha2::Digest as _;
    use tendermint::{
        block,
        chain,
        evidence,
        hash::AppHash,
        merkle::simple_hash_from_byte_vectors,
        Block,
        Hash,
        Time,
    };
    let suffix = height.to_string().into_bytes();
    let chain_id = [b"test_chain_id_", &*suffix].concat();
    let signed_tx_bytes = UnsignedTransaction {
        nonce: 1,
        actions: vec![
            SequenceAction {
                chain_id: chain_id.clone(),
                data: [b"hello_world_id_", &*suffix].concat(),
            }
            .into(),
        ],
    }
    .into_signed(signing_key)
    .into_raw()
    .encode_to_vec();
    let action_tree =
        astria_sequencer_validation::MerkleTree::from_leaves(vec![signed_tx_bytes.clone()]);
    let chain_ids_commitment = MerkleTree::from_leaves(vec![chain_id]).root();
    let data = vec![
        action_tree.root().to_vec(),
        chain_ids_commitment.to_vec(),
        signed_tx_bytes,
    ];
    let data_hash = Some(Hash::Sha256(simple_hash_from_byte_vectors::<sha2::Sha256>(
        &data.iter().map(sha2::Sha256::digest).collect::<Vec<_>>(),
    )));

    let (last_commit_hash, last_commit) = sequencer_types::test_utils::make_test_commit_and_hash();

    Block::new(
        block::Header {
            version: block::header::Version {
                block: 0,
                app: 0,
            },
            chain_id: chain::Id::try_from("test").unwrap(),
            height: block::Height::from(height),
            time: Time::now(),
            last_block_id: None,
            last_commit_hash: (height > 1).then_some(last_commit_hash),
            data_hash,
            validators_hash: Hash::Sha256([0; 32]),
            next_validators_hash: Hash::Sha256([0; 32]),
            consensus_hash: Hash::Sha256([0; 32]),
            app_hash: AppHash::try_from([0; 32].to_vec()).unwrap(),
            last_results_hash: None,
            evidence_hash: None,
            proposer_address: account,
        },
        data,
        evidence::List::default(),
        // The first height must not, every height after must contain a last commit
        (height > 1).then_some(last_commit),
    )
    .unwrap()
}

// /// Mounts 4 changing mock responses with the last one repeating
// pub async fn mount_4_changing_block_responses(
//     sequencer_relayer: &TestSequencerRelayer,
// ) -> Vec<endpoint::block::Response> { async fn create_and_mount_block( delay: Duration, server:
//   &MockServer, validator: &Validator, height: u32,
//     ) -> endpoint::block::Response { let rsp = create_block_response(validator, height); let
//       wrapped = Wrapper::new_with_id(Id::Num(1), Some(rsp.clone()), None);
//       Mock::given(body_partial_json(json!({"method": "block"}))) .respond_with(
//       ResponseTemplate::new(200) .set_body_json(wrapped) .set_delay(delay), ) .up_to_n_times(1)
//       .mount(server) .await; rsp
//     }

//     let response_delay = Duration::from_millis(1000); // was block_time
//     let validator = &sequencer_relayer.validator;
//     let server = &sequencer_relayer.sequencer;

//     let mut rsps = Vec::new();
//     // The first one resolves immediately
//     rsps.push(create_and_mount_block(Duration::ZERO, server, validator, 1).await);

//     for i in 2..=3 {
//         rsps.push(create_and_mount_block(response_delay, server, validator, i).await);
//     }

//     // The last one will repeat
//     rsps.push(create_block_response(validator, 4));
//     let wrapped = Wrapper::new_with_id(Id::Num(1), Some(rsps[3].clone()), None);
//     Mock::given(body_partial_json(json!({"method": "block"})))
//         .respond_with(
//             ResponseTemplate::new(200)
//                 .set_body_json(wrapped)
//                 .set_delay(response_delay),
//         )
//         .mount(&sequencer_relayer.sequencer)
//         .await;
//     rsps
// }

#[derive(Deserialize)]
struct ProxyQuery {
    query: String,
}

#[derive(Deserialize)]
#[serde(try_from = "ProxyQuery")]
#[allow(unreachable_pub)]
pub struct Query {
    _query: tendermint_rpc::query::Query,
}

impl TryFrom<ProxyQuery> for Query {
    type Error = tendermint_rpc::error::Error;

    fn try_from(proxy: ProxyQuery) -> Result<Self, Self::Error> {
        let query = proxy.query.parse::<tendermint_rpc::query::Query>()?;
        Ok(Self {
            _query: query,
        })
    }
}

#[rpc(server)]
trait Sequencer {
    #[subscription(name = "subscribe", item = jsonrpsee_core::JsonValue)]
    async fn subscribe(&self, query: Query) -> SubscriptionResult;

    #[method(name = "abci_info")]
    async fn abci_info(&self) -> jsonrpsee_core::JsonValue;
}

struct SequencerImpl {
    block_tx: Sender<tendermint::Block>,
}

#[async_trait]
impl SequencerServer for SequencerImpl {
    async fn abci_info(&self) -> jsonrpsee_core::JsonValue {
        let resp = serde_json::json!({
                "response": {
                    "data": "SequencerRelayerTest",
                    "version": "1.0.0",
                    "app_version": "1",
                    "last_block_height": "5",
                    "last_block_app_hash": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                }
        });
        resp
    }

    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        _query: Query,
    ) -> SubscriptionResult {
        println!("subscribe()");
        use jsonrpsee::server::SubscriptionMessage;
        let sink = pending.accept().await.unwrap();
        let mut rx = self.block_tx.subscribe();
        loop {
            tokio::select!(
                biased;
                () = sink.closed() => break,
                Ok(block) = rx.recv() => {
                    // let mut map = BTreeMap::new();
                    // map.insert("tm.event".to_string(), vec!["NewBlock".to_string()]);

                    // let event = tendermint_rpc::event::Event {
                    //     query: "tm.event='NewBlock'".to_string(),
                    //     data: tendermint_rpc::event::EventData::NewBlock { block: Some(block), result_begin_block: None, result_end_block: None },
                    //     events: Some(map),
                    // };
                    // let event_wrapper: tendermint_rpc::event::DialectEvent<tendermint_rpc::dialect::v0_37::Event> = event.into();

                    // let response_wrapper = tendermint_rpc::response::Wrapper::new_with_id(
                    //     tendermint_rpc::Id::Num(1),
                    //     Some(event_wrapper),
                    //     None,
                    // );
                    let resp = serde_json::json!({
                        "query": "tm.event='NewBlock'",
                        "data": {
                            "type": "tendermint/event/NewBlock",
                            "value": block,
                        },
                        "events": {
                            "tm.event": ["NewBlock"],
                        }
                    });
                    sink.send(
                        SubscriptionMessage::from_json(&resp).unwrap()
                    ).await?
                }
            );
        }
        Ok(())
    }
}

pub(crate) struct MockSequencer {
    /// The local address to which the mocked jsonrpc server is bound.
    local_addr: String,
    block_tx: Sender<tendermint::Block>,
    _server_task_handle: tokio::task::JoinHandle<()>,
}

impl MockSequencer {
    /// Spawns a new mocked sequencer server.
    /// # Panics
    /// Panics if the server fails to start.
    pub(crate) async fn spawn() -> Self {
        use jsonrpsee::server::Server;
        let server = Server::builder()
            .ws_only()
            .build("127.0.0.1:0")
            .await
            .expect("should be able to start a jsonrpsee server bound to a 0 port");
        let local_addr = server
            .local_addr()
            .expect("server should have a local addr");
        let (block_tx, _) = channel(256);
        let sequencer_impl = SequencerImpl {
            block_tx: block_tx.clone(),
        };
        let handle = server.start(sequencer_impl.into_rpc());
        let server_task_handle = tokio::spawn(handle.stopped());
        println!("sequencer server started on {}", local_addr);
        Self {
            local_addr: format!("ws://{}", local_addr),
            block_tx,
            _server_task_handle: server_task_handle,
        }
    }

    #[must_use]
    pub(crate) fn local_addr(&self) -> String {
        self.local_addr.clone()
    }
}

#[allow(clippy::missing_panics_doc)]
#[must_use]
/// Returns a default tendermint block header for test purposes.
pub fn make_celestia_tendermint_header() -> celestia_client::celestia_tendermint::block::Header {
    use celestia_client::celestia_tendermint::{
        account,
        block::{
            header::Version,
            Header,
            Height,
        },
        chain,
        hash::AppHash,
        Hash,
        Time,
    };

    Header {
        version: Version {
            block: 0,
            app: 0,
        },
        chain_id: chain::Id::try_from("test").unwrap(),
        height: Height::from(1u32),
        time: Time::now(),
        last_block_id: None,
        last_commit_hash: Hash::None,
        data_hash: Hash::None,
        validators_hash: Hash::Sha256([0; 32]),
        next_validators_hash: Hash::Sha256([0; 32]),
        consensus_hash: Hash::Sha256([0; 32]),
        app_hash: AppHash::try_from([0; 32].to_vec()).unwrap(),
        last_results_hash: Hash::None,
        evidence_hash: Hash::None,
        proposer_address: account::Id::try_from([0u8; 20].to_vec()).unwrap(),
    }
}
