//! Optional external provider integrations.
//!
//! Providers are small, config-driven interfaces to observability backends
//! that launch from the currently selected Kubernetes object. The core stays
//! fully usable without any provider configured.
//!
//! The only provider kind is a Prometheus-compatible metrics backend for
//! `:rightsize`. With no `[providers.metrics]` section, sofka discovers a
//! query `Service` in the cluster by its well-known labels and reaches it
//! through the Kubernetes API server's service proxy, because the ClusterIP
//! is not routable from a workstation and the proxy reuses the session's
//! authentication.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use k8s_openapi::api::core::v1::Service;
use kube::api::{Api, ListParams};

/// A query that runs longer than this is almost certainly stuck.
const QUERY_TIMEOUT_SECS: u64 = 30;

/// How the backend is reached.
#[derive(Clone)]
enum Transport {
    /// Straight HTTP(S) to a configured base URL (no trailing slash) — an
    /// ingress, a load balancer, or a local port-forward.
    Direct { url: String },
    /// Through the Kubernetes API server's service proxy
    /// (`/api/v1/namespaces/<ns>/services/<name>:<port>/proxy`), using the
    /// session's already-authenticated client. How discovered in-cluster
    /// services are reached: their ClusterIP isn't routable from a laptop.
    ServiceProxy {
        client: kube::Client,
        ns: String,
        service: String,
        port: i32,
    },
    /// Not yet known — [`discover_metrics`] resolves it to [`Transport::ServiceProxy`]
    /// on first use.
    Auto,
}

/// `"90s"` / `"15m"` / `"1h"` / `"2d"` (bare numbers are seconds).
pub(crate) fn parse_lookback(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like \"30m\" or \"1h\""))?;
    let per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("{s:?} has unknown unit {unit:?} (use s/m/h/d)")),
    };
    let secs = n
        .checked_mul(per_unit)
        .ok_or_else(|| format!("{s:?} is too large"))?;
    if secs <= 0 {
        return Err(format!("{s:?} must be a positive duration"));
    }
    Ok(secs)
}

type DirectHttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// The hyper client for direct-transport requests, built once per process.
/// `with_native_roots()` reads and parses the system root store from disk —
/// per request that dwarfed the request itself — and one client also pools
/// connections across the burst of queries right-sizing sends.
fn direct_client() -> Result<DirectHttpClient, String> {
    static CLIENT: std::sync::OnceLock<Result<DirectHttpClient, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let https = hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .map_err(|e| format!("loading system TLS roots: {e}"))?
                .https_or_http()
                .enable_http1()
                .build();
            Ok(
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build(https),
            )
        })
        .clone()
}

/// Form-urlencode `params`. Scoped helper: the serializer holds a non-`Send`
/// encoder, so it must never be held across an await.
fn form_body(params: &[(&str, &str)]) -> String {
    let mut form = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        form.append_pair(k, v);
    }
    form.finish()
}

