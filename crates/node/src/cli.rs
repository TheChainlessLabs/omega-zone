//! Tempo Zone CLI.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, Bytes};
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, CommandFactory, FromArgMatches};
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::info;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use url::Url;
use zone_evm::ZoneEvmConfig;
use zone_payload::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;
use zone_sequencer::{
    BatchAnchorConfig,
    proof::{ProofBackend, TeeAttestationFormat, TeeProviderOptions},
};

use crate::{
    ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig, dev::DevCommand,
    rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};
use zone_rpc::refprice::{ReferencePriceProviderConfig, ReferencePriceProviderKind};

const MAX_LOGS_PER_RESPONSE: u64 = 1_000_000;
const MAX_BLOCKS_PER_FILTER: u64 = 1_000_000;

const ZONE_LOG_FILTER_DIRECTIVES: &str = concat!(
    "tungstenite=warn,",
    "alloy_pubsub=warn,",
    "alloy_transport_ws=warn,",
    "rustls::client=warn"
);

/// Tempo Zone CLI entry point.
pub enum ZoneCli {
    Node(Box<Cli<TempoChainSpecParser, ZoneArgs>>),
    Dev(DevCommand),
}

impl ZoneCli {
    fn command() -> clap::Command {
        Cli::<TempoChainSpecParser, ZoneArgs>::command()
            .about("Tempo Zone")
            .subcommand(DevCommand::command())
    }

    /// Parse CLI arguments from the environment.
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    /// Parse CLI arguments from an iterator. The first item is the binary name.
    pub fn parse_from<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::try_parse_from(args).unwrap_or_else(|err| err.exit())
    }

    /// Try to parse CLI arguments from an iterator.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = Self::command().try_get_matches_from(args)?;
        if let Some(("dev", dev_matches)) = matches.subcommand() {
            return DevCommand::from_arg_matches(dev_matches).map(Self::Dev);
        }
        Cli::from_arg_matches(&matches)
            .map(Box::new)
            .map(Self::Node)
    }

    /// Run the Tempo Zone node.
    ///
    /// Configures the node builder, launches the zone node with all sequencer
    /// background tasks, and blocks until exit.
    pub fn run(self) -> eyre::Result<()> {
        match self {
            Self::Node(cli) => run_node(*cli),
            Self::Dev(command) => command.run(),
        }
    }
}

/// Main entry point for the `node` command.
fn run_node(mut cli: Cli<TempoChainSpecParser, ZoneArgs>) -> eyre::Result<()> {
    prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
    prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);

    let components = |spec: Arc<TempoChainSpec>| {
        (
            ZoneEvmConfig::new_without_l1(spec),
            NoopConsensus::default(),
        )
    };

    cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        validate_l1_rpc_url(&args.l1_rpc_url)?;

        builder.config_mut().network.discovery.disable_discovery = true;
        builder.config_mut().rpc.disable_auth_server = true;
        builder.config_mut().rpc.rpc_max_logs_per_response = MAX_LOGS_PER_RESPONSE.into();
        builder.config_mut().rpc.rpc_max_blocks_per_filter = MAX_BLOCKS_PER_FILTER.into();

        let tee_options = args.tee_provider_options()?;

        let mut node = ZoneNode::new(
            args.l1_rpc_url,
            args.portal_address,
            args.l1_genesis_block_number,
            args.l1_fetch_concurrency,
            Duration::from_millis(args.l1_retry_connection_interval_ms),
        )
        .with_withdrawal_batch_interval_blocks(args.zone_batch_interval_blocks)
        .with_private_rpc(ZonePrivateRpcConfig {
            private_rpc_port: args.private_rpc_port,
            zone_id: args.zone_id,
            max_auth_token_validity: Duration::from_secs(
                args.private_rpc_max_auth_token_validity_secs,
            ),
            ref_price_provider: build_ref_price_provider(
                args.ref_price_static_price,
                &args.ref_price_source,
                args.ref_price_max_deviation_bps,
                args.ref_price_max_staleness_secs,
            ),
        });

        if args.enable_sequencer {
            let sequencer_signer: PrivateKeySigner = args
                .sequencer_key
                .parse()
                .expect("invalid sequencer private key");
            let proof_provider = args
                .proof_backend
                .into_provider(tee_options)
                .map_err(|err| eyre::eyre!("failed to construct proof provider: {err}"))?;
            node = node.with_sequencer(ZoneSequencerAddOnsConfig {
                sequencer_signer,
                zone_id: args.zone_id,
                zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                batch_interval_blocks: args.zone_batch_interval_blocks,
                batch_anchor_config: BatchAnchorConfig::default(),
                withdrawal_poll_interval: Duration::from_secs(args.withdrawal_poll_interval_secs),
                proof_provider,
            });
        }

        let handle = builder.node(node).launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, Args)]
