use std::fmt::Arguments;
use std::fs;
#[cfg(feature = "metrics")]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(feature = "json-rpc")]
use std::sync::OnceLock;

pub use bitcoin::Network;
use fern::colors::Color;
use fern::colors::ColoredLevelConfig;
use fern::FormatCallback;
#[cfg(feature = "zmq-server")]
use floresta_chain::pruned_utreexo::BlockchainInterface;
pub use floresta_chain::AssumeUtreexoValue;
use floresta_chain::AssumeValidArg;
use floresta_chain::BlockchainError;
use floresta_chain::ChainState;
#[cfg(feature = "flat-chainstore")]
use floresta_chain::FlatChainStore as ChainStore;
#[cfg(feature = "flat-chainstore")]
use floresta_chain::FlatChainStoreConfig;
#[cfg(all(feature = "kv-chainstore", not(doc)))]
use floresta_chain::KvChainStore as ChainStore;
#[cfg(feature = "compact-filters")]
use floresta_compact_filters::flat_filters_store::FlatFiltersStore;
#[cfg(feature = "compact-filters")]
use floresta_compact_filters::network_filters::NetworkFilters;
use floresta_electrum::electrum_protocol::client_accept_loop;
use floresta_electrum::electrum_protocol::ElectrumServer;
use floresta_watch_only::kv_database::KvDatabase;
use floresta_watch_only::AddressCache;
use floresta_watch_only::AddressCacheDatabase;
use floresta_wire::address_man::AddressMan;
use floresta_wire::mempool::Mempool;
use floresta_wire::node::UtreexoNode;
use floresta_wire::running_node::RunningNode;
use floresta_wire::UtreexoNodeConfig;
use log::debug;
use log::error;
use log::info;
use log::warn;
use log::Record;
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::IsCa;
use rcgen::KeyPair;
use rustreexo::accumulator::pollard::Pollard;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task;
#[cfg(feature = "metrics")]
use tokio::time::Duration;
#[cfg(feature = "metrics")]
use tokio::time::{self};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::config_file::ConfigFile;
use crate::error;
#[cfg(feature = "json-rpc")]
use crate::json_rpc;
use crate::wallet_input::InitialWalletSetup;
#[cfg(feature = "zmq-server")]
use crate::zmq::ZMQServer;

// flat-chainstore and kv-chainstore are mutually exclusive
#[cfg(all(feature = "flat-chainstore", feature = "kv-chainstore", not(doc)))]
compile_error!(
    "You cannot use both flat-chainstore and kv-chainstore at the same time. Please choose one."
);

// at least one of flat-chainstore or kv-chainstore must be enabled
#[cfg(not(any(feature = "flat-chainstore", feature = "kv-chainstore")))]
compile_error!("You must enable either the flat-chainstore or kv-chainstore feature.");

