//! API discovery on a `:` lookup miss. Discovery otherwise runs only on
//! connect, so a CRD installed while sofka runs would stay unknown until a
//! restart or a context switch.

use super::*;

/// Wait at least this long between miss-triggered discovery runs.
pub(super) const REDISCOVERY_THROTTLE: Duration = Duration::from_secs(10);

/// The lookup to run again once discovery finishes.
pub(super) enum Retry {
    Kind { input: String, ns: Option<String> },
    Query(crate::filter::ResourceQuery),
}

impl Retry {
    fn resource(&self) -> &str {
        match self {
            Retry::Kind { input, .. } => input.trim(),
            Retry::Query(query) => &query.resource,
        }
    }
}

impl App {
    /// Start API discovery for a lookup that matched no known kind. Returns
    /// `false` when throttled, or while a context switch is replacing the
    /// cluster, so the caller reports the miss at once.
    pub(super) fn rediscover_on_miss(&mut self, retry: Retry) -> bool {
        if !self.cluster.connected
            || self.context_switch_target.is_some()
            || self
                .last_rediscovery
                .is_some_and(|at| at.elapsed() < REDISCOVERY_THROTTLE)
        {
            return false;
        }
        let resource = retry.resource().to_string();
        self.last_rediscovery = Some(std::time::Instant::now());
        self.rediscovery_request += 1;
        self.pending_rediscovery = Some((self.generation, retry));
        self.set_flash(format!("looking up '{resource}' in API discovery…"));
        crate::log_info!(
            "cluster.rediscover",
            context = self.cluster.context,
            resource = resource
        );
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let epoch = self.context_epoch;
        let request = self.rediscovery_request;
        tokio::spawn(async move {
            let result = crate::k8s::rediscover(&client)
                .await
                .map(Box::new)
                .map_err(|e| format!("{e:#}"));
            let _ = tx
                .send(Msg::Rediscovered {
                    epoch,
                    request,
                    result,
                })
                .await;
        });
        true
    }

    /// Install the kinds a miss-triggered discovery found, then run the
    /// waiting lookup again if the user is still where they made it.
    pub(super) fn apply_rediscovery(&mut self, result: Result<Box<crate::k8s::Discovery>, String>) {
        let pending = self.pending_rediscovery.take();
        match result {
            Ok(discovery) => {
                self.cluster.apply_discovery(*discovery);
                self.cluster.add_aliases(&self.user_aliases);
                crate::log_info!(
                    "cluster.rediscovered",
                    context = self.cluster.context,
                    kinds = self.cluster.catalog.len()
                );
                // A review that fails or comes back incomplete sends nothing,
                // so drop the old allow-list rather than let it hide new kinds.
                self.rbac_allowed = None;
                self.refresh_rbac();
                if self.mode == Mode::Command {
                    self.update_suggestions();
                }
            }
            Err(e) => crate::log_error!(
                "cluster.rediscover.failed",
                context = self.cluster.context,
                error = e
            ),
        }
        // Restart the throttle so a retry that still misses reports the miss
        // instead of starting another discovery.
        self.last_rediscovery = Some(std::time::Instant::now());
        let Some((generation, retry)) = pending else {
            return;
        };
        if generation != self.generation || self.mode != Mode::Table {
            return;
        }
        let resource = retry.resource().to_string();
        match retry {
            Retry::Kind { input, ns } => self.switch_kind_ns(&input, ns.as_deref()),
            Retry::Query(query) => self.apply_resource_query(query),
        }
        // The kind may live in an API group discovery could not read; say so
        // next to the miss rather than leave it unexplained.
        if self.cluster.resolve(&resource).is_none()
            && let Some(warning) = self.discovery_warning()
        {
            self.flash_warn(&format!("{}; {warning}", self.flash));
        }
    }
}