pub struct ZoneArgs {
    /// Certified Tempo follower WebSocket RPC URL for finalized L1 state, deposit events, and chain notifications.
    #[arg(long = "l1.rpc-url", env = "L1_RPC_URL")]
    pub l1_rpc_url: String,

    /// ZonePortal contract address on L1.
    #[arg(long = "l1.portal-address", env = "L1_PORTAL_ADDRESS")]
    pub portal_address: Address,

    /// Block building interval in milliseconds.
    #[arg(
        long = "block.interval-ms",
        env = "BLOCK_INTERVAL_MS",
        default_value_t = 250
    )]
    pub block_interval_ms: u64,

    /// Sequencer private key (hex, with or without 0x prefix).
    #[arg(long = "sequencer-key", env = "SEQUENCER_KEY", hide_env_values = true)]
    pub sequencer_key: String,

    /// How often (in seconds) the zone monitor polls for new L2 blocks.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value_t = 1
    )]
    pub zone_poll_interval_secs: u64,

    /// Number of zone blocks between withdrawal batch boundaries.
    ///
    /// Also used by the sequencer monitor to decide when enough chain progress has
    /// occurred to look for empty finalized batches to submit to L1. Default 120 is
    /// ~1 minute at Tempo's expected 500 ms block time.
    #[arg(
        long = "zone.batch-interval-blocks",
        env = "ZONE_BATCH_INTERVAL_BLOCKS",
        default_value_t = DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS
    )]
    pub zone_batch_interval_blocks: u64,

    /// How often (in seconds) the withdrawal processor polls the L1 queue.
    #[arg(
        long = "withdrawal-poll-interval-secs",
        env = "WITHDRAWAL_POLL_INTERVAL_SECS",
        default_value_t = 5
    )]
    pub withdrawal_poll_interval_secs: u64,

    /// Genesis Tempo L1 block number override.
    #[arg(long = "l1.genesis-block-number", env = "L1_GENESIS_BLOCK_NUMBER")]
    pub l1_genesis_block_number: Option<u64>,

    /// Maximum number of concurrent L1 receipt fetches.
    #[arg(
        long = "l1.fetch-concurrency",
        env = "L1_FETCH_CONCURRENCY",
        default_value_t = 4
    )]
    pub l1_fetch_concurrency: usize,

    /// Interval in milliseconds between WebSocket reconnection attempts to L1.
    #[arg(
        long = "l1.retry-connection-interval",
        env = "L1_RETRY_CONNECTION_INTERVAL_MS",
        default_value_t = 100
    )]
    pub l1_retry_connection_interval_ms: u64,

    /// Zone ID for the private RPC auth token validation.
    #[arg(long = "zone.id", env = "ZONE_ID", default_value_t = 0)]
    pub zone_id: u32,

    /// Port for the private zone RPC server (0 for OS-assigned).
    #[arg(
        long = "private-rpc.port",
        env = "PRIVATE_RPC_PORT",
        default_value_t = 8544
    )]
    pub private_rpc_port: u16,

    /// Maximum auth token validity window the private RPC accepts, in seconds.
    #[arg(
        long = "private-rpc.max-auth-token-validity-secs",
        env = "PRIVATE_RPC_MAX_AUTH_TOKEN_VALIDITY_SECS",
        default_value_t = DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS
    )]
    pub private_rpc_max_auth_token_validity_secs: u64,

    /// Enable the Zone node in sequencer mode. This advances block production and submits
    /// withdrawal batches.
    #[arg(long = "sequencer", env = "SEQUENCER")]
    pub enable_sequencer: bool,

    /// Proof backend used to build `verifierConfig` / `proof` bytes for each
    /// batch. `fail-fast` (default) refuses to submit until an operator opts
    /// in; `empty-legacy` keeps the pre-TEE behaviour for permissive dev
    /// verifiers; `tee` enables the TEE attestation provider — with an
    /// `--proof.tee.endpoint` set the sequencer POSTs each batch's public
    /// inputs to that service and forwards the returned `verifierConfig` /
    /// `proof` bytes; without an endpoint it stays diagnostic-only (logs the
    /// commitment that *would* be signed and refuses to submit).
    #[arg(
        long = "proof.backend",
        env = "PROOF_BACKEND",
        default_value = "fail-fast"
    )]
    pub proof_backend: ProofBackend,

    /// HTTP(S) endpoint of the TEE attestation service the sequencer will
    /// POST [`TeeAttestationRequest`] payloads to. Leaving this unset with
    /// `--proof.backend=tee` keeps the diagnostic-only
    /// `PendingTeeAttestationProvider` behaviour (no L1 submission).
    #[arg(long = "proof.tee.endpoint", env = "PROOF_TEE_ENDPOINT")]
    pub proof_tee_endpoint: Option<Url>,

    /// Optional bearer token forwarded to the attestation service as the
    /// `Authorization` header. Only honoured when `--proof.tee.endpoint` is set.
    #[arg(long = "proof.tee.auth-bearer", env = "PROOF_TEE_AUTH_BEARER")]
    pub proof_tee_auth_bearer: Option<String>,

    /// Per-request timeout for the attestation service, in seconds.
    #[arg(
        long = "proof.tee.timeout-secs",
        env = "PROOF_TEE_TIMEOUT_SECS",
        default_value_t = 15
    )]
    pub proof_tee_timeout_secs: u64,

    /// Enclave identity hash to advertise in the request (hex string, with or
    /// without `0x` prefix). Echoed back in the response's `verifierConfig`.
    #[arg(long = "proof.tee.enclave-id", env = "PROOF_TEE_ENCLAVE_ID")]
    pub proof_tee_enclave_id: Option<String>,

    /// Domain separator to bind into the request (hex string, with or without
    /// `0x` prefix). Typically the portal address concatenated with the zone id.
    #[arg(long = "proof.tee.domain", env = "PROOF_TEE_DOMAIN")]
    pub proof_tee_domain: Option<String>,

    /// Best-known attestation flavour the service produces. Tags the request so
    /// the service can refuse early on a mismatch. One of `sev-snp`,
    /// `nitro-enclaves`, `intel-tdx`, or `unconfirmed` (default).
    #[arg(
        long = "proof.tee.format",
        env = "PROOF_TEE_FORMAT",
        default_value = "unconfirmed"
    )]
    pub proof_tee_format: TeeAttestationFormat,

    /// Static reference price (raw integer, same units as the orderbook
    /// precompile) for the configured darkpool market. When unset, the
    /// reference-price provider stays disabled and `zone_getReferencePrice`
    /// returns `enabled: false`. Setting this opts into the static provider.
    #[arg(long = "ref-price.static-price", env = "REF_PRICE_STATIC_PRICE")]
    pub ref_price_static_price: Option<u128>,

    /// Origin tag surfaced to clients alongside the static reference price.
    /// Ignored when `--ref-price.static-price` is unset.
    #[arg(
        long = "ref-price.source",
        env = "REF_PRICE_SOURCE",
        default_value = "static:alpha"
    )]
    pub ref_price_source: String,

    /// Maximum allowed deviation between an order's limit price and the
    /// reference price, in basis points. `1000` = ±10%. Ignored when
    /// `--ref-price.static-price` is unset.
    #[arg(
        long = "ref-price.max-deviation-bps",
        env = "REF_PRICE_MAX_DEVIATION_BPS",
        default_value_t = 1_000
    )]
    pub ref_price_max_deviation_bps: u32,

    /// Maximum staleness window in seconds; `0` disables the staleness check.
    /// `0` is the natural default for a static provider (no real freshness
    /// signal). Ignored when `--ref-price.static-price` is unset.
    #[arg(
        long = "ref-price.max-staleness-secs",
        env = "REF_PRICE_MAX_STALENESS_SECS",
        default_value_t = 0
    )]
    pub ref_price_max_staleness_secs: u64,
}

