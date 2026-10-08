use super::*;

/// Serve aggregated discovery with a `widgets.example.com` CRD and count
/// discovery requests. Every other request gets an empty list.
fn serve_discovery(app: &mut App) -> Arc<AtomicU64> {
    let requests = Arc::new(AtomicU64::new(0));
    let seen = requests.clone();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let body = match request.uri().path() {
                "/apis" => {
                    seen.fetch_add(1, Ordering::SeqCst);
                    json!({"kind": "APIGroupDiscoveryList", "apiVersion": "apidiscovery.k8s.io/v2",
                        "metadata": {}, "items": [{"metadata": {"name": "example.com"},
                        "versions": [{"version": "v1", "freshness": "Current", "resources": [{
                            "resource": "widgets", "scope": "Namespaced", "singularResource": "widget",
                            "responseKind": {"group": "example.com", "version": "v1", "kind": "Widget"},
                            "verbs": ["get", "list", "watch"]}]}]}]})
                }
                "/api" => {
                    seen.fetch_add(1, Ordering::SeqCst);
                    json!({"kind": "APIGroupDiscoveryList", "apiVersion": "apidiscovery.k8s.io/v2",
                        "metadata": {}, "items": [{"metadata": {"name": ""},
                        "versions": [{"version": "v1", "freshness": "Current", "resources": [{
                            "resource": "pods", "scope": "Namespaced", "singularResource": "pod",
                            "responseKind": {"group": "", "version": "v1", "kind": "Pod"},
                            "verbs": ["get", "list", "watch"]}]}]}]})
                }
                _ => {
                    json!({"apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": "1"}, "items": []})
                }
            };
            async move {
                Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Full::new(
                    hyper::body::Bytes::from(body.to_string()),
                )))
            }
        }),
        "default",
    );
    requests
}

async fn next_rediscovery(app: &mut App, rx: &mut Receiver<Msg>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx.recv().await.unwrap();
            let done = matches!(msg, Msg::Rediscovered { .. });
            app.handle_msg(msg);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_crd_installed_after_connect_opens_from_the_palette() {
    let (mut app, mut rx) = test_app();
    app.switch_kind("pods");
    assert!(app.cluster.resolve("widgets").is_none());
    let requests = serve_discovery(&mut app);
    palette(&mut app, "widgets");
    assert!(!app.flash_err, "{}", app.flash);
    assert!(app.flash.contains("API discovery"), "{}", app.flash);
    next_rediscovery(&mut app, &mut rx).await;
    assert_eq!(app.kind_plural, "widgets");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    // Kinds discovered at connect stay usable.
    assert!(app.cluster.resolve("deployments").is_some());
}

#[tokio::test]
async fn a_typo_runs_discovery_once_per_cooldown() {
    let (mut app, mut rx) = test_app();
    app.switch_kind("pods");
    let requests = serve_discovery(&mut app);
    palette(&mut app, "zzqqxx");
    next_rediscovery(&mut app, &mut rx).await;
    assert!(app.flash_err);
    assert!(
        app.flash.contains("No resource matches 'zzqqxx'"),
        "{}",
        app.flash
    );
    assert_eq!(app.kind_plural, "pods");
    palette(&mut app, "zzqqxx");
    assert!(app.flash_err);
    assert!(
        app.flash.contains("No resource matches 'zzqqxx'"),
        "{}",
        app.flash
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_late_discovery_result_does_not_move_the_user() {
    let (mut app, mut rx) = test_app();
    app.switch_kind("pods");
    serve_discovery(&mut app);
    palette(&mut app, "widgets");
    palette(&mut app, "deployments");
    next_rediscovery(&mut app, &mut rx).await;
    assert_eq!(app.kind_plural, "deployments");
    // The new kind is still registered for the next `:`.
    assert!(app.cluster.resolve("widgets").is_some());
}
