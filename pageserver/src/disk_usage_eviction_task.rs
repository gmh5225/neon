//! This module implements the pageserver-global disk-usage-based layer eviction task.
//!
//! # Mechanics
//!
//! Function `launch_disk_usage_global_eviction_task` starts a pageserver-global background
//! loop that evicts layers in response to a shortage of available bytes
//! in the $repo/tenants directory's filesystem.
//!
//! The loop runs periodically at a configurable `period`.
//!
//! Each loop iteration uses `statvfs` to determine filesystem-level space usage.
//! It compares the returned usage data against two different types of thresholds.
//! The iteration tries to evict layers until app-internal accounting says we should be below the thresholds.
//! We cross-check this internal accounting with the real world by making another `statvfs` at the end of the iteration.
//! We're good if that second statvfs shows that we're _actually_ below the configured thresholds.
//! If we're still above one or more thresholds, we emit a warning log message, leaving it to the operator to investigate further.
//!
//! # Eviction Policy
//!
//! There are two thresholds:
//! `max_usage_pct` is the relative available space, expressed in percent of the total filesystem space.
//! If the actual usage is higher, the threshold is exceeded.
//! `min_avail_bytes` is the absolute available space in bytes.
//! If the actual usage is lower, the threshold is exceeded.
//! If either of these thresholds is exceeded, the system is considered to have "disk pressure", and eviction
//! is performed on the next iteration, to release disk space and bring the usage below the thresholds again.
//! The iteration evicts layers in LRU fashion, but, with a weak reservation per tenant.
//! The reservation is to keep the most recently accessed X bytes per tenant resident.
//! If we cannot relieve pressure by evicting layers outside of the reservation, we
//! start evicting layers that are part of the reservation, LRU first.
//!
//! The value for the per-tenant reservation is referred to as `tenant_min_resident_size`
//! throughout the code, but, no actual variable carries that name.
//! The per-tenant default value is the `max(tenant's layer file sizes, regardless of local or remote)`.
//! The idea is to allow at least one layer to be resident per tenant, to ensure it can make forward progress
//! during page reconstruction.
//! An alternative default for all tenants can be specified in the `tenant_config` section of the config.
//! Lastly, each tenant can have an override in their respective tenant config (`min_resident_size_override`).

// Implementation notes:
// - The `#[allow(dead_code)]` above various structs are to suppress warnings about only the Debug impl
//   reading these fields. We use the Debug impl for semi-structured logging, though.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use camino::Utf8Path;
use remote_storage::GenericRemoteStorage;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn, Instrument};
use utils::completion;
use utils::serde_percent::Percent;