impl ZoneArgs {
    /// Collapse the `--proof.tee.*` flags into a [`TeeProviderOptions`].
    pub(crate) fn tee_provider_options(&self) -> eyre::Result<TeeProviderOptions> {
        let enclave_id = parse_optional_hex(
            self.proof_tee_enclave_id.as_deref(),
            "--proof.tee.enclave-id",
        )?;
        let domain = parse_optional_hex(self.proof_tee_domain.as_deref(), "--proof.tee.domain")?;
        Ok(TeeProviderOptions {
            endpoint: self.proof_tee_endpoint.clone(),
            bearer_token: self.proof_tee_auth_bearer.clone(),
            request_timeout: Some(Duration::from_secs(self.proof_tee_timeout_secs)),
            enclave_id,
            domain,
            format: self.proof_tee_format,
        })
    }
}

fn parse_optional_hex(value: Option<&str>, flag: &str) -> eyre::Result<Bytes> {
    let Some(raw) = value else {
        return Ok(Bytes::new());
    };
    let trimmed = raw.trim_start_matches("0x").trim_start_matches("0X");
    let decoded = const_hex::decode(trimmed)
        .map_err(|err| eyre::eyre!("{flag}: invalid hex value `{raw}`: {err}"))?;
    Ok(Bytes::from(decoded))
}

