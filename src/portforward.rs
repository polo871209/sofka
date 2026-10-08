//! Port-forwarding through the Kubernetes API, without `kubectl`.
//!
//! A forward binds its local port when it starts, so a port conflict fails
//! at once. Resolving the target and every connection runs in a background
//! task. Targets follow `kubectl port-forward`: the pod is chosen once at
//! start, and the forward ends when that pod can no longer be reached.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams};
use kube::core::{ApiResource, GroupVersionKind, Selector};
use serde_json::Value;

use crate::json::Pointer as _;
use tokio::net::{TcpListener, TcpStream};

/// Why a forward stopped. `None` while it runs.
pub type Exit = Arc<Mutex<Option<String>>>;

/// A started forward: its task, its exit reason, and the bound local port.
pub struct Started {
    pub task: tokio::task::JoinHandle<()>,
    pub exit: Exit,
    pub local: u16,
}

/// Parse `LOCAL:REMOTE`, `REMOTE`, or `:REMOTE`. `REMOTE` is a number or a
/// port name. `LOCAL` 0 or empty picks a free port.
pub fn parse_ports(ports: &str) -> Result<(u16, String)> {
    let (local, remote) = match ports.trim().split_once(':') {
        Some((local, remote)) => (local, remote),
        None => (ports.trim(), ports.trim()),
    };
    if remote.is_empty() {
        bail!("no remote port in '{ports}'");
    }
    let local = if local.is_empty() {
        0
    } else {
        local
            .parse()
            .with_context(|| format!("local port '{local}' is not a number"))?
    };
    Ok((local, remote.to_string()))
}

/// Bind the local listeners and start forwarding to `target` in `ns`.
/// Must be called inside a Tokio runtime.
pub fn start(client: Client, ns: &str, target: &str, ports: &str) -> std::io::Result<Started> {
    let invalid =
        |e: anyhow::Error| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string());
    let (local, remote) = parse_ports(ports).map_err(invalid)?;
    let (listeners, local) = bind_localhost(local)?;
    let exit: Exit = Arc::default();
    let reason = exit.clone();
    let (ns, target) = (ns.to_string(), target.to_string());
    let task = tokio::spawn(async move {
        let error = match serve(client, &ns, &target, &remote, listeners).await {
            Ok(()) => "stopped".to_string(),
            Err(e) => format!("{e:#}"),
        };
        *reason.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
    });
    Ok(Started { task, exit, local })
}

/// Bind `port` on 127.0.0.1 and ::1, as `kubectl` does for `localhost`. A
/// host without IPv6 keeps the IPv4 listener.
fn bind_localhost(port: u16) -> std::io::Result<(Vec<TcpListener>, u16)> {
    let v4 = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    match std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)) {
        Ok(v6) => listeners.push(v6),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(e),
        Err(_) => {}
    }
    let listeners = listeners
        .into_iter()
        .map(|l| {
            l.set_nonblocking(true)?;
            TcpListener::from_std(l)
        })
        .collect::<std::io::Result<_>>()?;
    Ok((listeners, port))
}