#[derive(Clone)]
/// General configuration for the floresta daemon.
///
/// Those configs should be passed in by anyone that wants to start a floresta instance. Some of
/// these are also exposed through the config file.
pub struct Config {
    /// Whether we should disable dns seeds
    pub disable_dns_seeds: bool,
    /// Where we should place our data
    ///
    /// This directory must be readable and writable by our process. We'll use this dir to store
    /// both chain and wallet data, so this should be kept in a non-volatile medium. We are not
    /// particularly aggressive in disk usage, so we don't need a fast disk to work.
    ///
    /// If not set, it defaults to $HOME/.floresta
    pub data_dir: Option<String>,
    /// We consider blocks prior to this one to have a valid signature
    ///
    /// This is an optimization mirrored from Core, where blocks before this one are considered to
    /// have valid signatures. The idea here is that if a block is buried under a lot of PoW, it's
    /// very unlikely that it is invalid. We still validate everything else and build the
    /// accumulator until this point (unless running on PoW-fraud proof or assumeutreexo mode) so
    /// there's still some work to do.
    pub assume_valid: Option<String>,
    /// A vector of xpubs to cache
    ///
    /// This is a list of SLIP-132-encoded extended public key that we should add to our Watch-only
    /// wallet. A descriptor may be only passed one time, if you call florestad with an already
    /// cached address, that will be a no-op. After a xpub is cached, we derive multiple addresses
    /// from it and try to find transactions involving it.
    pub wallet_xpub: Option<Vec<String>>,
    /// An output descriptor to cache
    ///
    /// This should be a list of output descriptors that we should add to our watch-only wallet.
    /// This works just like wallet_xpub, but with a descriptor.
    pub wallet_descriptor: Option<Vec<String>>,
    /// Where should we read from a config file
    ///
    /// This is a toml-encoded file with floresta's configs. For a sample of how this file looks
    /// like, see config.toml.sample inside floresta's codebase.
    ///
    /// If a setting is modified by the config file and this config struct, the following logic is
    /// used:
    ///     - For vectors, we use the combination of both vectors
    ///     - for mutually exclusive options, this struct has precedence over the config file
    pub config_file: Option<String>,
    /// A proxy that we should use to connect with others
    ///
    /// This should be a socks5 proxy, like Tor's socks. If provided, all our outgoing connections
    /// will be made through this one, except dns seed connections.
    pub proxy: Option<String>,
    /// The network we are running in, it may be one of: bitcoin, signet, regtest or testnet.
    pub network: Network,
    /// Whether we should build and store compact block filters
    ///
    /// Those filters are used for rescanning our wallet for historical transactions. If you don't
    /// have this on, the only way to find historical transactions is to download all blocks, which
    /// is very inefficient and resource/time consuming. But keep in mind that filters will take
    /// up disk space.
    pub cfilters: bool,
    /// If we are using block filters, we may not need to download the whole chain of filters, as
    /// our wallets may not have been created at the beginning of the chain. With this option, we
    /// can make a rough estimate of the block height we need to start downloading filters.
    ///
    /// If the value is negative, it's relative to the current tip. For example, if the current tip
    /// is at height 1000, and we set this value to -100, we will start downloading filters from
    /// height 900.
    pub filters_start_height: Option<i32>,
    #[cfg(feature = "zmq-server")]
    /// The address to listen to for our ZMQ server
    ///
    /// We have an (optional) ZMQ server, that pushes new blocks over a PUSH/PULL ZMQ queue, this
    /// is the address that we'll listen for incoming connections.
    pub zmq_address: Option<String>,
    pub connect: Option<String>,
    #[cfg(feature = "json-rpc")]
    /// The address our json-rpc should listen to
    pub json_rpc_address: Option<String>,
    /// Whether we should write logs to `stdout`.
    pub log_to_stdout: bool,
    /// Whether we should log to a fs file
    pub log_to_file: bool,
    /// Whether we should use assume utreexo
    pub assume_utreexo: bool,
    /// Whether we should post debug information to the console
    pub debug: bool,
    /// The user agent that we will advertise to our peers
    pub user_agent: String,
    /// The value to use for assumeutreexo
    pub assumeutreexo_value: Option<AssumeUtreexoValue>,
    /// Address the Electrum Server will listen to.
    pub electrum_address: Option<String>,
    /// Whether to enable the Electrum TLS server.
    pub enable_electrum_tls: bool,
    /// Address the Electrum TLS Server will listen to.
    pub electrum_address_tls: Option<String>,
    /// TLS private key path (defaults to `{data_dir}/tls/key.pem`).
    /// It must be PKCS#8-encoded. You can use `openssl` to generate it:
    ///
    /// ```shell
    /// openssl genpkey -algorithm RSA -out key.pem -pkeyopt rsa_keygen_bits:2048
    /// ```
    pub tls_key_path: Option<String>,
    /// TLS certificate path (defaults to `{data_dir}/tls/cert.pem`).
    /// It must be PKCS#8-encoded. You can use `openssl` to generate it from a PKCS#8-encoded private key:
    ///
    /// ```shell
    /// openssl req -x509 -new -key key.pem -out cert.pem -days 365 -subj "/CN=localhost"
    /// ```
    pub tls_cert_path: Option<String>,
    /// Whether to create self signed certificate for `tls_key_path` and `tls_cert_path`.
    pub generate_cert: bool,
    /// Whether to allow fallback to v1 transport if v2 connection fails.
    pub allow_v1_fallback: bool,
    /// Whehter we should backfill
    ///
    /// If we assumeutreexo or use pow fraud proofs, you have the option to download and validate
    /// the blocks that were skipped. This will take a long time, but will run on the background
    /// and won't affect the node's operation. You may notice that this will take a lot of CPU
    /// and bandwidth to run.
    pub backfill: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            disable_dns_seeds: false,
            data_dir: None,
            assume_valid: None,
            wallet_xpub: None,
            wallet_descriptor: None,
            config_file: None,
            proxy: None,
            network: Network::Bitcoin,
            cfilters: false,
            filters_start_height: None,
            #[cfg(feature = "zmq-server")]
            zmq_address: None,
            connect: None,
            #[cfg(feature = "json-rpc")]
            json_rpc_address: None,
            log_to_stdout: false,
            log_to_file: false,
            assume_utreexo: false,
            debug: false,
            user_agent: String::default(),
            assumeutreexo_value: None,
            electrum_address: None,
            enable_electrum_tls: false,
            electrum_address_tls: None,
            generate_cert: false,
            tls_key_path: None,
            tls_cert_path: None,
            allow_v1_fallback: false,
            backfill: false,
        }
    }
}

