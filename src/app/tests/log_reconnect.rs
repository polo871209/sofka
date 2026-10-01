use super::*;
use std::sync::Mutex;

fn running_pod(phase: &str, state: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "web", "namespace": "default", "resourceVersion": "1"},
        "spec": {"containers": [{"name": "app", "image": "test"}]},
        "status": {"phase": phase, "containerStatuses": [{"name": "app", "ready": true,
            "restartCount": 1, "image": "test", "imageID": "", "state": state}]}})
}

/// Serve log requests from `bodies` in order, then empty bodies, and answer
/// pod reads with `pod`. Returns the query of every log request.
fn serve_logs(app: &mut App, bodies: Vec<&'static str>, pod: Value) -> Arc<Mutex<Vec<String>>> {
    let queries = Arc::new(Mutex::new(Vec::new()));
    let seen = queries.clone();
    let bodies = Arc::new(Mutex::new(std::collections::VecDeque::from(bodies)));
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let uri = request.uri().clone();
            let body = if uri.path().ends_with("/log") {
                seen.lock()
                    .unwrap()
                    .push(uri.query().unwrap_or_default().to_owned());
                bodies.lock().unwrap().pop_front().unwrap_or("").to_owned()
            } else if uri.query().is_some_and(|q| q.contains("watch=true")) {
                String::new()
            } else {
                json!({"apiVersion": "v1", "kind": "PodList",
                    "metadata": {"resourceVersion": "1"}, "items": [pod.clone()]})
                .to_string()
            };
            async move {
                Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Full::new(
                    hyper::body::Bytes::from(body),
                )))
            }
        }),
        "default",
    );
    queries
}

fn open_pod_logs(pod: Value) -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("pods");
    apply(&mut app, pod);
    app.table_state.select(Some(0));
    (app, rx)
}

#[tokio::test]
async fn followed_logs_resume_after_the_stream_ends_without_repeating_lines() {
    let pod = running_pod("Running", json!({"running": {}}));
    let (mut app, mut rx) = open_pod_logs(pod.clone());
    let queries = serve_logs(
        &mut app,
        vec![
            "2026-10-01T00:00:01.5Z a\n2026-10-01T00:00:02.25Z b\n2026-10-01T00:00:02.25Z b\n",
            // `sinceTime` has second precision, so the server repeats `b`.
            "2026-10-01T00:00:02.1Z early\n2026-10-01T00:00:02.25Z b\n2026-10-01T00:00:02.25Z b\n2026-10-01T00:00:03Z c\n",
        ],
        pod,
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.logs.view.lines.len() < 4 {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(app.logs.view.lines, ["a", "b", "b", "c"]);
    let queries = queries.lock().unwrap().clone();
    let first: HashMap<String, String> = form_urlencoded::parse(queries[0].as_bytes())
        .into_owned()
        .collect();
    assert!(first.contains_key("tailLines"));
    let second: HashMap<String, String> = form_urlencoded::parse(queries[1].as_bytes())
        .into_owned()
        .collect();
    assert_eq!(
        second.get("sinceTime").map(String::as_str),
        Some("2026-10-01T00:00:02Z")
    );
    assert!(!second.contains_key("tailLines"));
    assert!(!second.contains_key("sinceSeconds"));
    assert_eq!(second.get("follow").map(String::as_str), Some("true"));
}

#[tokio::test]
async fn logs_of_a_finished_pod_do_not_reconnect() {
    let pod = running_pod("Succeeded", json!({"terminated": {"exitCode": 0}}));
    let (mut app, mut rx) = open_pod_logs(pod.clone());
    let queries = serve_logs(&mut app, vec!["2026-10-01T00:00:01Z done\n"], pod);
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.logs.view.lines.is_empty() {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    // The reconnect check runs after a one-second pause.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(queries.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn previous_logs_do_not_reconnect() {
    let pod = running_pod("Running", json!({"running": {}}));
    let (mut app, mut rx) = open_pod_logs(pod.clone());
    let queries = serve_logs(&mut app, vec!["2026-10-01T00:00:01Z old\n"], pod);
    app.handle_key(press(KeyCode::Char('p'))).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.logs.view.lines.is_empty() {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(queries.lock().unwrap().len(), 1);
}

#[test]
fn log_source_state_follows_restart_policy() {
    let pod = |phase: &str, policy: &str, state: Value| -> Pod {
        let mut p = running_pod(phase, state);
        p["spec"]["restartPolicy"] = policy.into();
        serde_json::from_value(p).unwrap()
    };
    let running = json!({"running": {}});
    let waiting = json!({"waiting": {"reason": "CrashLoopBackOff"}});
    let failed = json!({"terminated": {"exitCode": 1}});
    let ok = json!({"terminated": {"exitCode": 0}});
    assert_eq!(
        log_source_state(&pod("Running", "Always", running), None),
        Some(true)
    );
    assert_eq!(
        log_source_state(&pod("Running", "Always", waiting), None),
        Some(false)
    );
    assert_eq!(
        log_source_state(&pod("Running", "Always", ok.clone()), None),
        Some(false)
    );
    assert_eq!(
        log_source_state(&pod("Running", "OnFailure", failed.clone()), None),
        Some(false)
    );
    assert_eq!(
        log_source_state(&pod("Running", "OnFailure", ok), None),
        None
    );
    assert_eq!(
        log_source_state(&pod("Running", "Never", failed.clone()), None),
        None
    );
    assert_eq!(
        log_source_state(&pod("Failed", "Always", failed), None),
        None
    );
}

#[test]
fn log_resume_drops_only_the_replayed_lines() {
    let mut resume = LogResume::default();
    for line in ["2026-10-01T00:00:01Z x", "2026-10-01T00:00:01Z x"] {
        assert!(resume.admit(line));
    }
    let since = resume.since().unwrap();
    assert_eq!(since.to_string(), "2026-10-01T00:00:01Z");
    // Both copies were shown, so both replays are dropped. A third is new.
    assert!(!resume.admit("2026-10-01T00:00:01Z x"));
    assert!(!resume.admit("2026-10-01T00:00:01Z x"));
    assert!(resume.admit("2026-10-01T00:00:01Z x"));
    // A second replay of the same second drops the same lines again.
    resume.since().unwrap();
    for _ in 0..3 {
        assert!(!resume.admit("2026-10-01T00:00:01Z x"));
    }
    assert!(resume.admit("2026-10-01T00:00:02Z y"));
    assert!(!resume.untimed);
    assert!(resume.admit("no timestamp"));
    assert!(resume.untimed);
}