/// Translate the alpha reference-price CLI knobs into an optional provider
/// configuration. Returns `None` when no static price was supplied so the
/// `zone_getReferencePrice` method stays explicitly disabled.
///
/// Kept as a pure function so the alpha config wiring is unit-testable
/// without spinning up the full CLI parser.
pub fn build_ref_price_provider(
    static_price: Option<u128>,
    source: &str,
    max_deviation_bps: u32,
    max_staleness_secs: u64,
) -> Option<ReferencePriceProviderConfig> {
    let price = static_price?;
    Some(ReferencePriceProviderConfig {
        max_deviation_bps,
        max_staleness_secs,
        kind: ReferencePriceProviderKind::Static {
            price,
            source: source.to_string(),
        },
    })
}

fn prepend_log_filter(filter: &mut String, directives: &str) {
    if filter.is_empty() {
        *filter = directives.to_owned();
    } else {
        *filter = format!("{directives},{filter}");
    }
}

#[cfg(test)]
mod tests {
    use super::{ReferencePriceProviderKind, build_ref_price_provider};

    #[test]
    fn ref_price_provider_is_disabled_when_static_price_missing() {
        assert!(build_ref_price_provider(None, "static:alpha", 1_000, 0).is_none());
    }

    #[test]
    fn ref_price_provider_is_static_when_price_supplied() {
        let provider = build_ref_price_provider(Some(1_234_000), "static:alpha", 250, 90)
            .expect("static price must materialize the provider");

        assert_eq!(provider.max_deviation_bps, 250);
        assert_eq!(provider.max_staleness_secs, 90);
        match provider.kind {
            ReferencePriceProviderKind::Static { price, source } => {
                assert_eq!(price, 1_234_000);
                assert_eq!(source, "static:alpha");
            }
        }
    }

    #[test]
    fn ref_price_provider_propagates_custom_source_tag() {
        let provider = build_ref_price_provider(Some(1), "static:demo-pin", 0, 0)
            .expect("provider must be present when price is set");
        match provider.kind {
            ReferencePriceProviderKind::Static { source, .. } => {
                assert_eq!(source, "static:demo-pin");
            }
        }
    }

    #[test]
    fn ref_price_provider_accepts_zero_deviation_and_zero_staleness() {
        // Zero deviation = exact-equality bound; zero staleness = no expiry.
        // Both are documented config choices, so the helper must accept them.
        let provider = build_ref_price_provider(Some(5_000_000), "static:alpha", 0, 0)
            .expect("provider must materialize for zero-bound config");
        assert_eq!(provider.max_deviation_bps, 0);
        assert_eq!(provider.max_staleness_secs, 0);
    }
}

fn validate_l1_rpc_url(l1_rpc_url: &str) -> eyre::Result<()> {
    let url: url::Url = l1_rpc_url
        .parse()
        .map_err(|err| eyre::eyre!("failed parsing --l1.rpc-url as URL: {err}"))?;
    eyre::ensure!(
        matches!(url.scheme(), "ws" | "wss"),
        "--l1.rpc-url must use ws:// or wss://, got `{}`",
        url.scheme()
    );
    Ok(())
}

#[cfg(test)]
mod upstream_tests {
    use super::{ZoneCli, validate_l1_rpc_url};

    #[test]
    fn top_level_help_lists_dev_subcommand() {
        let result = ZoneCli::try_parse_from(["tempo-zone", "--help"]);
        let error = result.err().expect("--help exits through clap");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("  dev"));
    }

    #[test]
    fn dev_is_parsed_by_the_top_level_cli() {
        let parsed = ZoneCli::try_parse_from(["tempo-zone", "dev"]).unwrap();
        assert!(matches!(parsed, ZoneCli::Dev(_)));
    }

    #[test]
    fn l1_rpc_url_accepts_websocket_schemes() {
        validate_l1_rpc_url("ws://localhost:8546").unwrap();
        validate_l1_rpc_url("wss://rpc.moderato.tempo.xyz").unwrap();
    }

    #[test]
    fn l1_rpc_url_rejects_non_websocket_schemes() {
        assert!(validate_l1_rpc_url("http://localhost:8545").is_err());
        assert!(validate_l1_rpc_url("https://rpc.moderato.tempo.xyz").is_err());
    }
}
