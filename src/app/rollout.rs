use super::*;

impl App {
    /// `ctrl-u` on a workload: list the revisions it keeps, newest first.
    pub(super) fn open_rollout_history(&mut self) {
        let (Some(history), Some(owner_kind)) = (
            crate::rollout::history_plural(&self.kind_plural),
            crate::rollout::owner_kind(&self.kind_plural),
        ) else {
            self.flash_warn("rollout undo applies to deployments, statefulsets, and daemonsets");
            return;
        };
        let Some(obj) = self.selected() else {
            return;
        };
        let Some(kind) = self.cluster.resolve(history) else {
            self.flash_warn(&format!("{history} kind unavailable"));
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let scope = format!("{}/{name}", trim_s(&self.kind_plural));
        self.push_frame();
        self.kind = Some(kind);
        self.kind_plural = "rollouthistory".into();
        self.namespace = ns;
        // Revisions carry the pod labels, so the selector narrows the watch
        // server-side and the owner scope keeps only this workload's rows.
        self.labels = label_selector(&obj, "matchLabels");
        self.fields = None;
        self.owner = Some(OwnerScope {
            kind: owner_kind.into(),
            name: name.clone(),
            uid: obj.metadata.uid.clone(),
        });
        self.scope_label = Some(scope);
        self.retain_filter_selectors();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {name} rollout history");
        self.flash_err = false;
        self.start_watch();
    }

    /// The owned revision with the highest number. The controllers renumber a
    /// revision when a rollback reuses it, so this is always the deployed one.
    fn rollout_current(&self) -> Option<&DynamicObject> {
        let owner = self.owner.as_ref()?;
        self.store
            .iter()
            .map(|(_, o)| o)
            .filter(|o| owner.owns(o))
            .filter_map(|o| crate::rollout::revision(o).map(|r| (r, o)))
            .max_by_key(|(r, _)| *r)
            .map(|(_, o)| o)
    }

    /// Keep the STATUS column in step with the deployed revision. The cell
    /// cache is keyed on each row's own resourceVersion, which does not move
    /// when another row becomes the deployed one.
    pub(super) fn sync_rollout_current(&mut self) {
        let current = if self.kind_plural == "rollouthistory" {
            self.rollout_current().and_then(crate::rollout::revision)
        } else {
            None
        };
        if self.spec.rollout_current() != current {
            self.spec.set_rollout_current(current);
            self.clear_rows_cache();
        }
    }

    /// The rollback to `obj`'s revision, or `None` after a flash.
    fn prepare_rollout_undo(&mut self, obj: &DynamicObject) -> Option<RolloutUndo> {
        let owner = self.owner.clone()?;
        let Some(kind) =
            crate::rollout::workload_plural(&owner.kind).and_then(|p| self.cluster.resolve(p))
        else {
            self.flash_warn(&format!("{} kind unavailable", owner.kind));
            return None;
        };
        let Some(revision) = crate::rollout::revision(obj) else {
            self.flash_warn("could not determine this revision's number");
            return None;
        };
        let Some(patch) = crate::rollout::undo_patch(obj) else {
            self.flash_warn("this revision has no pod template");
            return None;
        };
        if self
            .rollout_current()
            .and_then(crate::rollout::revision)
            .is_some_and(|current| current == revision)
        {
            self.set_flash(format!("revision {revision} is already deployed"));
            return None;
        }
        Some(RolloutUndo {
            kind,
            ns: obj.metadata.namespace.clone().unwrap_or_default(),
            name: owner.name,
            revision,
            patch,
        })
    }

    /// `enter` on a revision: diff the deployed pod template against it.
    /// `enter` in the diff then asks to roll back.
    pub(super) fn open_rollout_diff(&mut self, obj: &DynamicObject) {
        self.set_return_mode();
        let Some(undo) = self.prepare_rollout_undo(obj) else {
            return;
        };
        let (current_rev, before) = match self.rollout_current() {
            Some(current) => (
                crate::rollout::revision(current).unwrap_or_default(),
                crate::rollout::template(current)
                    .map(|t| crate::rollout::template_yaml(&t))
                    .unwrap_or_default(),
            ),
            None => (0, String::new()),
        };
        let after = crate::rollout::template(obj)
            .map(|t| crate::rollout::template_yaml(&t))
            .unwrap_or_default();
        let lines = details::diff_lines(&before, &after);
        if lines.iter().all(|l| l.starts_with(' ')) {
            self.set_flash(format!(
                "no diff: revision {} matches the deployed template",
                undo.revision
            ));
        }
        let accept = self.keymap.first_label("diff", Action::Accept).to_string();
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!(
                "{} — rollback diff (deployed v{current_rev} → v{}) · {accept} roll back",
                undo.name, undo.revision
            ),
            lines: lines.into(),
            ..Default::default()
        };
        self.rollout_diff = Some(undo);
        self.mode = Mode::Diff;
    }

