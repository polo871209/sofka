//! Rollout history of Deployments, StatefulSets, and DaemonSets.
//!
//! A Deployment keeps each revision as an owned ReplicaSet with the
//! `deployment.kubernetes.io/revision` annotation. StatefulSets and DaemonSets
//! keep each revision as an owned ControllerRevision whose `data` holds the
//! pod template. Undo writes the chosen revision's pod template back to the
//! workload, as `kubectl rollout undo --to-revision` does.

use kube::core::DynamicObject;
use serde_json::{Map, Value, json};

/// The kind that stores revisions for a workload plural.
pub fn history_plural(workload_plural: &str) -> Option<&'static str> {
    match workload_plural {
        "deployments" => Some("replicasets"),
        "statefulsets" | "daemonsets" => Some("controllerrevisions"),
        _ => None,
    }
}

/// The workload kind that owns a revision, as it appears in `ownerReferences`.
pub fn owner_kind(workload_plural: &str) -> Option<&'static str> {
    match workload_plural {
        "deployments" => Some("Deployment"),
        "statefulsets" => Some("StatefulSet"),
        "daemonsets" => Some("DaemonSet"),
        _ => None,
    }
}

/// The workload plural for an owner kind.
pub fn workload_plural(owner_kind: &str) -> Option<&'static str> {
    match owner_kind {
        "Deployment" => Some("deployments"),
        "StatefulSet" => Some("statefulsets"),
        "DaemonSet" => Some("daemonsets"),
        _ => None,
    }
}

fn is_controller_revision(obj: &DynamicObject) -> bool {
    obj.data.get("revision").is_some() && obj.data.get("spec").is_none()
}

/// The revision number of a ReplicaSet or ControllerRevision.
pub fn revision(obj: &DynamicObject) -> Option<i64> {
    if is_controller_revision(obj) {
        return obj.data.get("revision")?.as_i64();
    }
    obj.metadata
        .annotations
        .as_ref()?
        .get("deployment.kubernetes.io/revision")?
        .parse()
        .ok()
}

/// The pod template of a revision, without the fields that differ between
/// revisions of one template: the ReplicaSet `pod-template-hash` label, the
/// ControllerRevision `$patch` directive, and an empty creation timestamp.
pub fn template(obj: &DynamicObject) -> Option<Value> {
    let pointer = if is_controller_revision(obj) {
        "/data/spec/template"
    } else {
        "/spec/template"
    };
    let mut template = obj.data.pointer(pointer)?.as_object()?.clone();
    template.remove("$patch");
    if let Some(meta) = template.get_mut("metadata").and_then(Value::as_object_mut) {
        if meta.get("creationTimestamp").is_some_and(Value::is_null) {
            meta.remove("creationTimestamp");
        }
        if let Some(labels) = meta.get_mut("labels").and_then(Value::as_object_mut) {
            labels.remove("pod-template-hash");
        }
    }
    Some(Value::Object(template))
}

/// The pod template of a live workload, cleaned like [`template`].
pub fn workload_template(workload: &Value) -> Option<Value> {
    let mut template = workload.pointer("/spec/template")?.as_object()?.clone();
    if let Some(meta) = template.get_mut("metadata").and_then(Value::as_object_mut)
        && meta.get("creationTimestamp").is_some_and(Value::is_null)
    {
        meta.remove("creationTimestamp");
    }
    Some(Value::Object(template))
}