async fn serve(
    client: Client,
    ns: &str,
    target: &str,
    remote: &str,
    listeners: Vec<TcpListener>,
) -> Result<()> {
    let (pod, port) = resolve(&client, ns, target, remote).await?;
    let pods: Api<Pod> = Api::namespaced(client, ns);
    let (closed_tx, mut closed) = tokio::sync::mpsc::channel::<String>(1);
    let mut accepts = tokio::task::JoinSet::new();
    for listener in listeners {
        let (pods, pod, closed_tx) = (pods.clone(), pod.clone(), closed_tx.clone());
        accepts.spawn(async move {
            loop {
                let (conn, _) = listener.accept().await?;
                let (pods, pod, closed_tx) = (pods.clone(), pod.clone(), closed_tx.clone());
                tokio::spawn(async move {
                    if let Err(e) = forward(&pods, &pod, port, conn).await
                        && let Some(reason) = fatal(&e, &pod)
                    {
                        let _ = closed_tx.try_send(reason);
                    }
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        });
    }
    drop(closed_tx);
    tokio::select! {
        reason = pod_stopped(pods.clone(), pod.clone()) => Err(anyhow!(reason)),
        Some(reason) = closed.recv() => Err(anyhow!(reason)),
        Some(Ok(Err(e))) = accepts.join_next() => Err(e).context("accepting local connections"),
    }
}

/// Resolve once the pod is deleted or finished, like `kubectl` losing its
/// connection. Watch errors back off and never end the forward.
async fn pod_stopped(pods: Api<Pod>, pod: String) -> String {
    use futures_util::StreamExt;
    use kube::runtime::WatchStreamExt;
    let events = kube::runtime::watcher::watch_object(pods, &pod).default_backoff();
    let mut events = std::pin::pin!(events);
    while let Some(event) = events.next().await {
        match event {
            Ok(None) => return format!("pod {pod} no longer exists"),
            Ok(Some(p))
                if p.metadata.deletion_timestamp.is_some()
                    || matches!(
                        p.status.as_ref().and_then(|s| s.phase.as_deref()),
                        Some("Succeeded" | "Failed")
                    ) =>
            {
                return format!("pod {pod} stopped");
            }
            _ => {}
        }
    }
    std::future::pending().await
}

/// Copy one local connection through its own API stream.
async fn forward(pods: &Api<Pod>, pod: &str, port: u16, mut conn: TcpStream) -> Result<()> {
    let _ = conn.set_nodelay(true);
    let mut forwarder = pods.portforward(pod, &[port]).await?;
    let mut upstream = forwarder
        .take_stream(port)
        .ok_or_else(|| anyhow!("no stream for port {port}"))?;
    let error = forwarder.take_error(port);
    tokio::io::copy_bidirectional(&mut conn, &mut upstream)
        .await
        .ok();
    drop(upstream);
    if let Some(error) = error
        && let Some(message) = error.await
    {
        bail!("{message}");
    }
    forwarder.join().await?;
    Ok(())
}

/// The reason to end the forward, if `error` means the pod is unreachable.
fn fatal(error: &anyhow::Error, pod: &str) -> Option<String> {
    let api_status = error
        .chain()
        .find_map(|e| match e.downcast_ref::<kube::Error>() {
            Some(kube::Error::Api(status)) => Some(status.code),
            Some(kube::Error::UpgradeConnection(
                kube::client::UpgradeConnectionError::ProtocolSwitch(code),
            )) => Some(code.as_u16()),
            _ => None,
        });
    match api_status {
        Some(404) => Some(format!("pod {pod} no longer exists")),
        Some(code @ (400 | 403)) => {
            Some(format!("pod {pod} refused the forward ({code}): {error:#}"))
        }
        _ => None,
    }
}

/// The pod and container port behind `target`, following `kubectl`.
pub async fn resolve(
    client: &Client,
    ns: &str,
    target: &str,
    remote: &str,
) -> Result<(String, u16)> {
    let (kind, name) = target.split_once('/').unwrap_or(("pod", target));
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    match kind.to_ascii_lowercase().as_str() {
        "pod" | "pods" | "po" => {
            let pod = pods.get(name).await?;
            let port = container_port(&pod, remote)?;
            Ok((name.to_string(), port))
        }
        "svc" | "service" | "services" => {
            let svc: Service = Api::namespaced(client.clone(), ns).get(name).await?;
            let spec = svc
                .spec
                .as_ref()
                .ok_or_else(|| anyhow!("service {name} has no spec"))?;
            let selector = spec
                .selector
                .as_ref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("service {name} has no pod selector"))?;
            let selector = selector
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            let pod = pick_pod(&pods, &selector)
                .await?
                .ok_or_else(|| anyhow!("no running pod for service {name}"))?;
            let svc_port = spec
                .ports
                .iter()
                .flatten()
                .find(|p| remote.parse() == Ok(p.port) || p.name.as_deref() == Some(remote))
                .ok_or_else(|| anyhow!("service {name} has no port {remote}"))?;
            let port = match &svc_port.target_port {
                Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(p)) => {
                    u16::try_from(*p).context("target port out of range")?
                }
                Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(named)) => {
                    container_port(&pod, named)?
                }
                None => u16::try_from(svc_port.port).context("service port out of range")?,
            };
            Ok((pod.metadata.name.clone().unwrap_or_default(), port))
        }
        other => {
            let gvk = match other {
                "deploy" | "deployment" | "deployments" => {
                    GroupVersionKind::gvk("apps", "v1", "Deployment")
                }
                "rs" | "replicaset" | "replicasets" => {
                    GroupVersionKind::gvk("apps", "v1", "ReplicaSet")
                }
                "sts" | "statefulset" | "statefulsets" => {
                    GroupVersionKind::gvk("apps", "v1", "StatefulSet")
                }
                "ds" | "daemonset" | "daemonsets" => {
                    GroupVersionKind::gvk("apps", "v1", "DaemonSet")
                }
                "job" | "jobs" => GroupVersionKind::gvk("batch", "v1", "Job"),
                _ => bail!(
                    "cannot port-forward to {kind}; use pod/, svc/, deploy/, rs/, sts/, ds/ or job/"
                ),
            };
            let ar = ApiResource::from_gvk(&gvk);
            let obj: DynamicObject = Api::namespaced_with(client.clone(), ns, &ar)
                .get(name)
                .await?;
            let selector = obj
                .data
                .at("/spec/selector")
                .cloned()
                .and_then(|v| {
                    serde_json::from_value::<
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
                    >(v)
                    .ok()
                })
                .and_then(|s| Selector::try_from(s).ok())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("{target} has no pod selector"))?;
            let pod = pick_pod(&pods, &selector)
                .await?
                .ok_or_else(|| anyhow!("no running pod for {target}"))?;
            let port = container_port(&pod, remote)?;
            Ok((pod.metadata.name.clone().unwrap_or_default(), port))
        }
    }
}

