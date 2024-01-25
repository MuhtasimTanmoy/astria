use std::{
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use celestia_client::celestia_types::nmt::Namespace;
use color_eyre::eyre::{
    self,
    WrapErr as _,
};
use ethers::prelude::{
    Address,
    Provider,
    Ws,
};
use tokio::{
    select,
    signal::unix::{
        signal,
        SignalKind,
    },
    sync::{
        oneshot,
        watch,
    },
    task::{
        spawn_local,
        LocalSet,
    },
    time::timeout,
};
use tokio_util::task::JoinMap;
use tracing::{
    error,
    info,
    warn,
};

use crate::{
    celestia,
    client_provider::{
        self,
        ClientProvider,
    },
    executor::Executor,
    sequencer,
    Config,
};

pub struct Conductor {
    /// The object pool of sequencer clients that restarts the websocket connection
    /// on failure.
    sequencer_client_pool: deadpool::managed::Pool<ClientProvider>,

    /// Channels to the long-running tasks to shut them down gracefully
    shutdown_channels: HashMap<&'static str, oneshot::Sender<()>>,

    /// Listens for several unix signals and notifies its subscribers.
    signals: SignalReceiver,

    /// The different long-running tasks that make up the conductor;
    tasks: JoinMap<&'static str, eyre::Result<()>>,
}

impl Conductor {
    const CELESTIA: &'static str = "celestia";
    const EXECUTOR: &'static str = "executor";
    const SEQUENCER: &'static str = "sequencer";

    /// Create a new [`Conductor`] from a [`Config`].
    ///
    /// # Errors
    /// Returns an error in the following cases if one of its constituent
    /// actors could not be spawned (executor, sequencer reader, or data availability reader).
    /// This usually happens if the actors failed to connect to their respective endpoints.
    pub async fn new(cfg: Config) -> eyre::Result<Self> {
        let mut tasks = JoinMap::new();
        let mut shutdown_channels = HashMap::new();

        let signals = spawn_signal_handler();

        // Spawn the executor task.
        let executor = {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();

            let hook = make_optimism_hook(&cfg)
                .await
                .wrap_err("failed constructing optimism hook")?;

            let executor = Executor::builder()
                .rollup_address(&cfg.execution_rpc_url)
                .wrap_err("failed to assign rollup node address")?
                .shutdown(shutdown_rx)
                .set_optimism_hook(hook)
                .build();

            shutdown_channels.insert(Self::EXECUTOR, shutdown_tx);
            executor
        };

        let sequencer_client_pool = client_provider::start_pool(&cfg.sequencer_url)
            .wrap_err("failed to create sequencer client pool")?;

        if !cfg.execution_commit_level.is_firm_only() {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();

            // The `sync_start_block_height` represents the height of the next
            // sequencer block that can be executed on top of the rollup state.
            // This value is derived by the Executor.
            let sequencer_reader = sequencer::Reader::new(
                sequencer_client_pool.clone(),
                shutdown_rx,
                executor.sequencer_channel(),
                executor.subscribe_to_state(),
            );
            tasks.spawn(Self::SEQUENCER, sequencer_reader.run_until_stopped());
            shutdown_channels.insert(Self::SEQUENCER, shutdown_tx);
        }

        if !cfg.execution_commit_level.is_soft_only() {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            shutdown_channels.insert(Self::CELESTIA, shutdown_tx);

            // Sequencer namespace is defined by the chain id of attached sequencer node
            // which can be fetched from any block header.
            let sequencer_namespace = {
                let client = sequencer_client_pool
                    .get()
                    .await
                    .wrap_err("failed to get a sequencer client from the pool")?;
                get_sequencer_namespace(client)
                    .await
                    .wrap_err("failed to get sequencer namespace")?
            };
            // XXX: This log is very misleading. The field called `sequencer_chain_id` is actually
            // the rollup ID.
            //
            // info!(
            //     celestia_namespace = %Base64Display::new(sequencer_namespace.as_bytes(),
            // &STANDARD),     sequencer_chain_id = %cfg.chain_id,
            //     "celestia namespace derived from sequencer chain id",
            // );

            // TODO ghi(https://github.com/astriaorg/astria/issues/470): add sync functionality to data availability reader
            let reader = celestia::Reader::builder()
                .celestia_endpoint(&cfg.celestia_node_url)
                .celestia_token(&cfg.celestia_bearer_token)
                .executor_channel(executor.celestia_channel())
                .executor_state(executor.subscribe_to_state())
                .sequencer_client_pool(sequencer_client_pool.clone())
                .sequencer_namespace(sequencer_namespace)
                .shutdown(shutdown_rx)
                .build()
                .await
                .wrap_err("failed constructing data availability reader")?;

            tasks.spawn(Self::CELESTIA, reader.run_until_stopped());
        };

        tasks.spawn(Self::EXECUTOR, executor.run_until_stopped());

        Ok(Self {
            sequencer_client_pool,
            shutdown_channels,
            signals,
            tasks,
        })
    }

    pub async fn run_until_stopped(mut self) {
        loop {
            select! {
                // FIXME: The bias should only be on the signal channels. The two handlers should have the same bias.
                biased;

                _ = self.signals.stop_rx.changed() => {
                    info!("shutting down conductor");
                    break;
                }

                _ = self.signals.reload_rx.changed() => {
                    info!("reloading is currently not implemented");
                }

                Some((name, res)) = self.tasks.join_next() => {
                    match res {
                        Ok(Ok(())) => error!(task.name = name, "task exited unexpectedly, shutting down"),
                        Ok(Err(e)) => {
                            let error: &(dyn std::error::Error + 'static) = e.as_ref();
                            error!(task.name = name, error, "task exited with error; shutting down");
                        }
                        Err(e) => {
                            let error = &e as &(dyn std::error::Error + 'static);
                            error!(task.name = name, error, "task failed; shutting down");
                        }
                    }
                }
            }
        }

        info!("shutting down conductor");
        self.shutdown().await;
    }

    async fn shutdown(self) {
        info!("sending shutdown command to all tasks");
        for (_, channel) in self.shutdown_channels {
            let _ = channel.send(());
        }

        self.sequencer_client_pool.close();

        info!("waiting 5 seconds for all tasks to shut down");
        // put the tasks into an Rc to make them 'static so they can run on a local set
        let mut tasks = Rc::new(self.tasks);
        let local_set = LocalSet::new();
        local_set
            .run_until(async {
                let mut tasks = tasks.clone();
                let _ = timeout(
                    Duration::from_secs(5),
                    spawn_local(async move {
                        while let Some((name, res)) = Rc::get_mut(&mut tasks)
                            .expect(
                                "only one Rc to the conductor tasks should exist; this is a bug",
                            )
                            .join_next()
                            .await
                        {
                            match res {
                                Ok(Ok(())) => info!(task.name = name, "task exited normally"),
                                Ok(Err(e)) => {
                                    let error: &(dyn std::error::Error + 'static) = e.as_ref();
                                    error!(task.name = name, error, "task exited with error");
                                }
                                Err(e) => {
                                    let error = &e as &(dyn std::error::Error + 'static);
                                    error!(task.name = name, error, "task failed");
                                }
                            }
                        }
                    }),
                )
                .await;
            })
            .await;

        if !tasks.is_empty() {
            warn!(
                number = tasks.len(),
                "aborting tasks that haven't shutdown yet"
            );
            Rc::get_mut(&mut tasks)
                .expect("only one Rc to the conductor tasks should exist; this is a bug")
                .shutdown()
                .await;
        }
    }
}

async fn make_optimism_hook(
    cfg: &Config,
) -> eyre::Result<Option<crate::executor::optimism::Handler>> {
    if !cfg.enable_optimism {
        return Ok(None);
    }
    let provider = Arc::new(
        Provider::<Ws>::connect(cfg.ethereum_l1_url.clone())
            .await
            .wrap_err("failed to connect to provider")?,
    );
    let contract_address: Address = hex::decode(cfg.optimism_portal_contract_address.clone())
        .wrap_err("failed to decode contract address as hex")
        .and_then(|bytes| {
            TryInto::<[u8; 20]>::try_into(bytes)
                .map_err(|_| eyre::eyre!("contract address must be 20 bytes"))
        })
        .wrap_err("failed to parse contract address")?
        .into();

    Ok(Some(crate::executor::optimism::Handler::new(
        provider,
        contract_address,
        cfg.initial_ethereum_l1_block_height,
    )))
}

struct SignalReceiver {
    reload_rx: watch::Receiver<()>,
    stop_rx: watch::Receiver<()>,
}

fn spawn_signal_handler() -> SignalReceiver {
    let (stop_tx, stop_rx) = watch::channel(());
    let (reload_tx, reload_rx) = watch::channel(());
    tokio::spawn(async move {
        let mut sighup = signal(SignalKind::hangup()).expect(
            "setting a SIGHUP listener should always work on unix; is this running on unix?",
        );
        let mut sigint = signal(SignalKind::interrupt()).expect(
            "setting a SIGINT listener should always work on unix; is this running on unix?",
        );
        let mut sigterm = signal(SignalKind::terminate()).expect(
            "setting a SIGTERM listener should always work on unix; is this running on unix?",
        );
        loop {
            select! {
                _ = sighup.recv() => {
                    info!("received SIGHUP");
                    let _ = reload_tx.send(());
                }
                _ = sigint.recv() => {
                    info!("received SIGINT");
                    let _ = stop_tx.send(());
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM");
                    let _ = stop_tx.send(());
                }
            }
        }
    });

    SignalReceiver {
        reload_rx,
        stop_rx,
    }
}

/// Get the sequencer namespace from the latest sequencer block.
async fn get_sequencer_namespace(
    client: deadpool::managed::Object<ClientProvider>,
) -> eyre::Result<Namespace> {
    use sequencer_client::SequencerClientExt as _;

    let retry_config = tryhard::RetryFutureConfig::new(10)
        .exponential_backoff(Duration::from_millis(100))
        .max_delay(Duration::from_secs(20))
        .on_retry(
            |attempt: u32,
             next_delay: Option<Duration>,
             error: &sequencer_client::extension_trait::Error| {
                let wait_duration = next_delay
                    .map(humantime::format_duration)
                    .map(tracing::field::display);
                warn!(
                    attempt,
                    wait_duration,
                    error = error as &dyn std::error::Error,
                    "attempt to grab sequencer block failed; retrying after backoff",
                );
                futures::future::ready(())
            },
        );

    let block = tryhard::retry_fn(|| client.latest_sequencer_block())
        .with_config(retry_config)
        .await
        .wrap_err("failed to get block from sequencer after 10 attempts")?;

    Ok(celestia_client::celestia_namespace_v0_from_cometbft_header(
        block.header(),
    ))
}