pub struct Florestad {
    /// The config used by this node, see [Config] for more details
    config: Config,
    /// A channel that tells others to stop what they are doing because we
    /// are about to die
    stop_signal: Arc<RwLock<bool>>,
    /// A channel that notifies we are done, and it's safe to die now
    stop_notify: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    #[cfg(feature = "json-rpc")]
    /// A handle to our json-rpc server
    json_rpc: OnceLock<tokio::task::JoinHandle<()>>,
}

impl Florestad {
    /// Kills a running florestad, this will return as soon as the main node stops.
    ///
    /// It's not safe to stop your program before this thread returns because some
    /// information may not be fully flushed to disk yet, and killing the process
    /// before flushing everything is equivalent to an unclean shutdown.
    pub async fn stop(&self) {
        info!("Stopping node...");
        let mut stop_signal = self.stop_signal.write().await;
        *stop_signal = true;
    }

    pub async fn should_stop(&self) -> bool {
        let stop_signal = self.stop_signal.read().await;
        *stop_signal
    }

    pub fn get_stop_signal(&self) -> Arc<RwLock<bool>> {
        self.stop_signal.clone()
    }

    pub async fn wait_shutdown(&self) {
        let chan = {
            let mut guard = self.stop_notify.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        if let Some(chan) = chan {
            if let Err(e) = chan.await {
                error!("POSSIBLE BUG: unexpected error while shutting down {e:?}");
            }
        }
    }

    /// Parses an address in the format `<hostname>[<:port>]` and returns a
    /// `SocketAddr` with the resolved IP address. If a hostname is provided,
    /// it will be resolved using the system's DNS resolver. This function will
    /// exit the program if it fails to resolve the hostname or the provided
    /// address is invalid.
    fn resolve_hostname(hostname: &str, default_port: u16) -> SocketAddr {
        if !hostname.contains(':') {
            let Ok(ip) = hostname.parse() else {
                error!("Invalid IP address: {hostname}");
                exit(1);
            };

            return SocketAddr::new(ip, default_port);
        }

        let ip = hostname.parse();
        match ip {
            Ok(ip) => ip,
            Err(_) => {
                let mut split = hostname.split(':');
                let hostname = split.next().unwrap();

                debug!("Resolving hostname: {hostname}");

                let ips: Vec<_> = match dns_lookup::lookup_host(hostname) {
                    Ok(ips) => ips,
                    Err(e) => {
                        error!("Could not resolve hostname: {e}");
                        exit(1);
                    }
                };

                if ips.is_empty() {
                    error!("No IP addresses found for hostname: {hostname}");
                    exit(1);
                }

                let port = split
                    .next()
                    .map(|x| x.parse().unwrap_or(default_port))
                    .unwrap_or(default_port);

                SocketAddr::new(ips[0], port)
            }
        }
    }

    /// Actually runs florestad, spawning all modules and waiting until
    /// someone asks to stop.
    pub async fn start(&self) {
        let data_dir = Self::data_dir_path(&self.config);

        // Create the data directory if it doesn't exist
        if !Path::new(&data_dir).exists() {
            fs::create_dir_all(&data_dir).expect("Could not create data directory");
        }

        // Setup global logger
        if self.config.log_to_stdout || self.config.log_to_file {
            Self::setup_logger(
                &data_dir,
                self.config.log_to_file,
                self.config.log_to_stdout,
                self.config.debug,
            )
            .expect("Failure to setup logger");
        }

        info!("Loading watch-only wallet");
        let mut wallet = Self::load_wallet(&data_dir);
        wallet.setup().expect("Could not initialize wallet");
        debug!("Done loading wallet");

        // Try to add more wallets to watch if needed
        if let Err(e) = self.setup_wallet(&data_dir, &mut wallet) {
            log::error!("Something went wrong while setting wallet up: {e}");
            return;
        }

        info!("Loading blockchain database");
        let blockchain_state = Arc::new(Self::load_chain_state(
            data_dir.clone(),
            self.config.network,
            self.config
                .assume_valid
                .as_ref()
                .map(|value| value.parse().expect("Invalid assumevalid")),
        ));

        #[cfg(feature = "compact-filters")]
        let cfilters = if self.config.cfilters {
            // Block Filters
            let filter_store = FlatFiltersStore::new((data_dir.clone() + "/cfilters").into());
            let cfilters = Arc::new(NetworkFilters::new(filter_store));
            info!(
                "Loaded compact filters store at height: {:?}",
                cfilters.get_height().unwrap()
            );
            Some(cfilters)
        } else {
            None
        };

        #[cfg(not(feature = "compact-filters"))]
        let cfilters = None;

        // For now, we only have compatible bridges on signet
        let pow_fraud_proofs = match self.config.network {
            Network::Bitcoin => false,
            Network::Signet => true,
            Network::Testnet => false,
            Network::Regtest => false,
            _ => false,
        };

        // If this network already allows pow fraud proofs, we should use it instead of assumeutreexo
        let assume_utreexo = match (pow_fraud_proofs, self.config.assume_utreexo) {
            (false, true) => Some(
                floresta_chain::ChainParams::get_assume_utreexo(self.config.network)
                    .expect("Network already validated as supported"),
            ),
            _ => None,
        };

        let proxy = self
            .config
            .proxy
            .as_ref()
            .map(|host| match host.parse::<SocketAddr>() {
                Ok(parsed) => parsed,
                Err(_) => Self::resolve_hostname(host, 9050),
            });

        let config = UtreexoNodeConfig {
            disable_dns_seeds: self.config.disable_dns_seeds,
            network: self.config.network,
            pow_fraud_proofs,
            proxy,
            datadir: data_dir.clone(),
            fixed_peer: self.config.connect.clone(),
            max_banscore: 50,
            compact_filters: self.config.cfilters,
            max_outbound: 10,
            max_inflight: 20,
            assume_utreexo: self.config.assumeutreexo_value.clone().or(assume_utreexo),
            backfill: self.config.backfill,
            filter_start_height: self.config.filters_start_height,
            user_agent: self.config.user_agent.clone(),
            allow_v1_fallback: self.config.allow_v1_fallback,
        };

        let acc = Pollard::new();
        let kill_signal = self.stop_signal.clone();

        // Chain Provider (p2p)
        let chain_provider = UtreexoNode::<_, RunningNode>::new(
            config,
            blockchain_state.clone(),
            Arc::new(tokio::sync::Mutex::new(Mempool::new(acc, 300_000_000))),
            cfilters.clone(),
            kill_signal.clone(),
            AddressMan::default(),
        )
        .expect("Could not create a chain provider");

        // ZMQ
        #[cfg(feature = "zmq-server")]
        {
            info!("Starting ZMQ server");
            if let Ok(zserver) = ZMQServer::new(
                self.config
                    .zmq_address
                    .as_ref()
                    .unwrap_or(&"tcp://127.0.0.1:5150".to_string()),
            ) {
                blockchain_state.subscribe(Arc::new(zserver));
                info!("Done!");
            } else {
                error!("Could not create zmq server, skipping");
            };
        }

        info!("Starting server");
        let wallet = Arc::new(wallet);

        // JSON-RPC
        #[cfg(feature = "json-rpc")]
        {
            let server = tokio::spawn(json_rpc::server::RpcImpl::create(
                blockchain_state.clone(),
                wallet.clone(),
                chain_provider.get_handle(),
                self.stop_signal.clone(),
                self.config.network,
                cfilters.clone(),
                self.config
                    .json_rpc_address
                    .as_ref()
                    .map(|x| Self::resolve_hostname(x, 8332)),
                format!("{data_dir}/output.log"),
            ));

            if self.json_rpc.set(server).is_err() {
                core::panic!("We should be the first one setting this");
            }
        }

        // Electrum Server configuration.

        // Instantiate the Electrum Server.
        let electrum_server = match ElectrumServer::new(
            wallet,
            blockchain_state,
            cfilters,
            chain_provider.get_handle(),
        )
        .await
        {
            Ok(server) => server,
            Err(e) => {
                error!("Could not create an Electrum Server: {e}");
                std::process::exit(1);
            }
        };

        // Default Electrum Server port.
        let default_electrum_port: u16 =
            Self::get_default_electrum_port(self.config.network, false);

        // Electrum Server address.
        let electrum_addr: SocketAddr = self
            .config
            .electrum_address
            .as_ref()
            .map(|addr| Self::resolve_hostname(addr, default_electrum_port))
            .unwrap_or(
                format!("0.0.0.0:{default_electrum_port}")
                    .parse()
                    .expect("Hardcoded address"),
            );

        // sans-TLS Electrum listener.
        let non_tls_listener = match TcpListener::bind(electrum_addr).await {
            Ok(listener) => Arc::new(listener),
            Err(_) => {
                error!(
                    "Failed to bind Electrum Server. Something is already bound to {electrum_addr}"
                );
                std::process::exit(1);
            }
        };
        task::spawn(client_accept_loop(
            non_tls_listener,
            electrum_server.message_transmitter.clone(),
            None,
        ));
        info!("Electrum Server is running at {electrum_addr}");

        if self.config.enable_electrum_tls {
            // Default Electrum TLS port.
            let default_electrum_port_tls: u16 =
                Self::get_default_electrum_port(self.config.network, true);

            // Electrum TLS address.
            let electrum_addr_tls: SocketAddr = self
                .config
                .electrum_address_tls
                .as_ref()
                .map(|addr| Self::resolve_hostname(addr, default_electrum_port_tls))
                .unwrap_or(
                    format!("0.0.0.0:{default_electrum_port_tls}")
                        .parse()
                        .expect("Hardcoded address"),
                );

            // Generate self-signed TLS certificate, if enabled.
            if self.config.generate_cert {
                // Create TLS directory, if it does not exist.
                let tls_dir = format!("{data_dir}/tls");
                if !Path::new(&tls_dir).exists() {
                    fs::create_dir_all(&tls_dir).expect("Could not create the TLS data directory");
                    info!("Created TLS directory at {tls_dir}");
                }

                // Create information for the self-signed certificate about the current node.
                let subject_alt_names = vec!["localhost".to_string()];

                // Define file paths
                let tls_key_path = format!("{data_dir}/tls/key.pem");
                let tls_cert_path = format!("{data_dir}/tls/cert.pem");

                // Create the certificate.
                match Self::generate_self_signed_certificate(
                    tls_key_path.clone(),
                    tls_cert_path.clone(),
                    subject_alt_names,
                ) {
                    Ok(()) => {
                        info!("TLS private key saved to {tls_key_path}");
                        info!("TLS certificate saved to {tls_cert_path}");
                    }
                    Err(e) => {
                        error!("Failed to generate TLS certificate: {e}");
                        std::process::exit(1);
                    }
                }
            }

            // Assemble TLS configuration from file.
            let tls_config = match self.create_tls_config(&data_dir) {
                Ok(config) => config,
                Err(e) => {
                    error!("Failed to create TLS configuration from file: {e}");
                    error!(
                        "You must create the certificate manually or pass the --generate-cert flag"
                    );
                    std::process::exit(1);
                }
            };

            // Electrum TLS accept loop.
            let tls_listener = match TcpListener::bind(electrum_addr_tls).await {
                Ok(listener) => Arc::new(listener),
                Err(_) => {
                    error!(
                    "Failed to bind Electrum TLS Server. Something is already bound to {electrum_addr_tls}"
                );
                    std::process::exit(1);
                }
            };

            // TLS Acceptor.
            let tls_acceptor: TlsAcceptor = TlsAcceptor::from(tls_config);

            task::spawn(client_accept_loop(
                tls_listener,
                electrum_server.message_transmitter.clone(),
                Some(tls_acceptor),
            ));
            info!("Electrum TLS Server is running at {electrum_addr_tls}");
        }

        // Electrum Server's main loop.
        task::spawn(electrum_server.main_loop());

        // Chain provider
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let mut recv = self.stop_notify.lock().unwrap();
        *recv = Some(receiver);

        task::spawn(chain_provider.run(sender));

        // Metrics
        #[cfg(feature = "metrics")]
        {
            let metrics_server_address =
                SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 3333);

            task::spawn(metrics::metrics_server(metrics_server_address));
            info!("Started metrics server on: {metrics_server_address}",);

            // Periodically update memory usage
            tokio::spawn(async {
                let interval = Duration::from_secs(5);
                let mut ticker = time::interval(interval);

                loop {
                    ticker.tick().await;
                    metrics::get_metrics().update_memory_usage();
                }
            });
        }
    }

