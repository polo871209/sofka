//! Cloud log viewer links for the selected object.
//!
//! `L` builds a GKE Logs Explorer query for the selection and opens it in the
//! browser. The query matches pods by their owner's pod selector, so it also
//! covers restarted and deleted pods, and never pods of another workload.

use serde_json::Value;

/// What the log query selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pod {
        ns: String,
        pod: String,
        container: Option<String>,
    },
    /// Every pod a workload or Service selects.
    Selector {
        ns: String,
        requirements: Vec<Requirement>,
    },
    /// A CronJob has no selector, but GKE tags each of its pods with the
    /// CronJob name.
    CronJob {
        ns: String,
        name: String,
    },
    Namespace {
        name: String,
    },
    Node {
        name: String,
    },
}

/// One label-selector requirement, in `matchExpressions` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub key: String,
    pub op: String,
    pub values: Vec<String>,
}

/// The requirements of a workload's `spec.selector` (`matchLabels` and
/// `matchExpressions`), or of a Service's plain `spec.selector` map.
pub fn selector_requirements(selector: &Value, service: bool) -> Vec<Requirement> {
    let label = |(key, value): (&String, &Value)| {
        value.as_str().map(|value| Requirement {
            key: key.clone(),
            op: "In".into(),
            values: vec![value.to_string()],
        })
    };
    if service {
        return selector
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(label)
            .collect();
    }
    let mut out: Vec<Requirement> = selector
        .get("matchLabels")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(label)
        .collect();
    for expr in selector
        .get("matchExpressions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(key), Some(op)) = (
            expr.get("key").and_then(Value::as_str),
            expr.get("operator").and_then(Value::as_str),
        ) else {
            continue;
        };
        let values = expr
            .get("values")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        out.push(Requirement {
            key: key.into(),
            op: op.into(),
            values,
        });
    }
    out
}

