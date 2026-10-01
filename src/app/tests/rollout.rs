use super::*;
use http_body_util::BodyExt;

type Requests = mpsc::UnboundedReceiver<(http::Method, String, Option<String>, Value)>;

fn deployment(paused: bool) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "a", "uid": "web-uid"},
        "spec": {"paused": paused,
            "selector": {"matchLabels": {"app": "web"}},
            "template": template("nginx:1.25")}})
}

fn template(image: &str) -> Value {
    json!({"metadata": {"labels": {"app": "web"}},
        "spec": {"containers": [{"name": "web", "image": image}]}})
}

fn replicaset(name: &str, owner: &str, revision: i64, image: &str) -> Value {
    let mut template = template(image);
    template["metadata"]["labels"]["pod-template-hash"] = json!(name);
    json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
        "metadata": {"name": format!("{owner}-{name}"), "namespace": "a",
            "creationTimestamp": format!("2024-01-1{revision}T10:30:00Z"),
            "annotations": {"deployment.kubernetes.io/revision": revision.to_string()},
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment",
                "name": owner, "uid": format!("{owner}-uid")}]},
        "spec": {"template": template}})
}

/// A deployments view with `web` selected and a fake API that answers the
/// undo's GET with `live` and records every request.
fn deployment_history(live: Value) -> (App, Receiver<Msg>, Requests) {
    let (mut app, rx) = test_app();
    app.cluster
        .register_kind("apps", "ReplicaSet", "replicasets", true);
    app.switch_kind("deployments");
    app.namespace = "a".into();
    apply(&mut app, deployment(false));
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    for (name, owner, revision, image) in [
        ("aaa", "web", 1, "nginx:1.23"),
        ("bbb", "web", 3, "nginx:1.25"),
        ("ccc", "web", 2, "nginx:1.24"),
        ("ddd", "api", 9, "api:9"),
    ] {
        apply(&mut app, replicaset(name, owner, revision, image));
    }
    let (requests, received) = mpsc::unbounded_channel();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let requests = requests.clone();
            let live = live.clone();
            async move {
                let (parts, body) = request.into_parts();
                // Only the undo's own calls, not access reviews.
                if parts.uri.path().contains("/deployments/") {
                    let bytes = body.collect().await.unwrap().to_bytes();
                    let patch = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                    let content_type = parts
                        .headers
                        .get("content-type")
                        .map(|v| v.to_str().unwrap().to_string());
                    requests
                        .send((
                            parts.method.clone(),
                            parts.uri.path().to_string(),
                            content_type,
                            patch,
                        ))
                        .unwrap();
                }
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(200)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from(
                            live.to_string(),
                        )))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    (app, rx, received)
}

async fn flash(app: &mut App, rx: &mut Receiver<Msg>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(msg) = rx.recv().await {
            if matches!(msg, Msg::Flash { .. }) {
                app.handle_msg(msg);
                return;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ctrl_u_lists_the_workload_revisions_newest_first() {
    let (app, _rx, _requests) = deployment_history(deployment(false));
    assert_eq!(app.kind_plural, "rollouthistory");
    assert_eq!(app.resource_title(), "rollout history");
    assert_eq!(app.scope_label.as_deref(), Some("deployment/web"));
    let (headers, rows) = app.snapshot_table();
    let col = |h: &str| headers.iter().position(|x| x == h).unwrap();
    let summary: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r[col("REVISION")].as_str(),
                r[col("STATUS")].as_str(),
                r[col("IMAGES")].as_str(),
                r[col("CREATED")].as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("3", "deployed", "nginx:1.25", "2024-01-13 10:30:00"),
            ("2", "superseded", "nginx:1.24", "2024-01-12 10:30:00"),
            ("1", "superseded", "nginx:1.23", "2024-01-11 10:30:00"),
        ]
    );
}

#[tokio::test]
async fn deployed_revision_moves_when_a_rollback_renumbers_one() {
    let (mut app, _rx, _requests) = deployment_history(deployment(false));
    apply(&mut app, replicaset("aaa", "web", 4, "nginx:1.23"));
    let (headers, rows) = app.snapshot_table();
    let status = headers.iter().position(|h| h == "STATUS").unwrap();
    let revision = headers.iter().position(|h| h == "REVISION").unwrap();
    let states: Vec<_> = rows
        .iter()
        .map(|r| (r[revision].as_str(), r[status].as_str()))
        .collect();
    assert_eq!(
        states,
        [("4", "deployed"), ("3", "superseded"), ("2", "superseded")]
    );
}

#[tokio::test]
async fn enter_diffs_then_enter_and_y_roll_back() {
    let (mut app, mut rx, mut requests) = deployment_history(deployment(false));
    app.handle_key(press(KeyCode::Down)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Diff);
    assert!(
        app.detail.title.contains("deployed v3 → v2"),
        "{}",
        app.detail.title
    );
    let lines: Vec<&str> = app.detail.lines.iter().map(String::as_str).collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('-') && l.contains("nginx:1.25"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('+') && l.contains("nginx:1.24"))
    );
    assert!(!lines.iter().any(|l| l.contains("pod-template-hash")));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(
        app.confirm_label,
        "Roll back deployment web in a to revision 2?"
    );
    assert!(requests.try_recv().is_err());
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Table);

    let (method, path, _, _) = requests.recv().await.unwrap();
    assert_eq!(method, http::Method::GET);
    assert_eq!(path, "/apis/apps/v1/namespaces/a/deployments/web");
    let (method, path, content_type, patch) = requests.recv().await.unwrap();
    assert_eq!(method, http::Method::PATCH);
    assert_eq!(path, "/apis/apps/v1/namespaces/a/deployments/web");
    assert_eq!(
        content_type.as_deref(),
        Some("application/strategic-merge-patch+json")
    );
    let mut expected = template("nginx:1.24");
    expected["$patch"] = json!("replace");
    assert_eq!(patch, json!({"spec": {"template": expected}}));
    flash(&mut app, &mut rx).await;
    assert_eq!(app.flash, "rolled back web to revision 2");
    assert!(!app.flash_err);
}