    fn data_dir_path(config: &Config) -> String {
        // base dir: config.data_dir or $HOME/.floresta or "./.floresta"
        let mut base: PathBuf = config
            .data_dir
            .as_ref()
            .map(|s| s.trim_end_matches(['/', '\\']).into())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".floresta")
            });

        // network-specific subdir
        match config.network {
            Network::Bitcoin => {} // no subdir
            Network::Signet => base.push("signet"),
            Network::Testnet => base.push("testnet3"),
            Network::Testnet4 => base.push("testnet4"),
            Network::Regtest => base.push("regtest"),
            // TODO: handle possible Err
            _ => panic!("This network is not supported: {}", config.network),
        }

        base.to_string_lossy().into_owned()
    }

    fn setup_logger(
        data_dir: &String,
        log_file: bool,
        log_to_stdout: bool,
        debug: bool,
    ) -> Result<(), fern::InitError> {
        let colors = ColoredLevelConfig::new()
            .error(Color::Red)
            .warn(Color::Yellow)
            .info(Color::Green)
            .debug(Color::Blue)
            .trace(Color::BrightBlack);

        let formatter = |use_colors: bool| {
            move |out: FormatCallback, message: &Arguments, record: &Record| {
                out.finish(format_args!(
                    "[{} {} {}] {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    match use_colors {
                        true => colors.color(record.level()).to_string(),
                        false => record.level().to_string(),
                    },
                    record.target(),
                    message
                ))
            }
        };

        let mut dispatchers = fern::Dispatch::new();
        let stdout_dispatcher = fern::Dispatch::new()
            .format(formatter(true))
            .level(if debug {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            })
            .chain(std::io::stdout());

        let file_dispatcher = fern::Dispatch::new()
            .format(formatter(false))
            .level(log::LevelFilter::Info)
            .chain(fern::log_file(format!("{data_dir}/output.log"))?);

        if log_file {
            dispatchers = dispatchers.chain(file_dispatcher);
        }

        if log_to_stdout {
            dispatchers = dispatchers.chain(stdout_dispatcher);
        }

        dispatchers.apply()?;

        Ok(())
    }

    pub fn from_config(config: Config) -> Self {
        Self {
            config,
            stop_signal: Arc::new(RwLock::new(false)),
            stop_notify: Arc::new(Mutex::new(None)),
            #[cfg(feature = "json-rpc")]
            json_rpc: OnceLock::new(),
        }
    }

    pub fn new() -> Self {
        Self::from_config(Config::default())
    }

    /// Loads a config file from disk, returns default if it cannot load it
    fn get_config_file(path: &str) -> ConfigFile {
        let data = ConfigFile::from_file(path);

        if let Ok(data) = data {
            data
        } else {
            match data.unwrap_err() {
                crate::error::Error::TomlParsing(e) => {
                    warn!("Could not parse config file, ignoring it");
                    debug!("{e}");
                    ConfigFile::default()
                }
                crate::error::Error::Io(e) => {
                    warn!("Could not read config file, ignoring it");
                    debug!("{e}");
                    ConfigFile::default()
                }
                // Shouldn't be any other error
                _ => unreachable!(),
            }
        }
    }

    fn get_key_from_env() -> Option<String> {
        let xpub = std::env::var("WALLET_XPUB");
        match xpub {
            Ok(key) => return Some(key),
            Err(e) => match e {
                std::env::VarError::NotPresent => {}
                std::env::VarError::NotUnicode(xpub) => error!("Invalid xpub {xpub:?}"),
            },
        }
        None
    }

    #[cfg(feature = "flat-chainstore")]
    fn load_chain_store(data_dir: String) -> ChainStore {
        let config = FlatChainStoreConfig::new(data_dir + "/chaindata");
        ChainStore::new(config).expect("failure while creating chainstate")
    }

    #[cfg(all(feature = "kv-chainstore", not(doc)))]
    fn load_chain_state(
        data_dir: String,
        network: Network,
        assume_valid: Option<bitcoin::BlockHash>,
    ) -> ChainState<ChainStore<'static>> {
        let db = ChainStore::new(data_dir.clone()).expect("Could not read db");
        let assume_valid =
            assume_valid.map_or(AssumeValidArg::Hardcoded, AssumeValidArg::UserInput);

        match ChainState::<ChainStore>::load_chain_state(db, network, assume_valid) {
            Ok(chainstate) => chainstate,
            Err(err) => match err {
                BlockchainError::ChainNotInitialized => {
                    let db = ChainStore::new(data_dir).expect("Could not read db");

                    ChainState::<ChainStore>::new(db, network, assume_valid)
                }
                _ => unreachable!(),
            },
        }
    }

    #[cfg(feature = "flat-chainstore")]
    fn load_chain_state(
        data_dir: String,
        network: Network,
        assume_valid: Option<bitcoin::BlockHash>,
    ) -> ChainState<ChainStore> {
        let db = Self::load_chain_store(data_dir.clone());

        let assume_valid =
            assume_valid.map_or(AssumeValidArg::Hardcoded, AssumeValidArg::UserInput);

        match ChainState::<ChainStore>::load_chain_state(db, network, assume_valid) {
            Ok(chainstate) => chainstate,
            Err(err) => match err {
                BlockchainError::ChainNotInitialized => {
                    let db = Self::load_chain_store(data_dir);

                    ChainState::<ChainStore>::new(db, network, assume_valid)
                }
                _ => unreachable!(),
            },
        }
    }

    fn load_wallet(data_dir: &String) -> AddressCache<KvDatabase> {
        match KvDatabase::new(data_dir.to_owned()) {
            Ok(database) => AddressCache::new(database),
            Err(_) => {
                error!("Cannot obtain a lock on data directory {data_dir}. Floresta is probably already running.");
                std::process::exit(1);
            }
        }
    }

    fn setup_wallet<D: AddressCacheDatabase>(
        &self,
        data_dir: &str,
        wallet: &mut AddressCache<D>,
    ) -> anyhow::Result<()> {
        // The config file inside our data directory or inside the specified directory
        let config_file = match self.config.config_file {
            Some(ref path) => Self::get_config_file(path),
            None => {
                let default_path = format!("{data_dir}/config.toml");
                Self::get_config_file(&default_path)
            }
        };
        let setup = self.prepare_wallet_setup(config_file)?;

        // Add the configured descriptors and addresses to the wallet
        for descriptor in setup.descriptors {
            let descriptor = descriptor.to_string();
            if !wallet.is_cached(&descriptor)? {
                wallet.push_descriptor(&descriptor)?;
            }
        }
        for addresses in setup.addresses {
            wallet.cache_address(addresses.script_pubkey());
        }

        info!("Wallet setup completed!");
        anyhow::Ok(())
    }

    /// Parses the configured list of xpubs, output descriptors and addresses to watch for, and
    /// returns the constructed `InitialWalletSetup`.
    fn prepare_wallet_setup(
        &self,
        config_file: ConfigFile,
    ) -> Result<InitialWalletSetup, error::Error> {
        let config = &self.config;

        let mut xpubs = Vec::new();
        xpubs.extend(config.wallet_xpub.clone().unwrap_or_default());
        xpubs.extend(config_file.wallet.xpubs.unwrap_or_default());
        xpubs.extend(Self::get_key_from_env());

        let mut descriptors = Vec::new();
        descriptors.extend(config.wallet_descriptor.clone().unwrap_or_default());
        descriptors.extend(config_file.wallet.descriptors.unwrap_or_default());

        let addresses = config_file.wallet.addresses.unwrap_or_default();

        InitialWalletSetup::build(&xpubs, &descriptors, &addresses, config.network, 100)
    }

    /// Get the default Electrum port for the Network and TLS combination.
    ///
    /// Bitcoin  => 50001 (50002 TLS)
    /// Signet   => 60001 (60002 TLS)
    /// Testnet4 => 40001 (40003 TLS)
    /// Testnet3 => 30001 (30002 TLS)
    /// Regtest  => 20001 (20002 TLS)
    fn get_default_electrum_port(network: Network, enable_electrum_tls: bool) -> u16 {
        let mut electrum_port = match network {
            Network::Bitcoin => 50001,
            Network::Signet => 60001,
            Network::Testnet4 => 40001,
            Network::Testnet => 30001,
            Network::Regtest => 20001,
            _ => 50001, // [`bitcoin::Network`] is `non-exhaustive`.
        };

        if enable_electrum_tls {
            electrum_port += 1;
        }

        electrum_port
    }

    /// Generate a self-signed TLS certificate from a random private key.
    pub fn generate_self_signed_certificate(
        tls_key_path: String,
        tls_cert_path: String,
        subject_alt_names: Vec<String>,
    ) -> Result<(), error::Error> {
        // Generate a key pair
        let tls_key_pair = KeyPair::generate().map_err(error::Error::CouldNotGenerateKeypair)?;

        // Generate self-signed certificate
        let mut cert_params = CertificateParams::new(subject_alt_names)
            .map_err(error::Error::CouldNotGenerateCertParam)?;

        cert_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let certificate = cert_params
            .self_signed(&tls_key_pair)
            .map_err(error::Error::CouldNotGenerateSelfSignedCert)?;

        // Create files
        fs::write(&tls_key_path, tls_key_pair.serialize_pem())
            .map_err(|err| error::Error::CouldNotWriteFile(tls_key_path, err))?;

        fs::write(&tls_cert_path, certificate.pem())
            .map_err(|err| error::Error::CouldNotWriteFile(tls_cert_path, err))?;

        Ok(())
    }

    /// Create the TLS configuration from a PKCS#8 private key and certificate.
    fn create_tls_config(&self, data_dir: &str) -> Result<Arc<ServerConfig>, error::Error> {
        // Use an agnostic way to build paths for platforms and fix the differences
        // in how Unix and Windows represent strings, maybe a user could use a weird
        // string on his/her path.
        //
        // See more at https://doc.rust-lang.org/std/ffi/struct.OsStr.html#method.to_string_lossy
        let tls_cert_path = self.config.tls_cert_path.clone().unwrap_or_else(|| {
            PathBuf::from(&data_dir)
                .join("tls")
                .join("cert.pem")
                .to_string_lossy()
                .into_owned()
        });

        let tls_key_path = self.config.tls_key_path.clone().unwrap_or_else(|| {
            PathBuf::from(&data_dir)
                .join("tls")
                .join("key.pem")
                .to_string_lossy()
                .into_owned()
        });

        // Convert paths to a [`Path`] for system-agnostic handling.
        let tls_cert_path = Path::new(&tls_cert_path);
        let tls_key_path = Path::new(&tls_key_path);

        // Parse the certificate's chain from the file.
        let tls_cert_chain =
            CertificateDer::from_pem_file(tls_cert_path).map_err(error::Error::InvalidCert)?;

        // Parse the private key from the file.
        let tls_key =
            PrivateKeyDer::from_pem_file(tls_key_path).map_err(error::Error::InvalidPrivKey)?;

        // Assemble the TLS configuration.
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![tls_cert_chain], tls_key)
            .map_err(error::Error::CouldNotConfigureTLS)?;

        Ok(Arc::new(tls_config))
    }
}