/// Container images of a revision's pod template, in container order.
pub fn images(obj: &DynamicObject) -> String {
    template(obj)
        .as_ref()
        .and_then(|t| t.pointer("/spec/containers"))
        .and_then(Value::as_array)
        .map(|containers| {
            containers
                .iter()
                .filter_map(|c| c.get("image").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

pub fn change_cause(obj: &DynamicObject) -> Option<&str> {
    obj.metadata
        .annotations
        .as_ref()?
        .get("kubernetes.io/change-cause")
        .map(String::as_str)
}

/// The creation time of a revision in UTC, `YYYY-MM-DD HH:MM:SS`.
pub fn created(obj: &DynamicObject) -> String {
    obj.metadata
        .creation_timestamp
        .as_ref()
        .map(|ts| ts.0.strftime("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// The strategic merge patch that sets a workload's pod template to this
/// revision. `$patch: replace` drops fields that the revision does not have,
/// which a plain merge would keep.
pub fn undo_patch(obj: &DynamicObject) -> Option<Value> {
    let Value::Object(mut template) = template(obj)? else {
        return None;
    };
    template.insert("$patch".into(), json!("replace"));
    let mut patch = json!({"spec": {"template": Value::Object(template)}});
    if !is_controller_revision(obj)
        && let Some(cause) = change_cause(obj)
    {
        let mut annotations = Map::new();
        annotations.insert("kubernetes.io/change-cause".into(), json!(cause));
        patch["metadata"] = json!({"annotations": annotations});
    }
    Some(patch)
}

/// YAML of a pod template, for the rollback diff.
pub fn template_yaml(template: &Value) -> String {
    serde_yaml::to_string(template).unwrap_or_else(|e| format!("# error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn replicaset(revision: &str, image: &str) -> DynamicObject {
        obj(json!({
            "apiVersion": "apps/v1", "kind": "ReplicaSet",
            "metadata": {"name": "web-abc", "namespace": "a",
                "creationTimestamp": "2024-01-15T10:30:00Z",
                "annotations": {"deployment.kubernetes.io/revision": revision,
                    "kubernetes.io/change-cause": "bump"}},
            "spec": {"template": {
                "metadata": {"creationTimestamp": null,
                    "labels": {"app": "web", "pod-template-hash": "abc"}},
                "spec": {"containers": [
                    {"name": "web", "image": image},
                    {"name": "sidecar", "image": "envoy:1"}]}}}
        }))
    }

    fn controller_revision(revision: i64) -> DynamicObject {
        obj(json!({
            "apiVersion": "apps/v1", "kind": "ControllerRevision",
            "metadata": {"name": "db-7f", "namespace": "a"},
            "revision": revision,
            "data": {"spec": {"template": {"$patch": "replace",
                "metadata": {"labels": {"app": "db"}},
                "spec": {"containers": [{"name": "db", "image": "postgres:16"}]}}}}
        }))
    }

    #[test]
    fn replicaset_revision_template_and_images() {
        let rs = replicaset("4", "nginx:1.25");
        assert_eq!(revision(&rs), Some(4));
        assert_eq!(images(&rs), "nginx:1.25, envoy:1");
        assert_eq!(change_cause(&rs), Some("bump"));
        assert_eq!(created(&rs), "2024-01-15 10:30:00");
        let t = template(&rs).unwrap();
        assert_eq!(t["metadata"], json!({"labels": {"app": "web"}}));
    }

    #[test]
    fn controller_revision_revision_template_and_images() {
        let cr = controller_revision(3);
        assert_eq!(revision(&cr), Some(3));
        assert_eq!(images(&cr), "postgres:16");
        let t = template(&cr).unwrap();
        assert!(t.get("$patch").is_none());
        assert_eq!(t["metadata"]["labels"]["app"], "db");
    }

    #[test]
    fn undo_patch_replaces_the_template() {
        let patch = undo_patch(&replicaset("2", "nginx:1.24")).unwrap();
        assert_eq!(patch["spec"]["template"]["$patch"], "replace");
        assert_eq!(
            patch["spec"]["template"]["spec"]["containers"][0]["image"],
            "nginx:1.24"
        );
        assert!(
            patch["spec"]["template"]["metadata"]["labels"]
                .get("pod-template-hash")
                .is_none()
        );
        assert_eq!(
            patch["metadata"]["annotations"]["kubernetes.io/change-cause"],
            "bump"
        );
        let patch = undo_patch(&controller_revision(1)).unwrap();
        assert_eq!(patch["spec"]["template"]["$patch"], "replace");
        assert!(patch.get("metadata").is_none());
    }

    #[test]
    fn workload_template_matches_its_own_revision() {
        let rs = replicaset("1", "nginx:1.25");
        let workload = json!({"spec": {"template": template(&rs).unwrap()}});
        assert_eq!(workload_template(&workload), template(&rs));
    }
}