#[tokio::test]
async fn cancel_and_deployed_revision_send_nothing() {
    let (mut app, _rx, mut requests) = deployment_history(deployment(false));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.flash, "revision 3 is already deployed");
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Table);

    app.handle_key(press(KeyCode::Down)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.confirm_action.is_none());

    // Esc leaves the diff, and enter in an unrelated diff confirms nothing.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.kind_plural, "rollouthistory");
    app.mode = Mode::Diff;
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Diff);

    app.mode = Mode::Table;
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn readonly_and_guardrails_block_the_rollback() {
    let (mut app, _rx, mut requests) = deployment_history(deployment(false));
    app.handle_key(press(KeyCode::Down)).unwrap();
    app.readonly = true;
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Diff);
    assert_eq!(app.flash, "read-only mode — action disabled");

    app.readonly = false;
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["rollback".into()],
        confirmation: Some("type-resource-name".into()),
        ..Default::default()
    }];
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(app.prompt_label.contains("'web'"), "{}", app.prompt_label);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn paused_deployment_is_not_patched() {
    let (mut app, mut rx, mut requests) = deployment_history(deployment(true));
    app.handle_key(press(KeyCode::Down)).unwrap();
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let (method, _, _, _) = requests.recv().await.unwrap();
    assert_eq!(method, http::Method::GET);
    flash(&mut app, &mut rx).await;
    assert!(app.flash_err);
    assert!(app.flash.contains("paused"), "{}", app.flash);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn statefulset_history_uses_controller_revisions() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("apps", "StatefulSet", "statefulsets", true);
    app.cluster
        .register_kind("apps", "ControllerRevision", "controllerrevisions", true);
    app.switch_kind("statefulsets");
    app.namespace = "a".into();
    apply(
        &mut app,
        json!({"apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": "db", "namespace": "a", "uid": "db-uid"},
            "spec": {"selector": {"matchLabels": {"app": "db"}}}}),
    );
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    assert_eq!(app.kind.as_ref().unwrap().ar.plural, "controllerrevisions");
    assert_eq!(app.labels.as_deref(), Some("app=db"));
    for (revision, image) in [(1, "postgres:15"), (2, "postgres:16")] {
        apply(
            &mut app,
            json!({"apiVersion": "apps/v1", "kind": "ControllerRevision",
                "metadata": {"name": format!("db-{revision}"), "namespace": "a",
                    "ownerReferences": [{"apiVersion": "apps/v1", "kind": "StatefulSet",
                        "name": "db", "uid": "db-uid"}]},
                "revision": revision,
                "data": {"spec": {"template": {"$patch": "replace",
                    "spec": {"containers": [{"name": "db", "image": image}]}}}}}),
        );
    }
    let (headers, rows) = app.snapshot_table();
    let images = headers.iter().position(|h| h == "IMAGES").unwrap();
    assert_eq!(rows[0][images], "postgres:16");
    app.handle_key(press(KeyCode::Down)).unwrap();
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(
        app.confirm_label,
        "Roll back statefulset db in a to revision 1?"
    );
}

#[tokio::test]
async fn ctrl_u_outside_workloads_says_where_it_applies() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "p", "namespace": "default"}}),
    );
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(
        app.flash,
        "rollout undo applies to deployments, statefulsets, and daemonsets"
    );
}

fn screen(app: &mut App, width: u16) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    (0..30)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn hints_show_the_undo_key_in_the_header() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    apply(&mut app, deployment(false));
    assert!(screen(&mut app, 140).contains("ctrl-u undo"));

    let (mut app, _rx, _requests) = deployment_history(deployment(false));
    let wide = screen(&mut app, 140);
    assert!(!wide.contains("enter diff"), "the header leaves out enter");
    assert!(wide.contains("r rollback"));
}