impl Default for Florestad {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Config> for Florestad {
    fn from(config: Config) -> Self {
        Self {
            config,
            stop_signal: Arc::new(RwLock::new(false)),
            stop_notify: Arc::new(Mutex::new(None)),
            #[cfg(feature = "json-rpc")]
            json_rpc: OnceLock::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_dir_path() {
        let mut config = Config::default();

        let expected = dirs::home_dir()
            .unwrap_or(PathBuf::from("."))
            .join(".floresta");

        assert_eq!(
            Florestad::data_dir_path(&config),
            expected.display().to_string(),
        );

        // Using other made-up directories
        config.data_dir = Some("path/to/dir".to_string());
        assert_eq!(Florestad::data_dir_path(&config), "path/to/dir");

        config.data_dir = Some("path/to/dir/".to_string());
        assert_eq!(Florestad::data_dir_path(&config), "path/to/dir");

        config.data_dir = Some(format!("path{}", '\\')); // test removing the \ separator
        assert_eq!(Florestad::data_dir_path(&config), "path");

        config.data_dir = Some("path///".to_string()); // test removing many separators
        assert_eq!(Florestad::data_dir_path(&config), "path");

        // Using other networks
        for &(net, suffix) in &[
            (Network::Testnet, "testnet3"),
            (Network::Testnet4, "testnet4"),
            (Network::Signet, "signet"),
            (Network::Regtest, "regtest"),
        ] {
            let expected = PathBuf::from("path").join(suffix);
            config.network = net;

            assert_eq!(
                Florestad::data_dir_path(&config),
                expected.display().to_string(),
            );
        }
    }
}