/// The most usable pod for `selector`: running and ready first, as `kubectl`
/// prefers. Name order breaks ties so the choice is stable.
async fn pick_pod(pods: &Api<Pod>, selector: &str) -> Result<Option<Pod>> {
    let list = pods.list(&ListParams::default().labels(selector)).await?;
    let mut candidates: Vec<Pod> = list
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .collect();
    candidates.sort_by_key(|p| (std::cmp::Reverse(pod_rank(p)), p.metadata.name.clone()));
    Ok(candidates.into_iter().next())
}

pub fn pod_rank(pod: &Pod) -> u8 {
    let status = pod.status.as_ref();
    let running = status.and_then(|s| s.phase.as_deref()) == Some("Running");
    let ready = status
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    u8::from(running) * 2 + u8::from(ready)
}

/// `remote` as a port number, or the container port with that name.
pub fn container_port(pod: &Pod, remote: &str) -> Result<u16> {
    if let Ok(port) = remote.parse::<u16>() {
        return Ok(port);
    }
    let value = serde_json::to_value(pod.spec.as_ref())?;
    value
        .get("containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("ports")?.as_array())
        .flatten()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(remote))
        .and_then(|p| p.get("containerPort")?.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| {
            anyhow!(
                "pod {} has no container port named {remote}",
                pod.metadata.name.as_deref().unwrap_or_default()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(name: &str, phase: &str, ready: bool) -> Pod {
        serde_json::from_value(json!({
            "metadata": {"name": name},
            "spec": {"containers": [{"name": "app", "ports": [{"name": "http", "containerPort": 8080}]}]},
            "status": {"phase": phase, "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}
        }))
        .unwrap()
    }

    #[test]
    fn ports_follow_kubectl_syntax() {
        assert_eq!(parse_ports("8080:80").unwrap(), (8080, "80".into()));
        assert_eq!(parse_ports("5432").unwrap(), (5432, "5432".into()));
        assert_eq!(parse_ports(":http").unwrap(), (0, "http".into()));
        assert!(parse_ports("x:80").is_err());
        assert!(parse_ports("80:").is_err());
    }

    #[test]
    fn named_ports_resolve_from_the_pod_spec() {
        let p = pod("web", "Running", true);
        assert_eq!(container_port(&p, "http").unwrap(), 8080);
        assert_eq!(container_port(&p, "9090").unwrap(), 9090);
        assert!(container_port(&p, "grpc").is_err());
    }

    #[test]
    fn running_ready_pods_rank_first() {
        assert!(pod_rank(&pod("a", "Running", true)) > pod_rank(&pod("b", "Running", false)));
        assert!(pod_rank(&pod("b", "Running", false)) > pod_rank(&pod("c", "Pending", false)));
    }

    fn mock_client() -> Client {
        let pending = json!({"metadata": {"name": "a", "labels": {"app": "web"}},
            "spec": {"containers": [{"name": "app", "ports": [{"name": "http", "containerPort": 8080}]}]},
            "status": {"phase": "Pending"}});
        let ready = json!({"metadata": {"name": "b", "labels": {"app": "web"}},
            "spec": {"containers": [{"name": "app", "ports": [{"name": "http", "containerPort": 8080}]}]},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}});
        Client::new(
            tower::service_fn(move |request: http::Request<kube::client::Body>| {
                let body = match request.uri().path() {
                    "/api/v1/namespaces/default/services/web" => json!({
                        "metadata": {"name": "web"},
                        "spec": {"selector": {"app": "web"},
                            "ports": [{"name": "http", "port": 80, "targetPort": "http"},
                                      {"name": "metrics", "port": 9100}]}}),
                    "/api/v1/namespaces/default/pods" => {
                        assert_eq!(request.uri().query(), Some("&labelSelector=app%3Dweb"));
                        json!({"metadata": {}, "items": [pending.clone(), ready.clone()]})
                    }
                    "/api/v1/namespaces/default/pods/b" => ready.clone(),
                    "/apis/apps/v1/namespaces/default/deployments/api" => json!({
                        "apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api"},
                        "spec": {"selector": {"matchLabels": {"app": "web"}}}}),
                    path => panic!("unexpected request {path}"),
                };
                async move {
                    Ok::<_, std::convert::Infallible>(http::Response::new(
                        http_body_util::Full::new(hyper::body::Bytes::from(body.to_string())),
                    ))
                }
            }),
            "default",
        )
    }

    #[tokio::test]
    async fn targets_resolve_to_a_ready_pod_and_its_container_port() {
        let client = mock_client();
        for (target, remote, port) in [
            ("svc/web", "80", 8080),
            ("service/web", "http", 8080),
            ("svc/web", "9100", 9100),
            ("deploy/api", "http", 8080),
            ("pod/b", "9000", 9000),
            ("b", "http", 8080),
        ] {
            let resolved = resolve(&client, "default", target, remote).await.unwrap();
            assert_eq!(resolved, ("b".to_string(), port), "{target} {remote}");
        }
        let missing = resolve(&client, "default", "svc/web", "81")
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("has no port 81"), "{missing}");
        let kind = resolve(&client, "default", "cm/x", "80").await.unwrap_err();
        assert!(
            kind.to_string().contains("cannot port-forward to cm"),
            "{kind}"
        );
    }

    #[test]
    fn only_a_missing_or_refusing_pod_ends_the_forward() {
        let api = |code| {
            anyhow::Error::from(kube::Error::Api(
                kube::core::Status::failure("nope", "NotFound")
                    .with_code(code)
                    .boxed(),
            ))
        };
        assert_eq!(fatal(&api(404), "b").unwrap(), "pod b no longer exists");
        assert!(fatal(&api(403), "b").is_some());
        assert!(fatal(&api(500), "b").is_none());
        assert!(fatal(&anyhow!("connection refused"), "b").is_none());
    }

    #[tokio::test]
    async fn a_busy_local_port_fails_at_start() {
        let taken = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let client =
            Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
        let error = start(client, "default", "pod/web", &format!("{port}:80"))
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn port_zero_binds_a_free_port() {
        let client =
            Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
        let started = start(client, "default", "pod/web", ":80").unwrap();
        assert_ne!(started.local, 0);
        started.task.abort();
    }
}
