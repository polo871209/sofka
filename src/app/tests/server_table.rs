use super::*;
use crate::server_table::{Table, Update};
use futures_util::{StreamExt, stream};
use hyper::body::{Bytes, Frame};
use std::convert::Infallible;
use std::sync::Mutex;

fn resource(name: &str, version: &str, uid: &str) -> serde_json::Value {
    json!({
        "apiVersion": "example.com/v1", "kind": "Widget",
        "metadata": {"name": name, "namespace": "default", "uid": uid, "resourceVersion": version},
        "spec": {"detail": "full object data"}
    })
}

fn table(objects: &[serde_json::Value], counts: &[serde_json::Value]) -> serde_json::Value {
    json!({
        "kind": "Table", "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": "10"},
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name"},
            {"name": "Count", "type": "integer"},
            {"name": "Ratio", "type": "string"},
            {"name": "Detail", "type": "string", "priority": 1}
        ],
        "rows": objects.iter().zip(counts).map(|(obj, count)| json!({
            "cells": [obj["metadata"]["name"], count, "2/3", "server detail"],
            "object": {"metadata": obj["metadata"]}
        })).collect::<Vec<_>>()
    })
}

fn deliver(app: &mut App, value: serde_json::Value, replace: bool) {
    let table = Table::decode(value, &app.server_table.columns).unwrap();
    app.handle_msg(Msg::ServerTable {
        generation: app.generation,
        resource: app.kind.as_ref().unwrap().resource_key(),
        update: if replace {
            Update::Replace(table)
        } else {
            Update::Apply(table)
        },
    });
}

fn open_widgets(app: &mut App) {
    app.cluster
        .register_kind("example.com", "Widget", "widgets", true);
    palette(app, "widgets.example.com");
}