fn http_error(status: http::StatusCode, body: &[u8]) -> String {
    let detail: String = String::from_utf8_lossy(body)
        .trim()
        .chars()
        .take(200)
        .collect();
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

/// Label selectors that identify a Prometheus-compatible query service across
/// common installs: the VictoriaMetrics k8s-stack / operator (`vmsingle`), the
/// single-node Helm chart, and kube-prometheus-stack Prometheus.
const METRICS_DISCOVERY_SELECTORS: &[&str] = &[
    "app.kubernetes.io/name=vmsingle",
    "app.kubernetes.io/name=victoria-metrics-single",
    "app.kubernetes.io/name=prometheus",
    "operated-prometheus=true",
];

pub const DEFAULT_METRICS_WINDOW: &str = "7d";
const DEFAULT_METRICS_STEP: &str = "5m";
const DEFAULT_HEADROOM: u32 = 15;

/// A compiled Prometheus-compatible metrics backend for `:rightsize`. Reaches
/// the query API at `/api/v1/query` over a direct URL or the API-server
/// service proxy.
#[derive(Clone)]
pub struct MetricsProvider {
    transport: Transport,
    headers: Vec<(String, String)>,
    /// Quantile lookback (`"7d"`), verbatim for the PromQL and titles.
    pub window: String,
    /// Subquery resolution for the CPU `rate()` (`"5m"`).
    pub step: String,
    /// Percent headroom added over P95 for the suggested request.
    pub headroom: u32,
}

impl Default for MetricsProvider {
    fn default() -> Self {
        Self {
            transport: Transport::Auto,
            headers: Vec::new(),
            window: DEFAULT_METRICS_WINDOW.into(),
            step: DEFAULT_METRICS_STEP.into(),
            headroom: DEFAULT_HEADROOM,
        }
    }
}

/// Validate `[providers.metrics]` into a [`MetricsProvider`]. An omitted/empty
/// url means "autodiscover in-cluster" (resolved on first use). Accepts
/// `prometheus` and `victoriametrics` (the same query API).
pub fn compile_metrics(
    cfg: Option<&crate::config::MetricsProviderConfig>,
) -> (Option<MetricsProvider>, Vec<String>) {
    let Some(cfg) = cfg else {
        return (None, Vec::new());
    };
    let mut warnings = Vec::new();
    match cfg.kind.as_str() {
        "prometheus" | "victoriametrics" => {}
        "" => {
            warnings.push(
                "providers.metrics: missing `type` (expected \"prometheus\" or \"victoriametrics\")"
                    .into(),
            );
            return (None, warnings);
        }
        other => {
            warnings.push(format!(
                "providers.metrics: unsupported type {other:?} (expected \"prometheus\"/\"victoriametrics\")"
            ));
            return (None, warnings);
        }
    }

    let url = cfg.url.trim().trim_end_matches('/').to_string();
    let transport = if url.is_empty() {
        Transport::Auto
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Transport::Direct { url }
    } else {
        warnings.push(format!(
            "providers.metrics: url {:?} must start with http:// or https:// (or be omitted for autodiscovery)",
            cfg.url
        ));
        return (None, warnings);
    };

    let window = cfg
        .window
        .clone()
        .unwrap_or_else(|| DEFAULT_METRICS_WINDOW.into());
    if let Err(e) = parse_lookback(&window) {
        warnings.push(format!("providers.metrics: window: {e}"));
        return (None, warnings);
    }
    let step = cfg
        .step
        .clone()
        .unwrap_or_else(|| DEFAULT_METRICS_STEP.into());
    if let Err(e) = parse_lookback(&step) {
        warnings.push(format!("providers.metrics: step: {e}"));
        return (None, warnings);
    }

    (
        Some(MetricsProvider {
            transport,
            headers: cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            window,
            step,
            headroom: cfg.headroom.unwrap_or(DEFAULT_HEADROOM),
        }),
        warnings,
    )
}

/// Find a Prometheus/VictoriaMetrics query `Service` and resolve `base`'s
/// transport to the API-server proxy for it.
pub async fn discover_metrics(
    client: kube::Client,
    base: &MetricsProvider,
) -> Result<MetricsProvider, String> {
    let api: Api<Service> = Api::all(client.clone());
    for selector in METRICS_DISCOVERY_SELECTORS {
        let list = api
            .list(&ListParams::default().labels(selector))
            .await
            .map_err(|e| format!("discovering metrics services: {e}"))?
            .items;
        if let Some((ns, service, port)) = pick_metrics_service(&list) {
            let mut provider = base.clone();
            provider.transport = Transport::ServiceProxy {
                client,
                ns,
                service,
                port,
            };
            return Ok(provider);
        }
    }
    Err(
        "no Prometheus/VictoriaMetrics service found — set [providers.metrics] url in config.toml"
            .into(),
    )
}

/// First (by namespace/name) service with a usable port, preferring `http`,
/// then the well-known query ports (Prometheus 9090, VM single 8428/8429).
fn pick_metrics_service(services: &[Service]) -> Option<(String, String, i32)> {
    // As above, select in one pass without materializing every candidate.
    let (svc, port) = services
        .iter()
        .filter_map(|s| {
            let ports = s.spec.as_ref()?.ports.as_ref()?;
            let port = ports
                .iter()
                .find(|p| p.name.as_deref() == Some("http"))
                .or_else(|| ports.iter().find(|p| matches!(p.port, 9090 | 8428 | 8429)))
                .or_else(|| ports.first())?;
            Some((s, port.port))
        })
        .min_by_key(|(s, _)| {
            (
                s.metadata.namespace.as_deref().unwrap_or_default(),
                s.metadata.name.as_deref().unwrap_or_default(),
            )
        })?;
    Some((
        svc.metadata.namespace.clone().unwrap_or_default(),
        svc.metadata.name.clone().unwrap_or_default(),
        port,
    ))
}

#[cfg(feature = "bench")]
pub fn bench_pick_metrics_service(services: &[Service]) -> Option<(String, String, i32)> {
    pick_metrics_service(services)
}

impl MetricsProvider {
    /// Whether the transport still needs [`discover_metrics`].
    pub fn needs_discovery(&self) -> bool {
        matches!(self.transport, Transport::Auto)
    }

    /// A human-readable description of where queries go, for messages.
    pub fn location(&self) -> String {
        match &self.transport {
            Transport::Direct { url } => url.clone(),
            Transport::ServiceProxy {
                ns, service, port, ..
            } => format!("{ns}/{service}:{port} (API-server proxy)"),
            Transport::Auto => "autodiscover".into(),
        }
    }

    /// Run one instant query, returning the first sample value (`None` = no
    /// data). Bounded by [`QUERY_TIMEOUT_SECS`].
    pub async fn query(&self, promql: &str) -> Result<Option<f64>, String> {
        let fut = self.fetch_query(promql);
        let body = tokio::time::timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| format!("query timed out after {QUERY_TIMEOUT_SECS}s"))??;
        Ok(crate::rightsize::scalar_from_query(&body))
    }

    /// POST `query=<promql>` to `/api/v1/query` over the active transport.
    async fn fetch_query(&self, promql: &str) -> Result<String, String> {
        let params = [("query", promql)];
        match &self.transport {
            Transport::ServiceProxy {
                client,
                ns,
                service,
                port,
            } => {
                let uri =
                    format!("/api/v1/namespaces/{ns}/services/{service}:{port}/proxy/api/v1/query");
                let mut req = http::Request::post(uri).header(
                    http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                );
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                let req = req
                    .body(form_body(&params).into_bytes())
                    .map_err(|e| format!("building request: {e}"))?;
                client
                    .request_text(req)
                    .await
                    .map_err(|e| format!("{}: {e}", self.location()))
            }
            Transport::Direct { url } => {
                let http_client = direct_client()?;
                let full = format!("{url}/api/v1/query");
                let mut req = hyper::Request::post(&full).header(
                    http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                );
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                let req = req
                    .body(Full::new(Bytes::from(form_body(&params))))
                    .map_err(|e| format!("building request: {e}"))?;
                let resp = http_client
                    .request(req)
                    .await
                    .map_err(|e| format!("{full}: {e}"))?;
                let status = resp.status();
                let body = resp
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| format!("reading response: {e}"))?
                    .to_bytes();
                if !status.is_success() {
                    return Err(http_error(status, &body));
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            }
            Transport::Auto => Err("metrics provider not discovered yet".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(ns: &str, name: &str, ports: serde_json::Value) -> Service {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "namespace": ns},
            "spec": {"ports": ports}
        }))
        .unwrap()
    }

    #[test]
    fn pick_metrics_service_is_deterministic_and_prefers_query_ports() {
        let ordinary = svc("z-monitoring", "prom", serde_json::json!([{"port": 8080}]));
        let query = svc(
            "a-monitoring",
            "victoria-metrics",
            serde_json::json!([{"port": 8080}, {"port": 8428}]),
        );
        assert_eq!(
            pick_metrics_service(&[ordinary, query]),
            Some(("a-monitoring".into(), "victoria-metrics".into(), 8428))
        );

        let named = svc(
            "monitoring",
            "prometheus",
            serde_json::json!([
                {"port": 9090},
                {"name": "http", "port": 8081}
            ]),
        );
        assert_eq!(
            pick_metrics_service(&[named]),
            Some(("monitoring".into(), "prometheus".into(), 8081))
        );
    }

    #[test]
    fn parse_lookback_units() {
        assert_eq!(parse_lookback("90s"), Ok(90));
        assert_eq!(parse_lookback("15m"), Ok(900));
        assert_eq!(parse_lookback("2h"), Ok(7200));
        assert_eq!(parse_lookback("1d"), Ok(86_400));
        assert_eq!(parse_lookback("45"), Ok(45)); // bare seconds
        assert!(parse_lookback("0m").is_err());
        assert!(parse_lookback("1w").is_err());
        assert!(parse_lookback("h").is_err());
    }
}