use crate::{
    config::PageServerConf,
    task_mgr::{self, TaskKind, BACKGROUND_RUNTIME},
    tenant::{
        self,
        storage_layer::{AsLayerDesc, EvictionError, Layer},
        Timeline,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsageEvictionTaskConfig {
    pub max_usage_pct: Percent,
    pub min_avail_bytes: u64,
    #[serde(with = "humantime_serde")]
    pub period: Duration,
    #[cfg(feature = "testing")]
    pub mock_statvfs: Option<crate::statvfs::mock::Behavior>,
    /// Select sorting for evicted layers
    #[serde(default)]
    pub eviction_order: EvictionOrder,
}

/// Selects the sort order for eviction candidates *after* per tenant `min_resident_size`
/// partitioning.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "args")]
pub enum EvictionOrder {
    /// Order the layers to be evicted by how recently they have been accessed in absolute
    /// time.
    ///
    /// This strategy is unfair when some tenants grow faster than others towards the slower
    /// growing.
    #[default]
    AbsoluteAccessed,

    /// Order the layers to be evicted by how recently they have been accessed relatively within
    /// the set of resident layers of a tenant.
    ///
    /// This strategy will evict layers more fairly but is untested.
    RelativeAccessed {
        #[serde(default)]
        highest_layer_count_loses_first: bool,
    },
}

impl EvictionOrder {
    /// Return true, if with [`Self::RelativeAccessed`] order the tenants with the highest layer
    /// counts should be the first ones to have their layers evicted.
    fn highest_layer_count_loses_first(&self) -> bool {
        match self {
            EvictionOrder::AbsoluteAccessed => false,
            EvictionOrder::RelativeAccessed {
                highest_layer_count_loses_first,
            } => *highest_layer_count_loses_first,
        }
    }
}

#[derive(Default)]
pub struct State {
    /// Exclude http requests and background task from running at the same time.
    mutex: tokio::sync::Mutex<()>,
}

pub fn launch_disk_usage_global_eviction_task(
    conf: &'static PageServerConf,
    storage: GenericRemoteStorage,
    state: Arc<State>,
    background_jobs_barrier: completion::Barrier,
) -> anyhow::Result<()> {
    let Some(task_config) = &conf.disk_usage_based_eviction else {
        info!("disk usage based eviction task not configured");
        return Ok(());
    };

    info!("launching disk usage based eviction task");

    task_mgr::spawn(
        BACKGROUND_RUNTIME.handle(),
        TaskKind::DiskUsageEviction,
        None,
        None,
        "disk usage based eviction",
        false,
        async move {
            let cancel = task_mgr::shutdown_token();

            // wait until initial load is complete, because we cannot evict from loading tenants.
            tokio::select! {
                _ = cancel.cancelled() => { return Ok(()); },
                _ = background_jobs_barrier.wait() => { }
            };

            disk_usage_eviction_task(&state, task_config, &storage, &conf.tenants_path(), cancel)
                .await;
            Ok(())
        },
    );

    Ok(())
}

#[instrument(skip_all)]
async fn disk_usage_eviction_task(
    state: &State,
    task_config: &DiskUsageEvictionTaskConfig,
    storage: &GenericRemoteStorage,
    tenants_dir: &Utf8Path,
    cancel: CancellationToken,
) {
    scopeguard::defer! {
        info!("disk usage based eviction task finishing");
    };

    use crate::tenant::tasks::random_init_delay;
    {
        if random_init_delay(task_config.period, &cancel)
            .await
            .is_err()
        {
            return;
        }
    }

    let mut iteration_no = 0;
    loop {
        iteration_no += 1;
        let start = Instant::now();

        async {
            let res = disk_usage_eviction_task_iteration(
                state,
                task_config,
                storage,
                tenants_dir,
                &cancel,
            )
            .await;

            match res {
                Ok(()) => {}
                Err(e) => {
                    // these stat failures are expected to be very rare
                    warn!("iteration failed, unexpected error: {e:#}");
                }
            }
        }
        .instrument(tracing::info_span!("iteration", iteration_no))
        .await;

        let sleep_until = start + task_config.period;
        if tokio::time::timeout_at(sleep_until, cancel.cancelled())
            .await
            .is_ok()
        {
            break;
        }
    }
}

pub trait Usage: Clone + Copy + std::fmt::Debug {
    fn has_pressure(&self) -> bool;
    fn add_available_bytes(&mut self, bytes: u64);
}

async fn disk_usage_eviction_task_iteration(
    state: &State,
    task_config: &DiskUsageEvictionTaskConfig,
    storage: &GenericRemoteStorage,
    tenants_dir: &Utf8Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let usage_pre = filesystem_level_usage::get(tenants_dir, task_config)
        .context("get filesystem-level disk usage before evictions")?;
    let res = disk_usage_eviction_task_iteration_impl(
        state,
        storage,
        usage_pre,
        task_config.eviction_order,
        cancel,
    )
    .await;
    match res {
        Ok(outcome) => {
            debug!(?outcome, "disk_usage_eviction_iteration finished");
            match outcome {
                IterationOutcome::NoPressure | IterationOutcome::Cancelled => {
                    // nothing to do, select statement below will handle things
                }
                IterationOutcome::Finished(outcome) => {
                    // Verify with statvfs whether we made any real progress
                    let after = filesystem_level_usage::get(tenants_dir, task_config)
                        // It's quite unlikely to hit the error here. Keep the code simple and bail out.
                        .context("get filesystem-level disk usage after evictions")?;

                    debug!(?after, "disk usage");

                    if after.has_pressure() {
                        // Don't bother doing an out-of-order iteration here now.
                        // In practice, the task period is set to a value in the tens-of-seconds range,
                        // which will cause another iteration to happen soon enough.
                        // TODO: deltas between the three different usages would be helpful,
                        // consider MiB, GiB, TiB
                        warn!(?outcome, ?after, "disk usage still high");
                    } else {
                        info!(?outcome, ?after, "disk usage pressure relieved");
                    }
                }
            }
        }
        Err(e) => {
            error!("disk_usage_eviction_iteration failed: {:#}", e);
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
#[allow(clippy::large_enum_variant)]
pub enum IterationOutcome<U> {
    NoPressure,
    Cancelled,
    Finished(IterationOutcomeFinished<U>),
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct IterationOutcomeFinished<U> {
    /// The actual usage observed before we started the iteration.
    before: U,
    /// The expected value for `after`, according to internal accounting, after phase 1.
    planned: PlannedUsage<U>,
    /// The outcome of phase 2, where we actually do the evictions.
    ///
    /// If all layers that phase 1 planned to evict _can_ actually get evicted, this will
    /// be the same as `planned`.
    assumed: AssumedUsage<U>,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct AssumedUsage<U> {
    /// The expected value for `after`, after phase 2.
    projected_after: U,
    /// The layers we failed to evict during phase 2.
    failed: LayerCount,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct PlannedUsage<U> {
    respecting_tenant_min_resident_size: U,
    fallback_to_global_lru: Option<U>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize)]
struct LayerCount {
    file_sizes: u64,
    count: usize,
}

pub(crate) async fn disk_usage_eviction_task_iteration_impl<U: Usage>(
    state: &State,
    _storage: &GenericRemoteStorage,
    usage_pre: U,
    eviction_order: EvictionOrder,
    cancel: &CancellationToken,
) -> anyhow::Result<IterationOutcome<U>> {
    // use tokio's mutex to get a Sync guard (instead of std::sync::Mutex)
    let _g = state
        .mutex
        .try_lock()
        .map_err(|_| anyhow::anyhow!("iteration is already executing"))?;

    debug!(?usage_pre, "disk usage");

    if !usage_pre.has_pressure() {
        return Ok(IterationOutcome::NoPressure);
    }

    warn!(
        ?usage_pre,
        "running disk usage based eviction due to pressure"
    );

    let candidates = match collect_eviction_candidates(eviction_order, cancel).await? {
        EvictionCandidates::Cancelled => {
            return Ok(IterationOutcome::Cancelled);
        }
        EvictionCandidates::Finished(partitioned) => partitioned,
    };

    // Debug-log the list of candidates
    let now = SystemTime::now();
    for (i, (partition, candidate)) in candidates.iter().enumerate() {
        let nth = i + 1;
        let desc = candidate.layer.layer_desc();
        let total_candidates = candidates.len();
        let size = desc.file_size;
        let rel = candidate.relative_last_activity;
        debug!(
            "cand {nth}/{total_candidates}: size={size}, rel_last_activity={rel}, no_access_for={}us, partition={partition:?}, {}/{}/{}",
            now.duration_since(candidate.last_activity_ts)
                .unwrap()
                .as_micros(),
            desc.tenant_shard_id,
            desc.timeline_id,
            candidate.layer,
        );
    }

    // phase1: select victims to relieve pressure
    //
    // Walk through the list of candidates, until we have accumulated enough layers to get
    // us back under the pressure threshold. 'usage_planned' is updated so that it tracks
    // how much disk space would be used after evicting all the layers up to the current
    // point in the list.
    //
    // If we get far enough in the list that we start to evict layers that are below
    // the tenant's min-resident-size threshold, print a warning, and memorize the disk
    // usage at that point, in 'usage_planned_min_resident_size_respecting'.
    let mut warned = None;
    let mut usage_planned = usage_pre;
    let mut evicted_amount = 0;

    for (i, (partition, candidate)) in candidates.iter().enumerate() {
        if !usage_planned.has_pressure() {
            debug!(
                no_candidates_evicted = i,
                "took enough candidates for pressure to be relieved"
            );
            break;
        }

        if partition == &MinResidentSizePartition::Below && warned.is_none() {
            warn!(?usage_pre, ?usage_planned, candidate_no=i, "tenant_min_resident_size-respecting LRU would not relieve pressure, evicting more following global LRU policy");
            warned = Some(usage_planned);
        }

        usage_planned.add_available_bytes(candidate.layer.layer_desc().file_size);
        evicted_amount += 1;
    }

    let usage_planned = match warned {
        Some(respecting_tenant_min_resident_size) => PlannedUsage {
            respecting_tenant_min_resident_size,
            fallback_to_global_lru: Some(usage_planned),
        },
        None => PlannedUsage {
            respecting_tenant_min_resident_size: usage_planned,
            fallback_to_global_lru: None,
        },
    };
    debug!(?usage_planned, "usage planned");

    // phase2: evict layers

    let mut js = tokio::task::JoinSet::new();
    let limit = 1000;

    let mut evicted = candidates.into_iter().take(evicted_amount).fuse();
    let mut consumed_all = false;

    // After the evictions, `usage_assumed` is the post-eviction usage,
    // according to internal accounting.
    let mut usage_assumed = usage_pre;
    let mut evictions_failed = LayerCount::default();

    let evict_layers = async move {
        loop {
            let next = if js.len() >= limit || consumed_all {
                js.join_next().await
            } else if !js.is_empty() {
                // opportunistically consume ready result, one per each new evicted
                futures::future::FutureExt::now_or_never(js.join_next()).and_then(|x| x)
            } else {
                None
            };

            if let Some(next) = next {
                match next {
                    Ok(Ok(file_size)) => {
                        usage_assumed.add_available_bytes(file_size);
                    }
                    Ok(Err((
                        file_size,
                        Some(EvictionError::NotFound | EvictionError::Downloaded),
                    ))) => {
                        evictions_failed.file_sizes += file_size;
                        evictions_failed.count += 1;
                    }
                    Ok(Err((file_size, None))) => {
                        // count timeouted as failed evictions even if they might complete later
                        evictions_failed.file_sizes += file_size;
                        evictions_failed.count += 1;
                    }
                    Err(je) if je.is_cancelled() => unreachable!("not used"),
                    Err(je) if je.is_panic() => { /* already logged */ }
                    Err(je) => tracing::error!("unknown JoinError: {je:?}"),
                }
            }

            if consumed_all && js.is_empty() {
                break;
            }

            // calling again when consumed_all is fine as evicted is fused.
            let Some((_partition, candidate)) = evicted.next() else {
                if !consumed_all {
                    tracing::info!("all evictions started, waiting");
                    consumed_all = true;
                }
                continue;
            };

            js.spawn(async move {
                let rtc = candidate.timeline.remote_client.as_ref().expect(
                    "holding the witness, all timelines must have a remote timeline client",
                );
                let file_size = candidate.layer.layer_desc().file_size;

                let evict_and_wait = candidate.layer.evict_and_wait(rtc);

                // have a low eviction waiting timeout because our LRU calculations go stale fast;
                // also individual layer evictions could hang because of bugs and we do not want to
                // pause disk_usage_based_eviction for such.
                let timeout = std::time::Duration::from_secs(5);
                let evict_and_wait = tokio::time::timeout(timeout, evict_and_wait);

                match evict_and_wait.await {
                    Ok(Ok(())) => Ok(file_size),
                    Ok(Err(e)) => Err((file_size, Some(e))),
                    Err(_timeout) => Err((file_size, None)),
                }
            });

            tokio::task::yield_now().await;
        }

        (usage_assumed, evictions_failed)
    };

    let started_at = std::time::Instant::now();

    let evict_layers = async move {
        let mut evict_layers = std::pin::pin!(evict_layers);

        let maximum_expected = std::time::Duration::from_secs(10);
        let nag_every = std::time::Duration::from_secs(2);

        match tokio::time::timeout(maximum_expected, &mut evict_layers).await {
            Ok(tuple) => return tuple,
            Err(_timeout) => {
                tracing::info!(
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "still ongoing"
                );
            }
        };

        loop {
            let res = tokio::time::timeout(nag_every, &mut evict_layers).await;

            let elapsed_ms = started_at.elapsed().as_millis();

            match res {
                Ok(tuple) => {
                    tracing::info!(%elapsed_ms, "completed");
                    return tuple;
                }
                Err(_timeout) => {
                    tracing::info!(%elapsed_ms, "still ongoing");
                }
            }
        }
    };

    let evict_layers =
        evict_layers.instrument(tracing::info_span!("evict_layers", layers=%evicted_amount));

    let (usage_assumed, evictions_failed) = tokio::select! {
        tuple = evict_layers => { tuple },
        _ = cancel.cancelled() => {
            // dropping joinset will abort all pending evict_and_waits and that is fine, our
            // requests will still stand
            return Ok(IterationOutcome::Cancelled);
        }
    };

    Ok(IterationOutcome::Finished(IterationOutcomeFinished {
        before: usage_pre,
        planned: usage_planned,
        assumed: AssumedUsage {
            projected_after: usage_assumed,
            failed: evictions_failed,
        },
    }))
}

#[derive(Clone)]
struct EvictionCandidate {
    timeline: Arc<Timeline>,
    layer: Layer,
    last_activity_ts: SystemTime,
    relative_last_activity: finite_f32::FiniteF32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MinResidentSizePartition {
    Above,
    Below,
}

enum EvictionCandidates {
    Cancelled,
    Finished(Vec<(MinResidentSizePartition, EvictionCandidate)>),
}

/// Gather the eviction candidates.
///
/// The returned `Ok(EvictionCandidates::Finished(candidates))` is sorted in eviction
/// order. A caller that evicts in that order, until pressure is relieved, implements
/// the eviction policy outlined in the module comment.
///
/// # Example with EvictionOrder::AbsoluteAccessed
///
/// Imagine that there are two tenants, A and B, with five layers each, a-e.
/// Each layer has size 100, and both tenant's min_resident_size is 150.
/// The eviction order would be
///
/// ```text
/// partition last_activity_ts tenant/layer
/// Above     18:30            A/c
/// Above     19:00            A/b
/// Above     18:29            B/c
/// Above     19:05            B/b
/// Above     20:00            B/a
/// Above     20:03            A/a
/// Below     20:30            A/d
/// Below     20:40            B/d
/// Below     20:45            B/e
/// Below     20:58            A/e
/// ```
///
/// Now, if we need to evict 300 bytes to relieve pressure, we'd evict `A/c, A/b, B/c`.
/// They are all in the `Above` partition, so, we respected each tenant's min_resident_size.
///
/// But, if we need to evict 900 bytes to relieve pressure, we'd evict
/// `A/c, A/b, B/c, B/b, B/a, A/a, A/d, B/d, B/e`, reaching into the `Below` partition
/// after exhauting the `Above` partition.
/// So, we did not respect each tenant's min_resident_size.
///
/// # Example with EvictionOrder::RelativeAccessed
///
/// ```text
/// partition relative_age last_activity_ts tenant/layer
/// Above     0/4          18:30            A/c
/// Above     0/4          18:29            B/c
/// Above     1/4          19:00            A/b
/// Above     1/4          19:05            B/b
/// Above     2/4          20:00            B/a
/// Above     2/4          20:03            A/a
/// Below     3/4          20:30            A/d
/// Below     3/4          20:40            B/d
/// Below     4/4          20:45            B/e
/// Below     4/4          20:58            A/e
/// ```
///
/// With tenants having the same number of layers the picture does not change much. The same with
/// A having many more layers **resident** (not all of them listed):
///
/// ```text
/// Above       0/100      18:30            A/c
/// Above       0/4        18:29            B/c
/// Above       1/100      19:00            A/b
/// Above       2/100      20:03            A/a
/// Above       3/100      20:03            A/nth_3
/// Above       4/100      20:03            A/nth_4
///             ...
/// Above       1/4        19:05            B/b
/// Above      25/100      20:04            A/nth_25
///             ...
/// Above       2/4        20:00            B/a
/// Above      50/100      20:10            A/nth_50
///             ...
/// Below       3/4        20:40            B/d
/// Below      99/100      20:30            A/nth_99
/// Below       4/4        20:45            B/e
/// Below     100/100      20:58            A/nth_100
/// ```
///
/// Now it's easier to see that because A has grown fast it has more layers to get evicted. What is
/// difficult to see is what happens on the next round assuming the evicting 23 from the above list
/// relieves the pressure (22 A layers gone, 1 B layers gone) but a new fast growing tenant C has
/// appeared:
///
/// ```text
/// Above       0/87       20:04            A/nth_23
/// Above       0/3        19:05            B/b
/// Above       0/50       20:59            C/nth_0
/// Above       1/87       20:04            A/nth_24
/// Above       1/50       21:00            C/nth_1
/// Above       2/87       20:04            A/nth_25
///             ...
/// Above      16/50       21:02            C/nth_16
/// Above       1/3        20:00            B/a
/// Above      27/87       20:10            A/nth_50
///             ...
/// Below       2/3        20:40            B/d
/// Below      49/50       21:05            C/nth_49
/// Below      86/87       20:30            A/nth_99
/// Below       3/3        20:45            B/e
/// Below      50/50       21:05            C/nth_50
/// Below      87/87       20:58            A/nth_100
/// ```
///
/// Now relieving pressure with 23 layers would cost:
/// - tenant A 14 layers
/// - tenant B 1 layer
/// - tenant C 8 layers
async fn collect_eviction_candidates(
    eviction_order: EvictionOrder,
    cancel: &CancellationToken,
) -> anyhow::Result<EvictionCandidates> {
    // get a snapshot of the list of tenants
    let tenants = tenant::mgr::list_tenants()
        .await
        .context("get list of tenants")?;

    let mut candidates = Vec::new();

    for (tenant_id, _state) in &tenants {
        if cancel.is_cancelled() {
            return Ok(EvictionCandidates::Cancelled);
        }
        let tenant = match tenant::mgr::get_tenant(*tenant_id, true) {
            Ok(tenant) => tenant,
            Err(e) => {
                // this can happen if tenant has lifecycle transition after we fetched it
                debug!("failed to get tenant: {e:#}");
                continue;
            }
        };

        if tenant.cancel.is_cancelled() {
            info!(%tenant_id, "Skipping tenant for eviction, it is shutting down");
            continue;
        }

        // collect layers from all timelines in this tenant
        //
        // If one of the timelines becomes `!is_active()` during the iteration,
        // for example because we're shutting down, then `max_layer_size` can be too small.
        // That's OK. This code only runs under a disk pressure situation, and being
        // a little unfair to tenants during shutdown in such a situation is tolerable.
        let mut tenant_candidates = Vec::new();
        let mut max_layer_size = 0;
        for tl in tenant.list_timelines() {
            if !tl.is_active() {
                continue;
            }
            let info = tl.get_local_layers_for_disk_usage_eviction().await;
            debug!(tenant_id=%tl.tenant_shard_id.tenant_id, shard_id=%tl.tenant_shard_id.shard_slug(), timeline_id=%tl.timeline_id, "timeline resident layers count: {}", info.resident_layers.len());
            tenant_candidates.extend(
                info.resident_layers
                    .into_iter()
                    .map(|layer_infos| (tl.clone(), layer_infos)),
            );
            max_layer_size = max_layer_size.max(info.max_layer_size.unwrap_or(0));

            if cancel.is_cancelled() {
                return Ok(EvictionCandidates::Cancelled);
            }
        }

        // `min_resident_size` defaults to maximum layer file size of the tenant.
        // This ensures that each tenant can have at least one layer resident at a given time,
        // ensuring forward progress for a single Timeline::get in that tenant.
        // It's a questionable heuristic since, usually, there are many Timeline::get
        // requests going on for a tenant, and, at least in Neon prod, the median
        // layer file size is much smaller than the compaction target size.
        // We could be better here, e.g., sum of all L0 layers + most recent L1 layer.
        // That's what's typically used by the various background loops.
        //
        // The default can be overridden with a fixed value in the tenant conf.
        // A default override can be put in the default tenant conf in the pageserver.toml.
        let min_resident_size = if let Some(s) = tenant.get_min_resident_size_override() {
            debug!(
                tenant_id=%tenant.tenant_id(),
                overridden_size=s,
                "using overridden min resident size for tenant"
            );
            s
        } else {
            debug!(
                tenant_id=%tenant.tenant_id(),
                max_layer_size,
                "using max layer size as min_resident_size for tenant",
            );
            max_layer_size
        };

        // Sort layers most-recently-used first, then partition by
        // cumsum above/below min_resident_size.
        tenant_candidates
            .sort_unstable_by_key(|(_, layer_info)| std::cmp::Reverse(layer_info.last_activity_ts));
        let mut cumsum: i128 = 0;

        // keeping the -1 or not decides if every tenant should lose their least recently accessed
        // layer OR if this should happen in the order of having highest layer count:
        let fudge = if eviction_order.highest_layer_count_loses_first() {
            // relative_age vs. tenant layer count:
            // - 0.1..=1.0 (10 layers)
            // - 0.01..=1.0 (100 layers)
            // - 0.001..=1.0 (1000 layers)
            //
            // leading to evicting less of the smallest tenants.
            0
        } else {
            // use full 0.0..=1.0 range, which means even the smallest tenants could always lose a
            // layer. the actual ordering is unspecified: for 10k tenants on a pageserver it could
            // be that less than 10k layer evictions is enough, so we would not need to evict from
            // all tenants.
            //
            // as the tenant ordering is now deterministic this could hit the same tenants
            // disproportionetly on multiple invocations. alternative could be to remember how many
            // layers did we evict last time from this tenant, and inject that as an additional
            // fudge here.
            1
        };

        let total = tenant_candidates
            .len()
            .checked_sub(fudge)
            .filter(|&x| x > 0)
            // support 0 or 1 resident layer tenants as well
            .unwrap_or(1);
        let divider = total as f32;

        for (i, (timeline, layer_info)) in tenant_candidates.into_iter().enumerate() {
            let file_size = layer_info.file_size();

            // as we iterate this reverse sorted list, the most recently accessed layer will always
            // be 1.0; this is for us to evict it last.
            let relative_last_activity = if matches!(
                eviction_order,
                EvictionOrder::RelativeAccessed { .. }
            ) {
                // another possibility: use buckets, like (256.0 * relative_last_activity) as u8 or
                // similarly for u16. unsure how it would help.
                finite_f32::FiniteF32::try_from_normalized((total - i) as f32 / divider)
                    .unwrap_or_else(|val| {
                        tracing::warn!(%fudge, "calculated invalid relative_last_activity for i={i}, total={total}: {val}");
                        finite_f32::FiniteF32::ZERO
                    })
            } else {
                finite_f32::FiniteF32::ZERO
            };

            let candidate = EvictionCandidate {
                timeline,
                last_activity_ts: layer_info.last_activity_ts,
                layer: layer_info.layer,
                relative_last_activity,
            };
            let partition = if cumsum > min_resident_size as i128 {
                MinResidentSizePartition::Above
            } else {
                MinResidentSizePartition::Below
            };
            candidates.push((partition, candidate));
            cumsum += i128::from(file_size);
        }
    }

    debug_assert!(MinResidentSizePartition::Above < MinResidentSizePartition::Below,
        "as explained in the function's doc comment, layers that aren't in the tenant's min_resident_size are evicted first");

    match eviction_order {
        EvictionOrder::AbsoluteAccessed => {
            candidates.sort_unstable_by_key(|(partition, candidate)| {
                (*partition, candidate.last_activity_ts)
            });
        }
        EvictionOrder::RelativeAccessed { .. } => {
            candidates.sort_unstable_by_key(|(partition, candidate)| {
                (*partition, candidate.relative_last_activity)
            });
        }
    }

    Ok(EvictionCandidates::Finished(candidates))
}

struct TimelineKey(Arc<Timeline>);

impl PartialEq for TimelineKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TimelineKey {}

impl std::hash::Hash for TimelineKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl std::ops::Deref for TimelineKey {
    type Target = Timeline;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

/// A totally ordered f32 subset we can use with sorting functions.
mod finite_f32 {

    /// A totally ordered f32 subset we can use with sorting functions.
    #[derive(Clone, Copy, PartialEq)]
    pub struct FiniteF32(f32);

    impl std::fmt::Debug for FiniteF32 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.0, f)
        }
    }

    impl std::fmt::Display for FiniteF32 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&self.0, f)
        }
    }

    impl std::cmp::Eq for FiniteF32 {}

    impl std::cmp::PartialOrd for FiniteF32 {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl std::cmp::Ord for FiniteF32 {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.0.total_cmp(&other.0)
        }
    }

    impl TryFrom<f32> for FiniteF32 {
        type Error = f32;

        fn try_from(value: f32) -> Result<Self, Self::Error> {
            if value.is_finite() {
                Ok(FiniteF32(value))
            } else {
                Err(value)
            }
        }
    }

    impl FiniteF32 {
        pub const ZERO: FiniteF32 = FiniteF32(0.0);

        pub fn try_from_normalized(value: f32) -> Result<Self, f32> {
            if (0.0..=1.0).contains(&value) {
                // -0.0 is within the range, make sure it is assumed 0.0..=1.0
                let value = value.abs();
                Ok(FiniteF32(value))
            } else {
                Err(value)
            }
        }
    }
}

mod filesystem_level_usage {
    use anyhow::Context;
    use camino::Utf8Path;

    use crate::statvfs::Statvfs;

    use super::DiskUsageEvictionTaskConfig;

    #[derive(Debug, Clone, Copy)]
    #[allow(dead_code)]
    pub struct Usage<'a> {
        config: &'a DiskUsageEvictionTaskConfig,

        /// Filesystem capacity
        total_bytes: u64,
        /// Free filesystem space
        avail_bytes: u64,
    }

    impl super::Usage for Usage<'_> {
        fn has_pressure(&self) -> bool {
            let usage_pct =
                (100.0 * (1.0 - ((self.avail_bytes as f64) / (self.total_bytes as f64)))) as u64;

            let pressures = [
                (
                    "min_avail_bytes",
                    self.avail_bytes < self.config.min_avail_bytes,
                ),
                (
                    "max_usage_pct",
                    usage_pct >= self.config.max_usage_pct.get() as u64,
                ),
            ];

            pressures.into_iter().any(|(_, has_pressure)| has_pressure)
        }

        fn add_available_bytes(&mut self, bytes: u64) {
            self.avail_bytes += bytes;
        }
    }

    pub fn get<'a>(
        tenants_dir: &Utf8Path,
        config: &'a DiskUsageEvictionTaskConfig,
    ) -> anyhow::Result<Usage<'a>> {
        let mock_config = {
            #[cfg(feature = "testing")]
            {
                config.mock_statvfs.as_ref()
            }
            #[cfg(not(feature = "testing"))]
            {
                None
            }
        };

        let stat = Statvfs::get(tenants_dir, mock_config)
            .context("statvfs failed, presumably directory got unlinked")?;

        // https://unix.stackexchange.com/a/703650
        let blocksize = if stat.fragment_size() > 0 {
            stat.fragment_size()
        } else {
            stat.block_size()
        };

        // use blocks_available (b_avail) since, pageserver runs as unprivileged user
        let avail_bytes = stat.blocks_available() * blocksize;
        let total_bytes = stat.blocks() * blocksize;

        Ok(Usage {
            config,
            total_bytes,
            avail_bytes,
        })
    }

    #[test]
    fn max_usage_pct_pressure() {
        use super::EvictionOrder;
        use super::Usage as _;
        use std::time::Duration;
        use utils::serde_percent::Percent;

        let mut usage = Usage {
            config: &DiskUsageEvictionTaskConfig {
                max_usage_pct: Percent::new(85).unwrap(),
                min_avail_bytes: 0,
                period: Duration::MAX,
                #[cfg(feature = "testing")]
                mock_statvfs: None,
                eviction_order: EvictionOrder::default(),
            },
            total_bytes: 100_000,
            avail_bytes: 0,
        };

        assert!(usage.has_pressure(), "expected pressure at 100%");

        usage.add_available_bytes(14_000);
        assert!(usage.has_pressure(), "expected pressure at 86%");

        usage.add_available_bytes(999);
        assert!(usage.has_pressure(), "expected pressure at 85.001%");

        usage.add_available_bytes(1);
        assert!(usage.has_pressure(), "expected pressure at precisely 85%");

        usage.add_available_bytes(1);
        assert!(!usage.has_pressure(), "no pressure at 84.999%");

        usage.add_available_bytes(999);
        assert!(!usage.has_pressure(), "no pressure at 84%");

        usage.add_available_bytes(16_000);
        assert!(!usage.has_pressure());
    }
}