fn set_filter(app: &mut App, text: &str) {
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    for ch in text.chars() {
        app.handle_key(press(KeyCode::Char(ch))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
}

/// Sorts low to high: `S` picks high to low, then `I` inverts.
fn sort_ascending(app: &mut App, header: &str) {
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    for ch in header.chars() {
        app.handle_key(press(KeyCode::Char(ch))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Char('I'))).unwrap();
}

#[tokio::test]
async fn server_columns_sort_filter_and_keep_full_objects() {
    let (mut app, _rx) = test_app();
    open_widgets(&mut app);
    let objects = [
        resource("large", "1", "a"),
        resource("small", "1", "b"),
        resource("missing", "1", "c"),
    ];
    for object in &objects {
        apply(&mut app, object.clone());
    }
    deliver(
        &mut app,
        table(&objects, &[json!(10), json!(2), json!(null)]),
        true,
    );
    assert_eq!(app.display_headers().to_vec(), ["NAME", "COUNT", "RATIO"]);
    sort_ascending(&mut app, "COUNT");
    assert_eq!(row_names(&app), ["small", "large", "missing"]);
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(
        app.display_headers().to_vec(),
        ["NAME", "COUNT", "RATIO", "DETAIL"]
    );
    assert_eq!(app.snapshot_table().1[0][3], "server detail");
    assert_eq!(app.snapshot_table().1[2][1], "<none>");

    set_filter(&mut app, "count>5");
    assert_eq!(row_names(&app), ["large"]);
    set_filter(&mut app, "ratio>1");
    assert!(
        app.rows().is_empty(),
        "string columns must not use leading-number comparisons"
    );
    set_filter(&mut app, "server detail");
    assert_eq!(app.rows().len(), 3);
    set_filter(&mut app, "count=2");
    let fields = app.selected_row_fields();
    assert!(fields.contains(&("COUNT".into(), "2".into())));
    assert!(fields.contains(&("DETAIL".into(), "server detail".into())));
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    let yaml = app
        .detail
        .lines
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(yaml.contains("full object data"));
    assert!(!yaml.contains("server detail"));
    assert!(!yaml.contains("columnDefinitions"));
}

#[tokio::test]
async fn cells_follow_object_versions_and_clear_after_deletion() {
    let (mut app, _rx) = test_app();
    open_widgets(&mut app);
    let old = resource("one", "1", "uid-1");
    apply(&mut app, old.clone());
    deliver(
        &mut app,
        table(std::slice::from_ref(&old), &[json!(10)]),
        true,
    );
    set_filter(&mut app, "count>5");
    assert_eq!(row_names(&app), ["one"]);

    let updated = resource("one", "2", "uid-1");
    apply(&mut app, updated.clone());
    assert!(app.rows().is_empty());
    deliver(
        &mut app,
        table(std::slice::from_ref(&old), &[json!(100)]),
        true,
    );
    assert!(
        app.rows().is_empty(),
        "a late Table snapshot must not restore old cells"
    );
    deliver(
        &mut app,
        table(std::slice::from_ref(&updated), &[json!(20)]),
        false,
    );
    assert_eq!(row_names(&app), ["one"]);

    let replacement = resource("one", "3", "uid-2");
    deliver(
        &mut app,
        table(std::slice::from_ref(&replacement), &[json!(30)]),
        false,
    );
    assert!(app.rows().is_empty());
    app.handle_msg(Msg::Deleted {
        generation: app.generation,
        key: "default/one".into(),
    });
    apply(&mut app, replacement.clone());
    assert_eq!(
        app.snapshot_table().1[0][1],
        "30",
        "an old delete must keep the new object's cells"
    );

    let deleted = Table::decode(table(std::slice::from_ref(&updated), &[json!(20)]), &[]).unwrap();
    app.handle_msg(Msg::ServerTable {
        generation: app.generation,
        resource: app.kind.as_ref().unwrap().resource_key(),
        update: Update::Delete(deleted),
    });
    assert_eq!(app.snapshot_table().1[0][1], "30");
    app.handle_msg(Msg::Deleted {
        generation: app.generation,
        key: "default/one".into(),
    });
    assert!(app.rows().is_empty());
    assert!(app.server_table.cells(&obj(replacement)).is_none());
}

#[tokio::test]
async fn periodic_cells_and_watch_resets_invalidate_cached_values() {
    let (mut app, _rx) = test_app();
    open_widgets(&mut app);
    let first = resource("one", "1", "a");
    let second = resource("two", "1", "b");
    apply(&mut app, first.clone());
    apply(&mut app, second.clone());
    app.handle_msg(Msg::Synced {
        generation: app.generation,
    });
    deliver(
        &mut app,
        table(&[first.clone(), second.clone()], &[json!(2), json!(10)]),
        true,
    );
    sort_ascending(&mut app, "COUNT");
    assert_eq!(row_names(&app), ["one", "two"]);
    deliver(
        &mut app,
        table(&[first.clone(), second.clone()], &[json!(20), json!(10)]),
        true,
    );
    assert_eq!(
        row_names(&app),
        ["two", "one"],
        "cells can change without a resource version change"
    );
    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    apply(&mut app, first.clone());
    app.handle_msg(Msg::Synced {
        generation: app.generation,
    });
    deliver(&mut app, table(&[first], &[json!(4)]), true);
    assert_eq!(row_names(&app), ["one"]);
    assert_eq!(app.snapshot_table().1[0][1], "4");
    assert!(app.server_table.cells(&obj(second)).is_none());

    app.handle_msg(Msg::ServerTableError {
        generation: app.generation,
        error: "Table request failed".into(),
    });
    assert_eq!(app.snapshot_table().1[0][1], "<none>");
    app.handle_msg(Msg::ServerTable {
        generation: app.generation,
        resource: app.kind.as_ref().unwrap().resource_key(),
        update: Update::Unavailable,
    });
    assert_eq!(app.display_headers().to_vec(), ["NAME", "AGE"]);
}

#[tokio::test]
async fn namespace_and_resource_changes_discard_old_table_results() {
    let (mut app, _rx) = test_app();
    open_widgets(&mut app);
    let object = resource("one", "1", "a");
    apply(&mut app, object.clone());
    deliver(
        &mut app,
        table(std::slice::from_ref(&object), &[json!(2)]),
        true,
    );
    let old_generation = app.generation;
    let old_resource = app.kind.as_ref().unwrap().resource_key();
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert!(app.namespace.is_empty());
    assert_eq!(app.display_headers().to_vec(), ["NAMESPACE", "NAME", "AGE"]);
    app.handle_msg(Msg::ServerTable {
        generation: old_generation,
        resource: old_resource.clone(),
        update: Update::Replace(
            Table::decode(table(std::slice::from_ref(&object), &[json!(2)]), &[]).unwrap(),
        ),
    });
    assert!(app.server_table.columns.is_empty());
    for resource in [
        GroupVersionResource::gvr("other.example.com", "v1", "widgets"),
        GroupVersionResource::gvr("example.com", "v2", "widgets"),
    ] {
        app.handle_msg(Msg::ServerTable {
            generation: app.generation,
            resource,
            update: Update::Replace(
                Table::decode(table(std::slice::from_ref(&object), &[json!(2)]), &[]).unwrap(),
            ),
        });
        assert!(app.server_table.columns.is_empty());
    }
    palette(&mut app, "pods");
    app.handle_msg(Msg::ServerTable {
        generation: app.generation,
        resource: old_resource,
        update: Update::Replace(Table::decode(table(&[object], &[json!(2)]), &[]).unwrap()),
    });
    assert!(app.display_headers().contains(&"RESTARTS".into()));
    assert!(!app.display_headers().contains(&"COUNT".into()));
}

#[tokio::test]
async fn explicit_and_crd_views_keep_priority_over_server_tables() {
    for explicit in [false, true] {
        let (mut app, _rx) = test_app();
        app.cluster
            .register_kind("example.com", "Widget", "widgets", true);
        let config = "[[views.widgets.columns]]\nname = 'LOCAL'\npath = '/spec/detail'\n";
        if explicit {
            install_views(&mut app, config);
        } else {
            let cfg: crate::config::Config = toml::from_str(config).unwrap();
            let (views, _) = crate::views::compile(&cfg.views);
            app.crd_views.insert(
                app.cluster.resolve("widgets").unwrap().resource_key(),
                views.get("widgets").cloned(),
            );
        }
        palette(&mut app, "widgets");
        assert!(!app.server_table_started);
        let object = resource("one", "1", "a");
        apply(&mut app, object.clone());
        deliver(&mut app, table(&[object], &[json!(2)]), true);
        assert_eq!(app.display_headers().to_vec(), ["NAME", "LOCAL", "AGE"]);
        assert_eq!(app.snapshot_table().1[0][1], "full object data");
    }
}

#[tokio::test]
async fn context_changes_stop_the_old_feed_and_discard_its_cells() {
    let (mut app, _rx) = test_app();
    open_widgets(&mut app);
    let object = resource("one", "1", "a");
    apply(&mut app, object.clone());
    deliver(
        &mut app,
        table(std::slice::from_ref(&object), &[json!(2)]),
        true,
    );
    let old_generation = app.generation;
    let old_resource = app.kind.as_ref().unwrap().resource_key();
    let old_tasks: Vec<_> = app.tasks.iter().map(|t| t.abort_handle()).collect();
    pick_context(&mut app, "west");
    let mut cluster = Cluster::fake();
    cluster.context = "west".into();
    cluster.register_kind("example.com", "Widget", "widgets", true);
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: "west".into(),
        result: Ok(Box::new(cluster)),
    });
    tokio::task::yield_now().await;
    assert!(old_tasks.iter().all(|t| t.is_finished()));
    assert_eq!(app.cluster.context, "west");
    assert!(app.server_table.columns.is_empty());
    palette(&mut app, "widgets.example.com");
    apply(&mut app, object.clone());
    app.handle_msg(Msg::ServerTable {
        generation: old_generation,
        resource: old_resource,
        update: Update::Replace(
            Table::decode(table(std::slice::from_ref(&object), &[json!(2)]), &[]).unwrap(),
        ),
    });
    assert_eq!(app.display_headers().to_vec(), ["NAME", "AGE"]);
    deliver(&mut app, table(&[object], &[json!(9)]), true);
    assert_eq!(app.snapshot_table().1[0][1], "9");
}