/// The Logs Explorer URL for `target` on the kubeconfig cluster `cluster`.
pub fn url(cluster: &str, target: &Target) -> Result<String, String> {
    let Some(rest) = cluster.strip_prefix("gke_") else {
        return Err(format!(
            "no cloud log provider matches cluster {cluster:?} (supported: GKE)"
        ));
    };
    // GKE kubeconfig names are gke_<project>_<location>_<cluster>, and no part can contain "_".
    let parts: Vec<&str> = rest.split('_').collect();
    let [project, location, cluster_name] = parts[..] else {
        return Err(format!("cannot parse GKE cluster name {cluster:?}"));
    };
    if [project, location, cluster_name]
        .iter()
        .any(|p| p.is_empty())
    {
        return Err(format!("cannot parse GKE cluster name {cluster:?}"));
    }

    let resource_type = match target {
        Target::Node { .. } => "k8s_node",
        _ => "k8s_container",
    };
    let mut terms = vec![
        format!("resource.type=\"{resource_type}\""),
        format!("resource.labels.project_id=\"{project}\""),
        format!("resource.labels.location=\"{location}\""),
        format!("resource.labels.cluster_name=\"{cluster_name}\""),
    ];
    match target {
        Target::Node { name } => terms.push(format!("resource.labels.node_name=\"{name}\"")),
        Target::Namespace { name } => {
            terms.push(format!("resource.labels.namespace_name=\"{name}\""));
        }
        Target::Pod { ns, pod, container } => {
            terms.push(format!("resource.labels.namespace_name=\"{ns}\""));
            terms.push(format!("resource.labels.pod_name=\"{pod}\""));
            if let Some(container) = container {
                terms.push(format!("resource.labels.container_name=\"{container}\""));
            }
        }
        Target::CronJob { ns, name } => {
            terms.push(format!("resource.labels.namespace_name=\"{ns}\""));
            terms.push(r#"labels."logging.gke.io/top_level_controller_type"="CronJob""#.into());
            terms.push(format!(
                r#"labels."logging.gke.io/top_level_controller_name"="{name}""#
            ));
        }
        Target::Selector { ns, requirements } => {
            terms.push(format!("resource.labels.namespace_name=\"{ns}\""));
            for requirement in requirements {
                terms.push(label_term(requirement)?);
            }
        }
    }

    Ok(format!(
        "https://console.cloud.google.com/logs/query;query={};duration=PT1H?project={}",
        percent_encode(&terms.join("\n")),
        percent_encode(project)
    ))
}

/// GKE writes pod label `a.b/c` as log label `k8s-pod/a_b/c`.
fn label_term(requirement: &Requirement) -> Result<String, String> {
    let field = format!("labels.\"k8s-pod/{}\"", requirement.key.replace('.', "_"));
    let any = requirement
        .values
        .iter()
        .map(|value| format!("{field}=\"{value}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    Ok(match requirement.op.as_str() {
        "In" if requirement.values.len() == 1 => any,
        "In" => format!("({any})"),
        "NotIn" => format!("NOT ({any})"),
        "Exists" => format!("{field}:*"),
        "DoesNotExist" => format!("NOT {field}:*"),
        op => return Err(format!("unknown selector operator {op:?}")),
    })
}

/// Percent-encode every byte outside the RFC 3986 unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Open `url` in the default browser.
pub fn open(url: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    // explorer exits 1 even when it opened the URL.
    if status.success() || cfg!(windows) {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query(url: &str) -> String {
        let encoded = url
            .split(";query=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap();
        form_urlencoded::parse(format!("q={}", encoded.replace('+', "%2B")).as_bytes())
            .next()
            .unwrap()
            .1
            .into_owned()
    }

    #[test]
    fn pod_query_scopes_to_the_gke_cluster() {
        let url = url(
            "gke_my-proj_us-central1_prod",
            &Target::Pod {
                ns: "web".into(),
                pod: "api-7d9f-x2".into(),
                container: Some("app".into()),
            },
        )
        .unwrap();
        assert!(
            url.starts_with("https://console.cloud.google.com/logs/query;query="),
            "{url}"
        );
        assert!(url.ends_with(";duration=PT1H?project=my-proj"), "{url}");
        assert_eq!(
            query(&url),
            "resource.type=\"k8s_container\"\n\
             resource.labels.project_id=\"my-proj\"\n\
             resource.labels.location=\"us-central1\"\n\
             resource.labels.cluster_name=\"prod\"\n\
             resource.labels.namespace_name=\"web\"\n\
             resource.labels.pod_name=\"api-7d9f-x2\"\n\
             resource.labels.container_name=\"app\""
        );
    }

    #[test]
    fn selector_terms_cover_every_operator() {
        let selector = json!({
            "matchLabels": {"app.kubernetes.io/name": "api"},
            "matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["a", "b"]},
                {"key": "track", "operator": "NotIn", "values": ["canary"]},
                {"key": "team", "operator": "Exists"},
                {"key": "legacy", "operator": "DoesNotExist"}
            ]
        });
        let requirements = selector_requirements(&selector, false);
        let url = url(
            "gke_p_europe-west1-b_c",
            &Target::Selector {
                ns: "web".into(),
                requirements,
            },
        )
        .unwrap();
        let q = query(&url);
        let terms: Vec<&str> = q.lines().skip(5).collect();
        assert_eq!(
            terms,
            [
                r#"labels."k8s-pod/app_kubernetes_io/name"="api""#,
                r#"(labels."k8s-pod/tier"="a" OR labels."k8s-pod/tier"="b")"#,
                r#"NOT (labels."k8s-pod/track"="canary")"#,
                r#"labels."k8s-pod/team":*"#,
                r#"NOT labels."k8s-pod/legacy":*"#,
            ]
        );
    }

    #[test]
    fn service_selector_is_a_plain_map() {
        let requirements = selector_requirements(&json!({"app": "api", "tier": "web"}), true);
        assert_eq!(
            requirements,
            [
                Requirement {
                    key: "app".into(),
                    op: "In".into(),
                    values: vec!["api".into()],
                },
                Requirement {
                    key: "tier".into(),
                    op: "In".into(),
                    values: vec!["web".into()],
                },
            ]
        );
    }

    #[test]
    fn node_cronjob_and_namespace_queries() {
        let node = query(&url("gke_p_l_c", &Target::Node { name: "n1".into() }).unwrap());
        assert!(node.starts_with("resource.type=\"k8s_node\""), "{node}");
        assert!(node.ends_with("resource.labels.node_name=\"n1\""), "{node}");

        let cron = query(
            &url(
                "gke_p_l_c",
                &Target::CronJob {
                    ns: "jobs".into(),
                    name: "nightly".into(),
                },
            )
            .unwrap(),
        );
        assert!(
            cron.ends_with(
                "labels.\"logging.gke.io/top_level_controller_type\"=\"CronJob\"\n\
                 labels.\"logging.gke.io/top_level_controller_name\"=\"nightly\""
            ),
            "{cron}"
        );

        let ns = query(&url("gke_p_l_c", &Target::Namespace { name: "web".into() }).unwrap());
        assert!(
            ns.ends_with("resource.labels.namespace_name=\"web\""),
            "{ns}"
        );
    }

    #[test]
    fn rejects_non_gke_and_malformed_cluster_names() {
        let pod = Target::Namespace { name: "x".into() };
        assert!(
            url("kind-dev", &pod)
                .unwrap_err()
                .contains("supported: GKE")
        );
        assert!(url("gke_p_l", &pod).unwrap_err().contains("cannot parse"));
        assert!(
            url("gke_p_l_c_d", &pod)
                .unwrap_err()
                .contains("cannot parse")
        );
        assert!(url("gke__l_c", &pod).unwrap_err().contains("cannot parse"));
    }

    #[test]
    fn percent_encode_keeps_only_unreserved_bytes() {
        assert_eq!(
            percent_encode("a-Z.9_~ \"/\n=é"),
            "a-Z.9_~%20%22%2F%0A%3D%C3%A9"
        );
    }
}
