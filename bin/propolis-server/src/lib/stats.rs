// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for starting an Oximeter endpoint and gathering server-level stats.

use dropshot::{ConfigDropshot, HandlerTaskMode};
use omicron_common::api::internal::nexus::ProducerEndpoint;
use omicron_common::api::internal::nexus::ProducerKind;
use oximeter::types::ProducerRegistry;
use oximeter::{
    types::{Cumulative, Sample},
    Metric, MetricsError, Producer,
};
use oximeter_instruments::kstat::KstatSampler;
use oximeter_producer::Error;
use oximeter_producer::{Config, Server};
#[cfg(not(test))]
use slog::error;
use slog::{info, Logger};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::server::MetricsEndpointConfig;
use crate::stats::virtual_machine::VirtualMachine;

mod pvpanic;
pub(crate) mod virtual_machine;
pub use self::pvpanic::PvpanicProducer;

// Interval on which we ask `oximeter` to poll us for metric data.
const OXIMETER_STAT_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

// Interval on which we produce vCPU metrics.
#[cfg(not(test))]
const VCPU_KSTAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

// The kstat sampler includes a limit to its internal buffers for each target,
// to avoid growing without bound. This defaults to 500 samples. Since we have 5
// vCPU microstates for which we track occupancy and up to 64 vCPUs, we can
// easily run up against this default.
//
// This limit provides extra space for up to 64 samples per vCPU per microstate,
// to ensure we don't throw away too much data if oximeter cannot reach us.
#[cfg(not(test))]
const KSTAT_LIMIT_PER_VCPU: u32 =
    crate::stats::virtual_machine::N_VCPU_MICROSTATES * 64;

/// An Oximeter `Metric` that specifies the number of times an instance was
/// reset via the server API.
#[derive(Debug, Default, Copy, Clone, Metric)]
struct Reset {
    /// The number of times this instance was reset via the API.
    #[datum]
    pub count: Cumulative<u64>,
}

/// The full set of server-level metrics, collated by
/// [`ServerStatsOuter::produce`] into the types needed to relay these
/// statistics to Oximeter.
#[derive(Clone, Debug)]
struct ServerStats {
    /// The oximeter Target identifying this instance as the source of metric
    /// data.
    virtual_machine: VirtualMachine,

    /// The reset count for the relevant instance.
    run_count: Reset,
}

impl ServerStats {
    pub fn new(virtual_machine: VirtualMachine) -> Self {
        ServerStats { virtual_machine, run_count: Default::default() }
    }
}

/// The public wrapper for server-level metrics.
#[derive(Clone, Debug)]
pub struct ServerStatsOuter {
    server_stats_wrapped: Arc<Mutex<ServerStats>>,
    kstat_sampler: Option<KstatSampler>,
}

impl ServerStatsOuter {
    /// Increments the number of times the instance was reset.
    pub fn count_reset(&self) {
        let mut inner = self.server_stats_wrapped.lock().unwrap();
        let datum = inner.run_count.datum_mut();
        *datum += 1;
    }
}

impl Producer for ServerStatsOuter {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample> + 'static>, MetricsError> {
        let run_count = {
            let inner = self.server_stats_wrapped.lock().unwrap();
            std::iter::once(Sample::new(
                &inner.virtual_machine,
                &inner.run_count,
            )?)
        };
        if let Some(sampler) = self.kstat_sampler.as_mut() {
            let samples = sampler.produce()?;
            Ok(Box::new(run_count.chain(samples)))
        } else {
            Ok(Box::new(run_count))
        }
    }
}

/// Launches and returns an Oximeter metrics server.
///
/// # Parameters
///
/// - `id`: The ID of the instance for whom this server is being started.
/// - `config`: The metrics config options, including our address (on which we
/// serve metrics for oximeter to collect), and the registration address (a
/// Nexus instance through which we request registration as an oximeter
/// producer).
/// - `log`: A logger to use when logging from this routine.
/// - `registry`: The oximeter [`ProducerRegistry`] that the spawned server will
/// use to return metric data to oximeter on request.
///
/// This method attempts to register a _single time_ with Nexus. Callers should
/// arrange for this to be called continuously if desired, such as with a
/// backoff policy.
pub async fn start_oximeter_server(
    id: Uuid,
    config: &MetricsEndpointConfig,
    log: &Logger,
    registry: &ProducerRegistry,
) -> Result<Server, Error> {
    // Request an ephemeral port on which to serve metrics.
    let my_address = SocketAddr::new(config.propolis_addr.ip(), 0);
    let registration_address = config.metric_addr;
    info!(
        log,
        "Attempting to register with Nexus as a metric producer";
        "my_address" => %my_address,
        "nexus_address" => %registration_address,
    );

    let dropshot_config = ConfigDropshot {
        bind_address: my_address,
        request_body_max_bytes: 2048,
        default_handler_task_mode: HandlerTaskMode::Detached,
    };

    let server_info = ProducerEndpoint {
        id,
        kind: ProducerKind::Instance,
        address: my_address,
        base_route: "/collect".to_string(),
        interval: OXIMETER_STAT_INTERVAL,
    };

    // Create a child logger, to avoid intermingling the producer server output
    // with the main Propolis server.
    let producer_log = oximeter_producer::LogConfig::Logger(
        log.new(slog::o!("component" => "oximeter-producer")),
    );
    let config = Config {
        server_info,
        registration_address,
        dropshot: dropshot_config,
        log: producer_log,
    };

    // Create the server which will attempt to register with Nexus.
    Server::with_registry(registry.clone(), &config).await
}

/// Creates and registers a set of server-level metrics for an instance.
///
/// This attempts to initialize kstat-based metrics for vCPU usage data. This
/// may fail, in which case those metrics will be unavailable.
pub async fn register_server_metrics(
    registry: &ProducerRegistry,
    virtual_machine: VirtualMachine,
    log: &Logger,
) -> anyhow::Result<ServerStatsOuter> {
    let stats = ServerStats::new(virtual_machine.clone());

    // Setup the collection of kstats for this instance.
    let kstat_sampler = setup_kstat_tracking(log, virtual_machine).await;
    let stats_outer = ServerStatsOuter {
        server_stats_wrapped: Arc::new(Mutex::new(stats)),
        kstat_sampler,
    };

    registry.register_producer(stats_outer.clone())?;

    Ok(stats_outer)
}

#[cfg(test)]
async fn setup_kstat_tracking(
    log: &Logger,
    _: VirtualMachine,
) -> Option<KstatSampler> {
    slog::debug!(log, "kstat sampling disabled during tests");
    None
}

#[cfg(not(test))]
async fn setup_kstat_tracking(
    log: &Logger,
    virtual_machine: VirtualMachine,
) -> Option<KstatSampler> {
    let kstat_limit =
        usize::try_from(virtual_machine.n_vcpus() * KSTAT_LIMIT_PER_VCPU)
            .unwrap();
    match KstatSampler::with_sample_limit(log, kstat_limit) {
        Ok(sampler) => {
            let details = oximeter_instruments::kstat::CollectionDetails::never(
                VCPU_KSTAT_INTERVAL,
            );
            if let Err(e) = sampler.add_target(virtual_machine, details).await {
                error!(
                    log,
                    "failed to add VirtualMachine target, \
                    vCPU stats will be unavailable";
                    "error" => ?e,
                );
            }
            Some(sampler)
        }
        Err(e) => {
            error!(
                log,
                "failed to create KstatSampler, \
                vCPU stats will be unavailable";
                "error" => ?e,
            );
            None
        }
    }
}