    /// Whether `enter` in the open diff rolls back.
    pub fn rollout_diff_active(&self) -> bool {
        self.mode == Mode::Diff && self.rollout_diff.is_some()
    }

    /// `enter` in a rollout diff.
    pub(super) fn confirm_rollout_diff(&mut self) {
        if let Some(undo) = self.rollout_diff.clone() {
            self.request_rollout_undo(undo);
        }
    }

    /// `r` on a revision: roll back without the diff, like Helm history.
    pub(super) fn request_rollout_undo_selected(&mut self) {
        let Some(obj) = self.selected() else {
            return;
        };
        if let Some(undo) = self.prepare_rollout_undo(&obj) {
            self.request_rollout_undo(undo);
        }
    }

    fn request_rollout_undo(&mut self, undo: RolloutUndo) {
        if self.deny_readonly() {
            return;
        }
        let targets = vec![(undo.name.clone(), undo.ns.clone())];
        let plural = undo.kind.ar.plural.clone();
        let Some(level) = self.guard("rollback", &plural, &targets, ConfirmLevel::Plain) else {
            return;
        };
        let label = format!(
            "Roll back {} {} in {} to revision {}?",
            trim_s(&plural),
            undo.name,
            undo.ns,
            undo.revision
        );
        let name_hint = undo.name.clone();
        self.begin_guarded(ConfirmAction::RolloutUndo(undo), label, level, name_hint);
    }

    pub(super) fn do_rollout_undo(&mut self, undo: RolloutUndo) {
        let RolloutUndo {
            kind,
            ns,
            name,
            revision,
            patch,
        } = undo;
        self.note_action(
            format!("rollout undo to {revision}"),
            format!("{name} in {ns}"),
        );
        let claim = self.claim_status(format!("rolling back {name} to revision {revision}…"));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.cluster.client.clone(), &ns, &kind.ar);
        let tx = self.tx.clone();
        let genr = self.generation;
        // The history watch shows the renumbered revision on its own.
        tokio::spawn(async move {
            let (message, err) = match rollout_undo(&api, &name, &patch).await {
                Ok(true) => (format!("rolled back {name} to revision {revision}"), false),
                Ok(false) => (
                    format!("skipped: {name} already runs the template of revision {revision}"),
                    false,
                ),
                Err(e) => (format!("rollout undo {name} failed: {e}"), true),
            };
            let _ = tx
                .send(Msg::Flash {
                    generation: genr,
                    claim,
                    message,
                    err,
                })
                .await;
        });
    }
}

/// Patch the workload unless it is paused or already runs the template.
/// `Ok(false)` means there was nothing to change.
async fn rollout_undo(api: &Api<DynamicObject>, name: &str, patch: &Value) -> Result<bool> {
    let live = api.get(name).await?;
    // kubectl refuses too: a paused Deployment takes the template but does
    // not roll it out, so the undo would look applied and not be.
    if live.data.pointer("/spec/paused") == Some(&Value::Bool(true)) {
        anyhow::bail!("the deployment is paused; resume it first");
    }
    let mut target = patch["spec"]["template"].clone();
    if let Some(t) = target.as_object_mut() {
        t.remove("$patch");
    }
    if crate::rollout::workload_template(&live.data).as_ref() == Some(&target) {
        return Ok(false);
    }
    api.patch(name, &PatchParams::default(), &Patch::Strategic(patch))
        .await?;
    Ok(true)
}