#[tokio::test]
async fn sort_only_config_applies_when_server_columns_arrive() {
    let (mut app, _rx) = test_app();
    install_views(&mut app, "[views.widgets]\nsort = 'COUNT:desc'\n");
    open_widgets(&mut app);
    let objects = [resource("large", "1", "a"), resource("small", "1", "b")];
    for object in &objects {
        apply(&mut app, object.clone());
    }
    assert!(app.server_table_started);
    deliver(&mut app, table(&objects, &[json!(10), json!(2)]), true);
    assert!(app.sort_desc);
    assert_eq!(row_names(&app), ["large", "small"]);
    // Re-picking the active column inverts it.
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    for ch in "COUNT".chars() {
        app.handle_key(press(KeyCode::Char(ch))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert!(!app.sort_desc);
    assert_eq!(row_names(&app), ["small", "large"]);
}

#[tokio::test]
async fn denied_crd_read_uses_calico_server_columns() {
    let (mut app, mut rx) = test_app();
    app.cluster.register_kind(
        "projectcalico.org",
        "CalicoNodeStatus",
        "caliconodestatuses",
        false,
    );
    app.cluster.register_kind(
        "apiextensions.k8s.io",
        "CustomResourceDefinition",
        "customresourcedefinitions",
        false,
    );
    let mut kind = app.cluster.resolve("caliconodestatuses").unwrap();
    kind.ar.version = "v3".into();
    kind.ar.api_version = "projectcalico.org/v3".into();
    app.kind = Some(kind);
    app.kind_plural = "caliconodestatuses".into();
    let object = json!({"apiVersion": "projectcalico.org/v3", "kind": "CalicoNodeStatus",
        "metadata": {"name": "mystatus", "uid": "node-status", "resourceVersion": "10", "creationTimestamp": "2026-01-01T00:00:00Z"},
        "spec": {"node": "node0", "classes": ["Agent", "BGP", "Routes"], "updatePeriodSeconds": 10},
        "status": {"lastUpdated": "2026-01-01T00:00:00Z"}});
    let table = json!({"kind": "Table", "apiVersion": "meta.k8s.io/v1", "metadata": {"resourceVersion": "10"},
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name"},
            {"name": "Node", "type": "string"}, {"name": "Classes", "type": "string"},
            {"name": "Update Interval", "type": "string"}, {"name": "Age", "type": "string"},
            {"name": "last Updated", "type": "string"},
            {"name": "Agent State", "type": "string", "priority": 1},
            {"name": "ESTABLISHED-V4", "type": "string", "priority": 1},
            {"name": "ESTABLISHED-V6", "type": "string", "priority": 1},
            {"name": "NUM-V4-ROUTES", "type": "string", "priority": 1},
            {"name": "NUM-V6-ROUTES", "type": "string", "priority": 1}
        ], "rows": [{"object": {"metadata": object["metadata"]},
            "cells": ["mystatus", "node0", "Agent,BGP,Routes", "10s", "3m", "3m ago", "v4(Ready) v6(Ready)", "2/3", "1/1", "2", "1"]}]});
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let uri = request.uri().clone();
            let accept = request
                .headers()
                .get(http::header::ACCEPT)
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default()
                .to_string();
            requests.lock().unwrap().push((uri.clone(), accept.clone()));
            let object = object.clone();
            let table = table.clone();
            async move {
                let query: HashMap<_, _> =
                    form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
                        .into_owned()
                        .collect();
                let watch = query.get("watch").is_some_and(|v| v == "true");
                let (code, body) = if uri.path() == "/apis/projectcalico.org/v3/caliconodestatuses"
                {
                    if accept.contains("as=Table") {
                        (
                            200,
                            if watch {
                                String::new()
                            } else {
                                table.to_string()
                            },
                        )
                    } else if query.contains_key("sendInitialEvents") {
                        (
                            200,
                            format!(
                                "{}\n{}\n",
                                json!({"type": "ADDED", "object": object}),
                                json!({"type": "BOOKMARK", "object": {"apiVersion": "projectcalico.org/v3", "kind": "CalicoNodeStatus",
                            "metadata": {"resourceVersion": "10", "annotations": {"k8s.io/initial-events-end": "true"}}}})
                            ),
                        )
                    } else {
                        (200, String::new())
                    }
                } else {
                    (403, json!({"apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "Forbidden", "message": "read denied", "code": 403}).to_string())
                };
                let frames = stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from(body)))]);
                let frames = if watch && code == 200 {
                    frames.chain(stream::pending()).boxed()
                } else {
                    frames.boxed()
                };
                Ok::<_, Infallible>(
                    http::Response::builder()
                        .status(code)
                        .body(http_body_util::StreamBody::new(frames))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    app.handle_key(ctrl(KeyCode::Char('r'))).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !app.store.synced || !app.display_headers().contains(&"NODE".into()) {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
    let base = "2026-01-01T00:00:00Z"
        .parse::<Timestamp>()
        .unwrap()
        .as_second();
    assert_eq!(
        app.snapshot_table_at(base + 180).1[0],
        [
            "mystatus",
            "node0",
            "Agent,BGP,Routes",
            "10s",
            "3m",
            "3m ago"
        ]
    );
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(
        &app.snapshot_table_at(base + 240).1[0][4..],
        [
            "4m",
            "3m ago",
            "v4(Ready) v6(Ready)",
            "2/3",
            "1/1",
            "2",
            "1"
        ]
    );
    assert_eq!(
        app.crd_views[&app.kind.as_ref().unwrap().resource_key()]
            .as_ref()
            .map(|v| v.columns.len()),
        None
    );
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|(uri, _)| {
        uri.path()
            .ends_with("customresourcedefinitions/caliconodestatuses.projectcalico.org")
    }));
    assert!(seen.iter().any(|(uri, accept)| uri.path()
        == "/apis/projectcalico.org/v3/caliconodestatuses"
        && accept.contains("as=Table")));
}
