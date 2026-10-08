use super::*;

/// The discovery result of a cluster that has since gained `kinds`, as
/// `(kind, plural)` pairs in `example.com`.
fn with_kinds(kinds: &[(&str, &str)]) -> Box<crate::k8s::Discovery> {
    let mut cluster = Cluster::fake();
    for (kind, plural) in kinds {
        cluster.register_kind("example.com", kind, plural, true);
    }
    Box::new(cluster.fake_discovery())
}

fn with_widgets() -> Box<crate::k8s::Discovery> {
    with_kinds(&[("Widget", "widgets")])
}

/// Deliver a result for the latest rediscovery on the current context.
fn deliver(app: &mut App, result: Result<Box<crate::k8s::Discovery>, String>) {
    let (epoch, request) = (app.context_epoch, app.rediscovery_request);
    deliver_as(app, epoch, request, result);
}

fn deliver_as(
    app: &mut App,
    epoch: u64,
    request: u64,
    result: Result<Box<crate::k8s::Discovery>, String>,
) {
    app.handle_msg(Msg::Rediscovered {
        epoch,
        request,
        result,
    });
}

#[tokio::test]
async fn a_kind_missing_at_startup_resolves_after_rediscovery() {
    let (mut app, _rx) = test_app();
    app.user_aliases.insert("wd".into(), "widgets".into());
    app.cluster.add_aliases(&app.user_aliases);
    app.switch_kind("pods");

    palette(&mut app, "widgets");
    assert_eq!(app.kind_plural, "pods");
    assert!(!app.flash_err, "{}", app.flash);
    assert!(app.flash.contains("looking up 'widgets'"), "{}", app.flash);

    deliver(&mut app, Ok(with_widgets()));
    assert_eq!(app.kind_plural, "widgets");
    assert!(!app.flash_err, "{}", app.flash);

    // The user alias aimed at the new kind now resolves too.
    palette(&mut app, "pods");
    palette(&mut app, "wd");
    assert_eq!(app.kind_plural, "widgets");

    // The palette catalog lists it.
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "widg".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    assert!(
        app.cmd_suggestions.iter().any(|s| s.label == "widgets"),
        "{:?}",
        app.cmd_suggestions
            .iter()
            .map(|s| &s.label)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_query_retries_with_its_namespace_after_rediscovery() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "widgets -n prod");
    assert!(!app.flash_err, "{}", app.flash);

    deliver(&mut app, Ok(with_widgets()));
    assert_eq!(app.kind_plural, "widgets");
    assert_eq!(app.namespace, "prod");
}

#[tokio::test]
async fn a_lookup_that_still_misses_shows_the_usual_error() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "gadgets");
    deliver(&mut app, Ok(with_widgets()));
    assert!(app.flash_err);
    assert_eq!(app.flash, "No resource matches 'gadgets'");

    // A failed discovery reports the miss as well.
    app.last_rediscovery = None;
    palette(&mut app, "gadgets");
    deliver(&mut app, Err("connection refused".into()));
    assert!(app.flash_err);
    assert_eq!(app.flash, "No resource matches 'gadgets'");
}

#[tokio::test]
async fn rediscovery_is_throttled_to_one_per_ten_seconds() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "widgets");
    assert!(!app.flash_err, "{}", app.flash);

    // A second miss while the first discovery runs reports at once.
    palette(&mut app, "gadgets");
    assert!(app.flash_err);
    assert_eq!(app.flash, "No resource matches 'gadgets'");

    deliver(&mut app, Ok(with_widgets()));
    assert_eq!(app.kind_plural, "widgets");

    palette(&mut app, "gadgets");
    assert!(app.flash_err);
    assert_eq!(app.flash, "No resource matches 'gadgets'");

    app.last_rediscovery = Some(std::time::Instant::now() - Duration::from_secs(9));
    palette(&mut app, "gadgets");
    assert!(app.flash_err);
    assert_eq!(app.flash, "No resource matches 'gadgets'");

    app.last_rediscovery = Some(std::time::Instant::now() - Duration::from_secs(11));
    palette(&mut app, "gadgets");
    assert!(!app.flash_err, "{}", app.flash);
    assert!(app.flash.contains("looking up 'gadgets'"), "{}", app.flash);
}

#[tokio::test]
async fn a_result_from_before_a_context_switch_is_ignored() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "widgets");
    let (stale, request) = (app.context_epoch, app.rediscovery_request);

    let mut west = Cluster::fake();
    west.context = "west".into();
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: "west".into(),
        result: Ok(Box::new(west)),
    });
    let kind = app.kind_plural.clone();

    deliver_as(&mut app, stale, request, Ok(with_widgets()));
    assert!(app.cluster.resolve("widgets").is_none());
    assert_eq!(app.kind_plural, kind);
}

#[tokio::test]
async fn navigating_away_keeps_the_new_kinds_but_drops_the_retry() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "widgets");
    palette(&mut app, "deploy");
    assert_eq!(app.kind_plural, "deployments");

    deliver(&mut app, Ok(with_widgets()));
    assert_eq!(app.kind_plural, "deployments");
    assert!(app.cluster.resolve("widgets").is_some());
}

#[tokio::test]
async fn a_superseded_run_does_not_answer_the_newer_lookup() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "widgets");
    let first = app.rediscovery_request;
    // The first run is still going when the throttle lets a second one start.
    app.last_rediscovery = Some(std::time::Instant::now() - Duration::from_secs(11));
    palette(&mut app, "gadgets");
    assert!(!app.flash_err, "{}", app.flash);

    let epoch = app.context_epoch;
    deliver_as(&mut app, epoch, first, Ok(with_widgets()));
    assert!(app.cluster.resolve("widgets").is_none());
    assert_ne!(app.kind_plural, "widgets");

    deliver(&mut app, Ok(with_kinds(&[("Gadget", "gadgets")])));
    assert_eq!(app.kind_plural, "gadgets");
}

#[tokio::test]
async fn new_kinds_are_not_hidden_by_the_old_rbac_allow_list() {
    let (mut app, _rx) = test_app();
    app.rbac_allowed = Some(HashSet::from(["pods".to_string()]));
    palette(&mut app, "widgets");
    // No RBAC review answers: the cluster is unreachable in tests.
    deliver(&mut app, Ok(with_widgets()));
    assert_eq!(app.kind_plural, "widgets");

    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert!(
        app.cmd_suggestions.iter().any(|s| s.label == "widgets"),
        "{:?}",
        app.cmd_suggestions
            .iter()
            .map(|s| &s.label)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_miss_names_the_api_groups_discovery_could_not_read() {
    let mut cluster = Cluster::fake();
    cluster.register_kind("example.com", "Widget", "widgets", true);
    cluster.discovery_warnings = vec!["broken.example.com/v1: 503 Service Unavailable".into()];
    let discovery = || Ok(Box::new(cluster.fake_discovery()));

    let (mut app, _rx) = test_app();
    palette(&mut app, "gadgets");
    deliver(&mut app, discovery());
    assert!(app.flash_err);
    assert_eq!(
        app.flash,
        "No resource matches 'gadgets'; API discovery could not read 1 API group. \
         Refer to :info for details."
    );

    // A retry that finds its kind says nothing about the skipped group.
    app.last_rediscovery = None;
    palette(&mut app, "widgets");
    deliver(&mut app, discovery());
    assert_eq!(app.kind_plural, "widgets");
    assert!(!app.flash_err, "{}", app.flash);
}
