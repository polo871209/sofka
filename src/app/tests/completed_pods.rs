use super::*;

fn phase_pod(name: &str, phase: &str, rv: &str) -> serde_json::Value {
    json!({"apiVersion": "v1", "kind": "Pod",
           "metadata": {"name": name, "namespace": "default", "uid": name,
                        "resourceVersion": rv},
           "status": {"phase": phase}})
}

#[tokio::test]
async fn completed_pods_are_hidden_until_h_shows_them() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(&mut app, phase_pod("done", "Succeeded", "1"));
    apply(&mut app, phase_pod("failed", "Failed", "1"));
    apply(&mut app, phase_pod("web", "Running", "1"));
    assert_eq!(row_names(&app), ["failed", "web"]);
    assert!(screen_text(&mut app, 160, 30).contains("h show completed"));

    app.handle_key(press(KeyCode::Down)).unwrap();
    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    assert_eq!(row_names(&app), ["failed", "web", "done"]);
    assert_eq!(
        app.selected_ref().unwrap().metadata.name.as_deref(),
        Some("web")
    );
    assert!(screen_text(&mut app, 160, 30).contains("h hide completed"));

    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    assert_eq!(row_names(&app), ["failed", "web"]);
}

#[tokio::test]
async fn a_pod_that_completes_leaves_the_list() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(&mut app, phase_pod("job", "Running", "1"));
    apply(&mut app, phase_pod("web", "Running", "1"));
    assert_eq!(row_names(&app), ["job", "web"]);

    apply(&mut app, phase_pod("job", "Succeeded", "2"));
    assert_eq!(row_names(&app), ["web"]);
}

#[tokio::test]
async fn drilled_pod_lists_show_completed_pods() {
    let (mut app, _rx) = test_app();
    app.switch_kind("jobs");
    apply(
        &mut app,
        json!({"apiVersion": "batch/v1", "kind": "Job",
               "metadata": {"name": "backup", "namespace": "default"},
               "spec": {"selector": {"matchLabels": {"job-name": "backup"}}}}),
    );
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    apply(&mut app, phase_pod("backup-x", "Succeeded", "1"));
    assert_eq!(row_names(&app), ["backup-x"]);
    assert!(!screen_text(&mut app, 160, 30).contains("show completed"));

    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    assert_eq!(row_names(&app), ["backup-x"]);
    assert!(app.flash.contains("drilled pod lists"));
}

#[tokio::test]
async fn h_on_other_kinds_leaves_the_key_to_bookmarks() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    assert!(app.flash.contains("applies to pods"));

    app.bookmarks = vec![crate::config::Bookmark {
        key: Some("h".into()),
        name: "deploys".into(),
        resource: "deployments".into(),
        ..Default::default()
    }];
    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert!(!app.show_completed);
}
