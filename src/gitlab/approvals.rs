//! Reversible approval overrides, journaled in a note owned by the token user.
//!
//! GitLab has no documented atomic approval-rule transaction. Persist recovery
//! data first, keep auto-merge cancelled until every write is verified, and
//! compensate on refusal/error. Recovery runs before repository code executes.
use super::*;
use serde::{Deserialize, Serialize};

const JOURNAL: &str = "upd approval override recovery v1\n";

/// Stable machine-readable result, with explanatory text for the run log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApprovalStatus {
    pub status: ApprovalKind,
    pub detail: String,
    #[serde(skip)]
    auto_merge_cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Overridden,
    Retained,
    AlreadyZero,
    PolicyDenied,
    Unavailable,
    RolledBack,
    Restored,
    RecoveryPending,
}

impl ApprovalStatus {
    fn new(status: ApprovalKind, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            auto_merge_cancelled: matches!(
                status,
                ApprovalKind::Restored | ApprovalKind::RolledBack | ApprovalKind::RecoveryPending
            ),
        }
    }
    fn cancelled(mut self) -> Self {
        self.auto_merge_cancelled = true;
        self
    }
    pub fn needs_rearm(&self) -> bool {
        self.auto_merge_cancelled
    }
    pub fn armed(&self) -> bool {
        matches!(
            self.status,
            ApprovalKind::Overridden | ApprovalKind::Retained
        )
    }
    pub fn recovery_pending(&self) -> bool {
        self.status == ApprovalKind::RecoveryPending
    }
}
impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.detail.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: u64,
    name: String,
    rule_type: String,
    approvals_required: u64,
    user_ids: Vec<u64>,
    group_ids: Vec<u64>,
    source_required: Option<u64>,
    project_rule_id: Option<u64>,
}

impl Rule {
    fn read(value: &Value) -> Result<Self, Error> {
        let required = |key: &str| {
            value[key]
                .as_u64()
                .ok_or_else(|| Error::Refused(format!("GitLab approval rule has no {key}")))
        };
        let ids = |key: &str| -> Result<Vec<u64>, Error> {
            let mut ids = value[key]
                .as_array()
                .ok_or_else(|| Error::Refused(format!("GitLab approval rule has no {key}")))?
                .iter()
                .map(|v| {
                    v["id"]
                        .as_u64()
                        .ok_or_else(|| Error::Refused("GitLab approver has no id".into()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            ids.sort_unstable();
            ids.dedup();
            Ok(ids)
        };
        Ok(Self {
            id: required("id")?,
            name: value["name"]
                .as_str()
                .ok_or_else(|| Error::Refused("GitLab approval rule has no name".into()))?
                .into(),
            rule_type: value["rule_type"]
                .as_str()
                .ok_or_else(|| Error::Refused("GitLab approval rule has no type".into()))?
                .into(),
            approvals_required: required("approvals_required")?,
            user_ids: ids("users")?,
            group_ids: ids("groups")?,
            source_required: if value["source_rule"].is_null() {
                None
            } else {
                Some(
                    value["source_rule"]["approvals_required"]
                        .as_u64()
                        .ok_or_else(|| {
                            Error::Refused("GitLab source rule has no approval requirement".into())
                        })?,
                )
            },
            project_rule_id: None,
        })
    }

    fn same_definition(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.rule_type == other.rule_type
            && self.approvals_required == other.approvals_required
            && self.user_ids == other.user_ids
            && self.group_ids == other.group_ids
            && self.source_required.is_some() == other.source_required.is_some()
    }

    fn body(&self) -> Value {
        json!({"name": self.name, "approvals_required": self.approvals_required,
            "user_ids": self.user_ids, "group_ids": self.group_ids, "remove_hidden_groups": false})
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    project: u64,
    iid: u64,
    branch: String,
    actor: u64,
    active: bool,
    native_before: bool,
    rules: Vec<Rule>,
}

struct Note {
    id: u64,
    journal: Journal,
}

fn editable(value: &Value) -> bool {
    matches!(
        value["rule_type"].as_str(),
        Some("regular" | "any_approver")
    ) && value["section"].is_null()
}

fn unavailable(message: &str) -> Error {
    Error::Refused(format!(
        "{message}; auto-merge remains cancelled and the approval recovery note is retained"
    ))
}

impl Client {
    fn approval_project_id(&self) -> Result<u64, Error> {
        self.approval_project()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| Error::Refused("Approval overrides require a numeric project ID".into()))
    }

    fn approval_project(&self) -> &str {
        self.merge_requests.trim_end_matches("/merge_requests")
    }

    async fn actor(&self) -> Result<u64, Error> {
        self.send(
            Method::GET,
            self.url(&format!("{}/user", self.api_url))?,
            None,
        )
        .await?["id"]
            .as_u64()
            .ok_or_else(|| Error::Refused("GitLab token user has no id".into()))
    }

    fn approval_scope(&self, mr: &Value, branch: &str, actor: u64) -> Result<(), Error> {
        let project = self.approval_project().rsplit('/').next().unwrap_or("");
        let same = |key: &str| mr[key].as_u64().is_some_and(|id| id.to_string() == project);
        if mr["source_branch"].as_str() != Some(branch)
            || !same("source_project_id")
            || !same("target_project_id")
            || mr["author"]["id"].as_u64() != Some(actor)
        {
            return Err(Error::Refused("Approval overrides require the token user's merge request from upd's source branch in the same project".into()));
        }
        Ok(())
    }

    async fn approval_notes(&self, iid: u64, branch: &str, actor: u64) -> Result<Vec<Note>, Error> {
        let notes = self
            .every_page(
                self.merge_request_url(&format!("{iid}/notes"))?,
                "approval recovery notes",
            )
            .await?;
        self.decode_approval_notes(notes, iid, branch, actor)
    }

    fn decode_approval_notes(
        &self,
        notes: Vec<Value>,
        iid: u64,
        branch: &str,
        actor: u64,
    ) -> Result<Vec<Note>, Error> {
        let mut journals = Vec::new();
        for note in notes {
            // A matching string from another user is not recovery authority.
            if note["author"]["id"].as_u64() != Some(actor) {
                continue;
            }
            let Some(body) = note["body"].as_str().and_then(|s| s.strip_prefix(JOURNAL)) else {
                continue;
            };
            let journal: Journal = serde_json::from_str(body)
                .map_err(|_| unavailable("Malformed bot-owned approval recovery note"))?;
            if journal.project != self.approval_project_id()?
                || journal.iid != iid
                || journal.branch != branch
                || journal.actor != actor
            {
                return Err(unavailable(
                    "Approval recovery note has a different project, MR, branch, or actor",
                ));
            }
            let id = note["id"]
                .as_u64()
                .ok_or_else(|| unavailable("Approval recovery note has no id"))?;
            journals.push(Note { id, journal });
        }
        Ok(journals)
    }

    async fn save_approval_note(&self, iid: u64, note: &mut Note) -> Result<bool, Error> {
        let body = json!({"body": format!("{JOURNAL}{}", serde_json::to_string(&note.journal).map_err(|e| Error::Io(e.to_string()))?), "internal": true});
        let value = if note.id == 0 {
            self.approval_request(
                Method::POST,
                self.merge_request_url(&format!("{iid}/notes"))?,
                Some(&body),
            )
            .await?
        } else {
            self.approval_request(
                Method::PUT,
                self.merge_request_url(&format!("{iid}/notes/{}", note.id))?,
                Some(&body),
            )
            .await?
        };
        let Some(value) = value else {
            return Ok(false);
        };
        // A lost or filtered journal write must never be followed by a rule write.
        if value["body"] != body["body"]
            || value["author"]["id"].as_u64() != Some(note.journal.actor)
        {
            return Err(unavailable(
                "GitLab did not persist the approval recovery note",
            ));
        }
        note.id = value["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| unavailable("Approval recovery note has no id"))?;
        Ok(true)
    }

    async fn mr_approval_rules(&self, iid: u64) -> Result<Vec<Value>, Error> {
        self.every_page(
            self.merge_request_url(&format!("{iid}/approval_rules"))?,
            "merge request approval rules",
        )
        .await
    }

    async fn approval_guard<T>(&self, iid: u64, result: Result<T, Error>) -> Result<T, Error> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let cancellation = self.cancel_auto_merge(iid).await;
                Err(Error::Refused(format!(
                    "Approval verification failed: {error}; auto-merge cancellation: {cancellation:?}; recovery journal retained"
                )))
            }
        }
    }

    async fn snapshot_is_zero(&self, iid: u64, note: &Note) -> Result<bool, Error> {
        let raw = self.mr_approval_rules(iid).await?;
        let editable_rules = raw.iter().filter(|r| editable(r)).collect::<Vec<_>>();
        if editable_rules.len() != note.journal.rules.len() || note.journal.rules.is_empty() {
            return Ok(false);
        }
        for original in &note.journal.rules {
            let Some(raw) = editable_rules.iter().find(|r| {
                r["id"].as_u64() == Some(original.id)
                    || (original.id == 0
                        && r["name"] == original.name
                        && r["rule_type"] == original.rule_type)
            }) else {
                return Ok(false);
            };
            if raw["contains_hidden_groups"] != false {
                return Ok(false);
            }
            let actual = Rule::read(raw)?;
            let mut expected = original.clone();
            expected.id = actual.id;
            expected.approvals_required = 0;
            if !actual.same_definition(&expected) {
                return Ok(false);
            }
        }
        self.effective_approvals_zero(iid).await
    }

    async fn effective_approvals_zero(&self, iid: u64) -> Result<bool, Error> {
        let state = self.approval_state(iid).await?;
        let rules = state["rules"]
            .as_array()
            .ok_or_else(|| unavailable("GitLab approval state has no rules"))?;
        Ok(rules
            .iter()
            .all(|r| r["approvals_required"].as_u64() == Some(0)))
    }

    async fn approval_state(&self, iid: u64) -> Result<Value, Error> {
        self.send(
            Method::GET,
            self.merge_request_url(&format!("{iid}/approval_state"))?,
            None,
        )
        .await
    }

    async fn verify_approval_note(&self, iid: u64, note: &Note) -> Result<(), Error> {
        let value = self
            .send(
                Method::GET,
                self.merge_request_url(&format!("{iid}/notes/{}", note.id))?,
                None,
            )
            .await?;
        let expected = format!(
            "{JOURNAL}{}",
            serde_json::to_string(&note.journal).map_err(|e| Error::Io(e.to_string()))?
        );
        if value["body"].as_str() != Some(&expected)
            || value["author"]["id"].as_u64() != Some(note.journal.actor)
        {
            return Err(unavailable(
                "Approval recovery journal changed during override; serialize upd runs for this project",
            ));
        }
        Ok(())
    }

    async fn put_approval_rule(&self, iid: u64, rule: &Rule) -> Result<bool, Error> {
        let url = self.merge_request_url(&format!("{iid}/approval_rules/{}", rule.id))?;
        let Some(updated) = self
            .approval_request(Method::PUT, url, Some(&rule.body()))
            .await?
        else {
            return Ok(false);
        };
        if !Rule::read(&updated)?.same_definition(rule) {
            return Err(unavailable(
                "GitLab did not apply the complete approval rule",
            ));
        }
        Ok(true)
    }

    /// Removing every purely inherited MR rule restores GitLab's native project
    /// fallback (approval_rules_overwritten = false), including rules added while
    /// the override was active. Keep MR identity and all discussion intact.
    async fn restore_inherited(
        &self,
        iid: u64,
        note: &mut Note,
        raw: &[Value],
    ) -> Result<(), Error> {
        let project_rules = self
            .every_page(
                self.url(&format!("{}/approval_rules", self.approval_project()))?,
                "project approval rules",
            )
            .await?;
        let current = raw
            .iter()
            .filter(|r| matches!(r["rule_type"].as_str(), Some("regular" | "any_approver")))
            .map(Rule::read)
            .collect::<Result<Vec<_>, _>>()?;
        for rule in &current {
            let Some(saved) = note.journal.rules.iter().find(|r| {
                r.id == rule.id
                    || (r.id == 0 && r.name == rule.name && r.rule_type == rule.rule_type)
            }) else {
                return Err(unavailable(
                    "MR approval rules were added independently; manual reconciliation is required",
                ));
            };
            let limit = project_rules
                .iter()
                .find(|r| r["id"].as_u64() == saved.project_rule_id)
                .map(|r| {
                    r["approvals_required"]
                        .as_u64()
                        .ok_or_else(|| unavailable("Project approval rule has no requirement"))
                })
                .transpose()?
                .unwrap_or(0);
            let mut original = saved.clone();
            original.id = rule.id;
            original.approvals_required = rule.approvals_required;
            if !rule.same_definition(&original) || rule.approvals_required > limit {
                return Err(unavailable(
                    "MR approval rules were strengthened or edited independently; manual reconciliation is required",
                ));
            }
        }
        for rule in current {
            let url = self.merge_request_url(&format!("{iid}/approval_rules/{}", rule.id))?;
            let answer = self.execute(Method::DELETE, url, None, MAX_ANSWER).await?;
            if answer.status != StatusCode::NOT_FOUND {
                parse_json(answer)?;
            }
        }
        let state = self
            .send(
                Method::GET,
                self.merge_request_url(&format!("{iid}/approval_state"))?,
                None,
            )
            .await?;
        if state["approval_rules_overwritten"] != false {
            // A reviewer may have added an MR-only rule between the preflight
            // and deletion. Recreate project requirements alongside it rather
            // than leave that new rule as the sole effective requirement.
            let remaining = self.mr_approval_rules(iid).await?;
            for source in &project_rules {
                if !editable(source) {
                    continue;
                }
                let rule = Rule::read(source)?;
                let collision = remaining
                    .iter()
                    .any(|r| r["name"] == source["name"] && r["rule_type"] == source["rule_type"]);
                let name = if collision {
                    format!("upd recovery {} project rule {}", note.id, rule.id)
                } else {
                    rule.name.clone()
                };
                let body = json!({"name":name,"approval_project_rule_id": rule.id, "approvals_required": rule.approvals_required});
                let recreated = self
                    .send(
                        Method::POST,
                        self.merge_request_url(&format!("{iid}/approval_rules"))?,
                        Some(&body),
                    )
                    .await?;
                let recreated = Rule::read(&recreated)?;
                if recreated.approvals_required < rule.approvals_required
                    || recreated.user_ids != rule.user_ids
                    || recreated.group_ids != rule.group_ids
                {
                    return Err(unavailable(
                        "GitLab did not recreate the complete project requirement after a concurrent edit",
                    ));
                }
            }
            return Err(unavailable(
                "MR approval rules changed during restoration; project requirements were recreated and manual reconciliation is required",
            ));
        }
        note.journal.active = false;
        if !self.save_approval_note(iid, note).await? {
            return Err(unavailable(
                "GitLab refused completion of the recovery journal",
            ));
        }
        Ok(())
    }

    /// Restore one complete snapshot. Inherited rules take their current project
    /// values; MR-only rules retain their pre-override definition. Never lower a
    /// requirement raised while the override was active.
    async fn restore_approval_note(
        &self,
        iid: u64,
        note: &mut Note,
        inherited: bool,
    ) -> Result<(), Error> {
        let rules = self.mr_approval_rules(iid).await?;
        if (inherited || note.journal.native_before)
            && note
                .journal
                .rules
                .iter()
                .all(|r| r.project_rule_id.is_some())
        {
            return self.restore_inherited(iid, note, &rules).await;
        }
        for saved in &note.journal.rules {
            let Some(raw) = rules.iter().find(|r| r["id"].as_u64() == Some(saved.id)) else {
                return Err(unavailable(
                    "An overridden approval rule was deleted; restore it manually before retrying",
                ));
            };
            if !editable(raw) || raw["contains_hidden_groups"] == true {
                return Err(unavailable(
                    "Approval restoration is blocked by an uneditable or hidden rule",
                ));
            }
            let current = Rule::read(raw)?;
            let mut target = saved.clone();
            target.approvals_required = target.approvals_required.max(current.approvals_required);
            if current.same_definition(&target) {
                continue;
            }
            // Preserve independent reviewer changes instead of replacing them.
            if current.name != saved.name
                || current.user_ids != saved.user_ids
                || current.group_ids != saved.group_ids
                || current.source_required.is_some() != saved.source_required.is_some()
                || current.rule_type != saved.rule_type
            {
                return Err(unavailable(
                    "Approval rule changed independently; manual reconciliation is required",
                ));
            }
            if !self.put_approval_rule(iid, &target).await? {
                return Err(unavailable(
                    "Approval restoration was refused by project policy",
                ));
            }
        }
        note.journal.active = false;
        if !self.save_approval_note(iid, note).await? {
            return Err(unavailable(
                "GitLab refused completion of the recovery journal",
            ));
        }
        Ok(())
    }

    /// Discovery does not depend on MR description text. Run before executing
    /// repository code, including on runs that subsequently fail or pause.
    pub(in crate::gitlab) async fn recover_approvals(
        &self,
        mr: &MergeRequest,
        branch: &str,
    ) -> Result<Option<ApprovalStatus>, Error> {
        self.reconcile_approvals(mr, branch, false).await
    }

    pub(in crate::gitlab) async fn reconcile_approvals(
        &self,
        mr: &MergeRequest,
        branch: &str,
        keep: bool,
    ) -> Result<Option<ApprovalStatus>, Error> {
        let raw = self
            .every_page(
                self.merge_request_url(&format!("{}/notes", mr.iid))?,
                "approval recovery notes",
            )
            .await?;
        if !raw
            .iter()
            .any(|n| n["body"].as_str().is_some_and(|s| s.starts_with(JOURNAL)))
        {
            return Ok(None);
        }
        let actor = self.actor().await?;
        // A token rotation must not silently abandon a journal belonging to the
        // original MR author. Only that identity may edit its recovery note.
        if mr.raw["author"]["id"].as_u64() != Some(actor)
            && raw.iter().any(|n| {
                n["author"]["id"] == mr.raw["author"]["id"]
                    && n["body"].as_str().is_some_and(|s| {
                        s.strip_prefix(JOURNAL).is_some_and(|body| {
                            serde_json::from_str::<Journal>(body)
                                .map_or(true, |journal| journal.active)
                        })
                    })
            })
        {
            self.cancel_auto_merge(mr.iid).await?;
            return Err(unavailable(
                "Approval recovery requires the original bot identity after token rotation",
            ));
        }
        let mut notes = match self.decode_approval_notes(raw, mr.iid, branch, actor) {
            Ok(notes) => notes,
            Err(error) => {
                self.cancel_auto_merge(mr.iid).await?;
                return Err(error);
            }
        };
        let active = notes
            .iter_mut()
            .filter(|n| n.journal.active)
            .collect::<Vec<_>>();
        if active.is_empty() {
            return Ok(None);
        }
        self.approval_scope(&mr.raw, branch, actor)?;
        let config_result = self
            .send(
                Method::GET,
                self.url(&format!("{}/approvals", self.approval_project()))?,
                None,
            )
            .await;
        let config = self.approval_guard(mr.iid, config_result).await?;
        if config["disable_overriding_approvers_per_merge_request"] == true {
            self.cancel_auto_merge(mr.iid).await?;
            let state = self
                .send(
                    Method::GET,
                    self.merge_request_url(&format!("{}/approval_state", mr.iid))?,
                    None,
                )
                .await?;
            if state["approval_rules_overwritten"] == false {
                return Ok(Some(ApprovalStatus::new(
                    ApprovalKind::RecoveryPending,
                    "recovery superseded by project lock; journal retained and auto-merge cancelled until overrides are unlocked",
                )));
            }
            return Err(unavailable(
                "Project lock did not restore effective inherited approval rules",
            ));
        }
        if keep
            && active.len() == 1
            && config["disable_overriding_approvers_per_merge_request"] == false
            && self
                .approval_guard(mr.iid, self.snapshot_is_zero(mr.iid, active[0]).await)
                .await?
        {
            self.approval_guard(mr.iid, self.verify_approval_note(mr.iid, active[0]).await)
                .await?;
            return Ok(Some(ApprovalStatus::new(
                ApprovalKind::Retained,
                "existing consented override retained",
            )));
        }
        // Always cancel before restoring; list responses can lag the server.
        self.cancel_auto_merge(mr.iid).await?;
        for note in active {
            self.restore_approval_note(mr.iid, note, true).await?;
        }
        Ok(Some(ApprovalStatus::new(
            ApprovalKind::Restored,
            "restored before update",
        )))
    }

    pub(in crate::gitlab) async fn override_approvals(
        &self,
        iid: u64,
        sha: &str,
        branch: &str,
        rearm: bool,
    ) -> Result<ApprovalStatus, Error> {
        let mr = self.wait_until_checked(iid, sha).await?;
        let actor = self.actor().await?;
        if let Err(error) = self.approval_scope(&mr, branch, actor) {
            return Ok(ApprovalStatus::new(
                ApprovalKind::Unavailable,
                format!("override unavailable: {error}"),
            ));
        }
        let mut existing = match self.approval_notes(iid, branch, actor).await {
            Ok(notes) => notes,
            Err(error) => {
                self.cancel_auto_merge(iid).await?;
                return Err(error);
            }
        };
        let active = existing
            .iter_mut()
            .filter(|n| n.journal.active)
            .collect::<Vec<_>>();
        if !active.is_empty() {
            if active.len() != 1 {
                self.cancel_auto_merge(iid).await?;
                return Err(unavailable(
                    "Multiple active approval journals require recovery",
                ));
            }
            let note = active.into_iter().next().unwrap();
            let config_result = self
                .send(
                    Method::GET,
                    self.url(&format!("{}/approvals", self.approval_project()))?,
                    None,
                )
                .await;
            let config = self.approval_guard(iid, config_result).await?;
            if config["disable_overriding_approvers_per_merge_request"] != false {
                self.cancel_auto_merge(iid).await?;
                return Ok(ApprovalStatus::new(
                    ApprovalKind::RecoveryPending,
                    "approval recovery pending under project lock; auto-merge cancelled",
                ));
            }
            if !self
                .approval_guard(iid, self.snapshot_is_zero(iid, note).await)
                .await?
            {
                self.cancel_auto_merge(iid).await?;
                self.restore_approval_note(iid, note, true).await?;
                return Ok(ApprovalStatus::new(
                    ApprovalKind::Restored,
                    "approval rules changed; override restored",
                ));
            }
            self.approval_guard(iid, self.verify_approval_note(iid, note).await)
                .await?;
            let mr = MergeRequest::from_response(&mr)?;
            if (rearm || !mr.auto_merge_enabled() || mr.raw["should_remove_source_branch"] != true)
                && let Err(error) = self.enable_auto_merge(iid, sha).await
            {
                let cancelled = self.cancel_auto_merge(iid).await;
                let restored = self.restore_approval_note(iid, note, true).await;
                return Err(Error::Refused(format!(
                    "Rearming retained override failed: {error}; cancellation: {cancelled:?}; restoration: {restored:?}"
                )));
            }
            return Ok(ApprovalStatus::new(
                ApprovalKind::Retained,
                "existing consented override retained",
            ));
        }
        let config_url = self.url(&format!("{}/approvals", self.approval_project()))?;
        let Some(config) = self.approval_request(Method::GET, config_url, None).await? else {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted or approval rules unavailable",
            ));
        };
        if config["disable_overriding_approvers_per_merge_request"] != false {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted by project policy",
            ));
        }
        let Some(effective) = self
            .approval_request(
                Method::GET,
                self.merge_request_url(&format!("{iid}/approval_state"))?,
                None,
            )
            .await?
        else {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted: effective approval state unavailable",
            ));
        };
        let effective = effective["rules"]
            .as_array()
            .ok_or_else(|| unavailable("GitLab approval state has no rules"))?;
        if effective.iter().any(|r| {
            r["approvals_required"].as_u64().unwrap_or(1) > 0
                && (!editable(r) || r["contains_hidden_groups"] == true)
        }) {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted by effective approval policy",
            ));
        }
        let rules_url = self.merge_request_url(&format!("{iid}/approval_rules"))?;
        let Some(_) = self.approval_request(Method::GET, rules_url, None).await? else {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted or approval rules unavailable",
            ));
        };
        let raw = self.mr_approval_rules(iid).await?;
        let native_before = !raw
            .iter()
            .any(|r| matches!(r["rule_type"].as_str(), Some("regular" | "any_approver")));
        if raw.iter().any(|r| {
            (editable(r) && r["contains_hidden_groups"] != false)
                || (!editable(r) && r["approvals_required"].as_u64().unwrap_or(1) > 0)
        }) {
            return Ok(ApprovalStatus::new(
                ApprovalKind::PolicyDenied,
                "override not permitted by project policy or hidden approvers",
            ));
        }
        let mut rules = raw
            .iter()
            .filter(|r| editable(r))
            .map(Rule::read)
            .collect::<Result<Vec<_>, _>>()?;
        if native_before {
            let projects = self
                .every_page(
                    self.url(&format!("{}/approval_rules", self.approval_project()))?,
                    "project approval rules",
                )
                .await?;
            for value in projects
                .iter()
                .filter(|r| matches!(r["rule_type"].as_str(), Some("regular" | "any_approver")))
            {
                if value["contains_hidden_groups"] != false {
                    return Ok(ApprovalStatus::new(
                        ApprovalKind::Unavailable,
                        "override unavailable: project approval rules contain hidden approvers",
                    ));
                }
                let mut rule = Rule::read(value)?;
                rule.project_rule_id = Some(rule.id);
                rule.source_required = Some(rule.approvals_required);
                rule.id = 0;
                rules.push(rule);
            }
        }
        if rules.iter().all(|r| r.approvals_required == 0) {
            return Ok(if self.effective_approvals_zero(iid).await? {
                ApprovalStatus::new(ApprovalKind::AlreadyZero, "already zero")
            } else {
                ApprovalStatus::new(
                    ApprovalKind::PolicyDenied,
                    "override not permitted by effective approval policy",
                )
            });
        }
        if rules.iter().any(|r| r.source_required.is_some())
            && rules.iter().any(|r| r.source_required.is_none())
        {
            return Ok(ApprovalStatus::new(
                ApprovalKind::Unavailable,
                "override unavailable: mixed inherited and MR-only approval rules require manual review",
            ));
        }
        if !native_before && rules.iter().any(|r| r.source_required.is_some()) {
            let projects = self
                .every_page(
                    self.url(&format!("{}/approval_rules", self.approval_project()))?,
                    "project approval rules",
                )
                .await?;
            // GitLab's source_rule exposes only approvals_required, not an ID.
            // Bind the source while names still match, before changing anything.
            for rule in &mut rules {
                if let Some(required) = rule.source_required {
                    let matches = projects
                        .iter()
                        .filter(|p| {
                            p["name"] == rule.name
                                && p["rule_type"] == rule.rule_type
                                && p["approvals_required"].as_u64() == Some(required)
                        })
                        .collect::<Vec<_>>();
                    if matches.len() != 1 || matches[0]["contains_hidden_groups"] != false {
                        return Ok(ApprovalStatus::new(
                            ApprovalKind::Unavailable,
                            "override unavailable: inherited approval rule cannot be identified safely",
                        ));
                    }
                    rule.project_rule_id = matches[0]["id"].as_u64();
                    if rule.project_rule_id.is_none() {
                        return Err(Error::Refused(
                            "GitLab project approval rule has no id".into(),
                        ));
                    }
                }
            }
        }
        let notes = self.approval_notes(iid, branch, actor).await?;
        if notes.iter().any(|n| n.journal.active) {
            return Err(unavailable(
                "Pending approval recovery must complete before another override",
            ));
        }
        let journal = Journal {
            project: self.approval_project_id()?,
            iid,
            branch: branch.into(),
            actor,
            active: true,
            native_before,
            rules,
        };
        // Never overwrite a previous episode's recovery snapshot.
        let mut note = Note { id: 0, journal };
        self.cancel_auto_merge(iid).await?;
        if !self.save_approval_note(iid, &mut note).await? {
            return Ok(ApprovalStatus::new(
                ApprovalKind::Unavailable,
                "override unavailable: recovery journal creation not permitted",
            )
            .cancelled());
        }
        let result = async {
            for original in &note.journal.rules {
                if original.id == 0 {
                    let body = json!({"name": original.name, "approvals_required": 0, "approval_project_rule_id": original.project_rule_id});
                    let Some(created) = self.approval_request(Method::POST, self.merge_request_url(&format!("{iid}/approval_rules"))?, Some(&body)).await? else { return Ok(false); };
                    let created = Rule::read(&created)?;
                    let mut expected = original.clone(); expected.id = created.id; expected.approvals_required = 0;
                    if !created.same_definition(&expected) { return Err(unavailable("GitLab did not create the expected inherited MR approval rule")); }
                    continue;
                }
                if original.approvals_required == 0 {
                    continue;
                }
                let mut zero = original.clone();
                zero.approvals_required = 0;
                if !self.put_approval_rule(iid, &zero).await? {
                    return Ok(false);
                }
            }
            let checked = self.mr_approval_rules(iid).await?;
            if checked
                .iter()
                .any(|r| r["approvals_required"].as_u64() != Some(0))
            {
                return Err(unavailable(
                    "Approval requirements changed during the override",
                ));
            }
            if !self.effective_approvals_zero(iid).await? {
                return Ok(false);
            }
            self.verify_approval_note(iid, &note).await?;
            self.enable_auto_merge(iid, sha).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => Ok(ApprovalStatus::new(
                ApprovalKind::Overridden,
                "overridden to zero (recovery journal saved)",
            )),
            other => {
                let cancelled = self.cancel_auto_merge(iid).await;
                let restored = self.restore_approval_note(iid, &mut note, false).await;
                if cancelled.is_err() || restored.is_err() {
                    return Err(Error::Refused(format!(
                        "Approval override failed: {}; cancellation: {}; restoration: {}",
                        match &other {
                            Err(e) => e.to_string(),
                            _ => "project policy refused the override".into(),
                        },
                        match cancelled {
                            Ok(_) => "confirmed".into(),
                            Err(e) => e.to_string(),
                        },
                        match restored {
                            Ok(_) => "confirmed".into(),
                            Err(e) => e.to_string(),
                        }
                    )));
                }
                match other {
                    Ok(false) => Ok(ApprovalStatus::new(
                        ApprovalKind::RolledBack,
                        "override not permitted by project policy; changes rolled back",
                    )),
                    Err(error) => Err(error),
                    Ok(true) => unreachable!(),
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::path_regex;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const SHA: &str = "1111111111111111111111111111111111111111";
    const BRANCH: &str = "automation/upd-dependencies";

    struct State {
        rules: Vec<Value>,
        project_rules: BTreeMap<u64, Value>,
        notes: Vec<Value>,
        calls: Vec<String>,
        armed: bool,
        denied: Option<u64>,
        deny_restore: bool,
        lose_write: bool,
        lose_completion: bool,
        lose_arm: bool,
        invalidate_note: bool,
        concurrent_rule: bool,
        note_failure: bool,
        note_denied: bool,
        state_denied: bool,
        cancel_lying: bool,
        effective_policy_after_write: bool,
        actor: u64,
        foreign_mr: bool,
        locked: bool,
    }

    #[derive(Clone)]
    pub(crate) struct Api(Arc<Mutex<State>>);

    pub(crate) fn rule(id: u64, required: u64) -> Value {
        json!({"id": id, "name": format!("rule {id}"), "rule_type": "regular",
            "approvals_required": required, "users": [{"id": 12}], "groups": [{"id": 13}],
            "contains_hidden_groups": false, "source_rule": null})
    }

    fn mr() -> MergeRequest {
        MergeRequest::from_response(&json!({"iid": 7, "web_url": "https://gitlab.test/mr/7",
            "sha": SHA, "detailed_merge_status": "mergeable", "source_project_id": 1,
            "target_project_id": 1, "source_branch": BRANCH, "author": {"id": 42},
            "description": "reviewer replaced the description", "user_notes_count": 0}))
        .unwrap()
    }

    impl Respond for Api {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let mut state = self.0.lock().unwrap();
            let path = request.url.path();
            let method = request.method.to_string();
            state.calls.push(format!("{method} {path}"));
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let ok = |v| ResponseTemplate::new(200).set_body_json(v);
            if method == "GET" {
                if path == "/api/v4/user" {
                    return ok(json!({"id": state.actor}));
                }
                if path.ends_with("/projects/1/merge_requests") {
                    let mut value = mr().raw;
                    value["auto_merge_enabled"] = json!(state.armed);
                    return ok(json!([value]));
                }
                if path.ends_with("/merge_requests/7") {
                    let mut value = mr().raw;
                    value["auto_merge_enabled"] = json!(state.armed);
                    value["should_remove_source_branch"] = json!(true);
                    if state.foreign_mr {
                        value["author"]["id"] = json!(99);
                    }
                    return ok(value);
                }
                if path.ends_with("/approvals") {
                    return ok(
                        json!({"disable_overriding_approvers_per_merge_request": state.locked}),
                    );
                }
                if path.ends_with("/approval_state") {
                    if state.state_denied {
                        return ResponseTemplate::new(403);
                    }
                    let overwritten = !state.locked && !state.rules.is_empty();
                    let mut effective: Vec<Value> = if overwritten {
                        state.rules.clone()
                    } else {
                        state.project_rules.values().cloned().collect()
                    };
                    if state.effective_policy_after_write
                        && state.rules.iter().all(|r| r["approvals_required"] == 0)
                    {
                        let mut mandatory = rule(77, 1);
                        mandatory["rule_type"] = json!("code_owner");
                        effective.push(mandatory);
                    }
                    return ok(
                        json!({"approval_rules_overwritten": overwritten, "rules": effective}),
                    );
                }
                if path.ends_with("/merge_requests/7/approval_rules") {
                    return ok(json!(state.rules));
                }
                if path == "/api/v4/projects/1/approval_rules" {
                    return ok(json!(
                        state.project_rules.values().cloned().collect::<Vec<_>>()
                    ));
                }
                if path.ends_with("/notes") {
                    return ok(json!(state.notes));
                }
                if let Some(id) = path
                    .strip_prefix("/api/v4/projects/1/merge_requests/7/notes/")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if state.invalidate_note {
                        let note = state.notes.iter_mut().find(|n| n["id"] == id).unwrap();
                        let mut journal: Journal = serde_json::from_str(
                            note["body"]
                                .as_str()
                                .unwrap()
                                .strip_prefix(JOURNAL)
                                .unwrap(),
                        )
                        .unwrap();
                        journal.active = false;
                        note["body"] = json!(format!(
                            "{JOURNAL}{}",
                            serde_json::to_string(&journal).unwrap()
                        ));
                    }
                    return state
                        .notes
                        .iter()
                        .find(|n| n["id"] == id)
                        .cloned()
                        .map(ok)
                        .unwrap_or(ResponseTemplate::new(404));
                }
                if let Some(id) = path
                    .strip_prefix("/api/v4/projects/1/approval_rules/")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    return state
                        .project_rules
                        .get(&id)
                        .cloned()
                        .map(ok)
                        .unwrap_or(ResponseTemplate::new(404));
                }
            }
            if method == "POST" && path.ends_with("/merge_requests/7/approval_rules") {
                let source = body["approval_project_rule_id"].as_u64().unwrap();
                let Some(mut value) = state.project_rules.get(&source).cloned() else {
                    return ResponseTemplate::new(404);
                };
                let count = value["approvals_required"].clone();
                value["id"] = json!(100 + state.rules.len());
                value["approvals_required"] = body["approvals_required"].clone();
                value["source_rule"] = json!({"approvals_required": count});
                state.rules.push(value.clone());
                return ResponseTemplate::new(201).set_body_json(value);
            }
            if method == "PUT" && path.ends_with("/merge") {
                state.armed = true;
                if state.lose_arm {
                    state.lose_arm = false;
                    return ResponseTemplate::new(500);
                }
                return ok(json!({}));
            }
            if method == "DELETE"
                && let Some(id) = path
                    .strip_prefix("/api/v4/projects/1/merge_requests/7/approval_rules/")
                    .and_then(|s| s.parse::<u64>().ok())
            {
                state.rules.retain(|r| r["id"] != id);
                if state.concurrent_rule {
                    state.concurrent_rule = false;
                    state.rules.push(rule(55, 1));
                }
                return ResponseTemplate::new(204);
            }
            if method == "POST" && path.ends_with("/cancel_merge_when_pipeline_succeeds") {
                if state.cancel_lying || !state.armed {
                    return ResponseTemplate::new(201).set_body_json(json!({"status":"error","http_status":406,"message":"Can't cancel the automatic merge"}));
                }
                state.armed = false;
                return ResponseTemplate::new(201).set_body_json(json!({"status":"success"}));
            }
            if method == "PUT"
                && let Some(id) = path
                    .strip_prefix("/api/v4/projects/1/merge_requests/7/approval_rules/")
                    .and_then(|s| s.parse::<u64>().ok())
            {
                if (body["approvals_required"] == 0 && state.denied == Some(id))
                    || (body["approvals_required"] != 0 && state.deny_restore)
                {
                    return ResponseTemplate::new(403);
                }
                let target = state.rules.iter_mut().find(|r| r["id"] == id).unwrap();
                for field in ["name", "approvals_required"] {
                    target[field] = body[field].clone();
                }
                for (field, ids) in [("users", "user_ids"), ("groups", "group_ids")] {
                    target[field] = Value::Array(
                        body[ids]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| json!({"id":id}))
                            .collect(),
                    );
                }
                let updated = target.clone();
                if state.lose_write && body["approvals_required"] == 0 {
                    state.lose_write = false;
                    return ResponseTemplate::new(500);
                }
                return ok(updated);
            }
            if method == "POST" && path.ends_with("/notes") {
                if state.note_denied {
                    return ResponseTemplate::new(403);
                }
                if state.note_failure {
                    return ResponseTemplate::new(500);
                }
                let value = json!({"id": state.notes.len() + 1, "body": body["body"], "author": {"id": 42}});
                state.notes.push(value.clone());
                return ResponseTemplate::new(201).set_body_json(value);
            }
            if method == "PUT"
                && let Some(id) = path
                    .strip_prefix("/api/v4/projects/1/merge_requests/7/notes/")
                    .and_then(|s| s.parse::<u64>().ok())
            {
                if state.lose_completion
                    && body["body"].as_str().unwrap().contains("\"active\":false")
                {
                    state.lose_completion = false;
                    return ResponseTemplate::new(500);
                }
                let target = state.notes.iter_mut().find(|n| n["id"] == id).unwrap();
                target["body"] = body["body"].clone();
                return ok(target.clone());
            }
            ResponseTemplate::new(404)
        }
    }

    pub(crate) async fn fixture(rules: Vec<Value>) -> (MockServer, Client, Api) {
        let server = MockServer::start().await;
        let api = Api(Arc::new(Mutex::new(State {
            rules,
            project_rules: BTreeMap::new(),
            notes: Vec::new(),
            calls: Vec::new(),
            armed: false,
            denied: None,
            deny_restore: false,
            lose_write: false,
            lose_completion: false,
            lose_arm: false,
            invalidate_note: false,
            concurrent_rule: false,
            note_failure: false,
            note_denied: false,
            state_denied: false,
            cancel_lying: false,
            effective_policy_after_write: false,
            actor: 42,
            foreign_mr: false,
            locked: false,
        })));
        Mock::given(path_regex("^/api/v4/"))
            .respond_with(api.clone())
            .mount(&server)
            .await;
        let retry = Retry {
            attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        };
        let client = Client::new(&format!("{}/api/v4", server.uri()), "test-token")
            .unwrap()
            .for_project(1)
            .with_retry(retry)
            .with_settle(retry);
        (server, client, api)
    }

    pub(crate) fn snapshot(api: &Api) -> Value {
        let state = api.0.lock().unwrap();
        json!({"rules":state.rules,"notes":state.notes,"armed":state.armed,"calls":state.calls})
    }

    fn active(api: &Api) -> bool {
        api.0.lock().unwrap().notes.iter().any(|n| {
            n["body"]
                .as_str()
                .is_some_and(|s| s.contains("\"active\":true"))
        })
    }

    #[tokio::test]
    async fn journal_precedes_override_and_description_edits_do_not_break_recovery() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap()
                .detail
                .starts_with("overridden to zero")
        );
        assert!(active(&api));
        {
            let state = api.0.lock().unwrap();
            assert_eq!(state.rules[0]["approvals_required"], 0);
            let journal = state
                .calls
                .iter()
                .position(|s| s == "POST /api/v4/projects/1/merge_requests/7/notes")
                .unwrap();
            let write = state
                .calls
                .iter()
                .position(|s| s == "PUT /api/v4/projects/1/merge_requests/7/approval_rules/9")
                .unwrap();
            assert!(journal < write);
        }
        assert!(
            client
                .recover_approvals(&mr(), BRANCH)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
        assert!(!active(&api));
        assert!(
            client
                .recover_approvals(&mr(), BRANCH)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn policy_refusal_rolls_back_every_prior_write() {
        let original = vec![rule(9, 2), rule(10, 3)];
        let (_server, client, api) = fixture(original.clone()).await;
        api.0.lock().unwrap().denied = Some(10);
        let status = client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        assert!(status.detail.contains("changes rolled back"));
        assert_eq!(api.0.lock().unwrap().rules, original);
        assert!(!active(&api));
        assert!(!api.0.lock().unwrap().armed);
    }

    #[tokio::test]
    async fn a_write_with_a_lost_response_is_also_rolled_back() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().lose_write = true;
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .is_err()
        );
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
        assert!(!active(&api));
        assert!(!api.0.lock().unwrap().armed);
    }

    #[tokio::test]
    async fn failed_rollback_keeps_the_journal_and_retries_on_the_next_run() {
        let (_server, client, api) = fixture(vec![rule(9, 2), rule(10, 3)]).await;
        {
            let mut state = api.0.lock().unwrap();
            state.denied = Some(10);
            state.deny_restore = true;
        }
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .is_err()
        );
        assert!(active(&api));
        assert!(!api.0.lock().unwrap().armed);
        api.0.lock().unwrap().deny_restore = false;
        assert!(
            client
                .recover_approvals(&mr(), BRANCH)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(api.0.lock().unwrap().rules[0]["approvals_required"], 2);
        assert!(!active(&api));
    }

    #[tokio::test]
    async fn inherited_restoration_updates_name_approvers_and_count_and_can_resume() {
        let mut inherited = rule(9, 2);
        inherited["source_rule"] = json!({"approvals_required": 2});
        let (_server, client, api) = fixture(vec![inherited]).await;
        let mut initial = rule(5, 2);
        initial["name"] = json!("rule 9");
        api.0.lock().unwrap().project_rules.insert(5, initial);
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        {
            let mut state = api.0.lock().unwrap();
            let mut project = rule(5, 3);
            project["name"] = json!("new project policy");
            project["users"] = json!([{"id": 22}]);
            project["groups"] = json!([{"id": 23}]);
            state.project_rules.insert(5, project);
            state.lose_completion = true;
        }
        assert!(client.recover_approvals(&mr(), BRANCH).await.is_err());
        assert!(active(&api));
        assert!(client.recover_approvals(&mr(), BRANCH).await.is_ok());
        assert!(api.0.lock().unwrap().rules.is_empty());
        let state = client
            .send(
                Method::GET,
                client.merge_request_url("7/approval_state").unwrap(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(state["approval_rules_overwritten"], false);
        assert_eq!(state["rules"][0]["name"], "new project policy");
        assert_eq!(state["rules"][0]["approvals_required"], 3);
        assert_eq!(state["rules"][0]["users"], json!([{"id":22}]));
        assert_eq!(state["rules"][0]["groups"], json!([{"id":23}]));
        assert!(!active(&api));
    }

    #[tokio::test]
    async fn foreign_notes_and_description_text_are_not_recovery_authority() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0
            .lock()
            .unwrap()
            .notes
            .push(json!({"id": 1, "author": {"id": 99}, "body": format!("{JOURNAL}malformed")}));
        let mut request = mr();
        request.raw["description"] = json!("<!-- upd-approval-override:malformed -->");
        assert!(
            client
                .recover_approvals(&request, BRANCH)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            api.0
                .lock()
                .unwrap()
                .calls
                .iter()
                .all(|s| s.starts_with("GET"))
        );
    }

    #[tokio::test]
    async fn locked_or_hidden_rules_have_no_override_writes() {
        for hidden in [false, true] {
            let mut value = rule(9, 1);
            value["contains_hidden_groups"] = json!(hidden);
            let (_server, client, api) = fixture(vec![value]).await;
            api.0.lock().unwrap().locked = !hidden;
            assert!(
                client
                    .override_approvals(7, SHA, BRANCH, false)
                    .await
                    .unwrap()
                    .detail
                    .contains("not permitted")
            );
            assert!(
                api.0
                    .lock()
                    .unwrap()
                    .calls
                    .iter()
                    .all(|s| s.starts_with("GET"))
            );
        }
    }
    #[tokio::test]
    async fn native_inheritance_survives_repeated_opt_in_and_current_project_changes() {
        let (_server, client, api) = fixture(vec![]).await;
        api.0.lock().unwrap().project_rules.insert(5, rule(5, 2));
        for iteration in 0..2 {
            assert!(
                client
                    .override_approvals(7, SHA, BRANCH, false)
                    .await
                    .unwrap()
                    .detail
                    .starts_with("overridden to zero")
            );
            if iteration == 0 {
                api.0.lock().unwrap().project_rules.insert(6, rule(6, 3));
            }
            client.recover_approvals(&mr(), BRANCH).await.unwrap();
            assert!(api.0.lock().unwrap().rules.is_empty());
            assert!(!active(&api));
        }
        assert_eq!(
            api.0.lock().unwrap().notes.len(),
            2,
            "Each override must have a distinct snapshot"
        );
    }

    #[tokio::test]
    async fn lost_arm_response_cancels_and_restores_the_override() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().lose_arm = true;
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .is_err()
        );
        assert!(!api.0.lock().unwrap().armed);
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
        assert!(!active(&api));
    }

    #[tokio::test]
    async fn recovery_note_changed_by_another_run_prevents_arming() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().invalidate_note = true;
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .is_err()
        );
        assert!(!api.0.lock().unwrap().armed);
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
    }

    #[tokio::test]
    async fn a_project_lock_supersedes_but_does_not_erase_pending_recovery() {
        let (_server, client, api) = fixture(vec![]).await;
        api.0.lock().unwrap().project_rules.insert(5, rule(5, 2));
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        api.0.lock().unwrap().locked = true;
        assert!(
            client
                .recover_approvals(&mr(), BRANCH)
                .await
                .unwrap()
                .unwrap()
                .detail
                .contains("superseded by project lock")
        );
        assert!(active(&api));
        api.0.lock().unwrap().locked = false;
        client.recover_approvals(&mr(), BRANCH).await.unwrap();
        assert!(api.0.lock().unwrap().rules.is_empty());
        assert!(!active(&api));
    }

    #[tokio::test]
    async fn a_rule_added_during_inherited_restoration_does_not_replace_project_requirements() {
        let (_server, client, api) = fixture(vec![]).await;
        api.0.lock().unwrap().project_rules.insert(5, rule(5, 2));
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        api.0.lock().unwrap().concurrent_rule = true;
        assert!(client.recover_approvals(&mr(), BRANCH).await.is_err());
        assert!(active(&api));
        assert!(!api.0.lock().unwrap().armed);
        let rules = api.0.lock().unwrap().rules.clone();
        assert!(
            rules
                .iter()
                .any(|r| r["name"] == "rule 5" && r["approvals_required"] == 2)
        );
        assert!(rules.iter().any(|r| r["id"] == 55));
    }
    #[tokio::test]
    async fn unchanged_consented_override_keeps_one_journal_and_its_auto_merge() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        api.0.lock().unwrap().calls.clear();
        assert_eq!(
            client
                .reconcile_approvals(&mr(), BRANCH, true)
                .await
                .unwrap()
                .unwrap()
                .status,
            ApprovalKind::Retained
        );
        assert_eq!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap()
                .status,
            ApprovalKind::Retained
        );
        let state = api.0.lock().unwrap();
        assert_eq!(state.notes.len(), 1);
        assert!(state.armed);
        assert!(
            state.calls.iter().all(|s| s.starts_with("GET")),
            "{:?}",
            state.calls
        );
    }

    #[tokio::test]
    async fn a_failed_journal_write_never_lowers_approvals() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().note_failure = true;
        assert!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .is_err()
        );
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
        assert!(!api.0.lock().unwrap().armed);
    }

    #[tokio::test]
    async fn policy_activated_during_override_rolls_back_before_normal_auto_merge() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().effective_policy_after_write = true;
        assert_eq!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap()
                .status,
            ApprovalKind::RolledBack
        );
        assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
        assert!(!active(&api));
        assert!(!api.0.lock().unwrap().armed);
    }

    #[tokio::test]
    async fn foreign_author_is_an_override_limitation_without_writes() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        api.0.lock().unwrap().foreign_mr = true;
        assert_eq!(
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap()
                .status,
            ApprovalKind::Unavailable
        );
        assert!(
            api.0
                .lock()
                .unwrap()
                .calls
                .iter()
                .all(|s| s.starts_with("GET"))
        );
    }

    #[tokio::test]
    async fn changing_bot_identity_requires_explicit_recovery() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        api.0.lock().unwrap().actor = 43;
        assert!(
            client
                .recover_approvals(&mr(), BRANCH)
                .await
                .unwrap_err()
                .to_string()
                .contains("original bot identity")
        );
        assert!(active(&api));
        assert!(!api.0.lock().unwrap().armed);
    }
    #[tokio::test]
    async fn unavailable_read_or_journal_permissions_are_reported_without_lowering() {
        for deny_note in [false, true] {
            let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
            {
                let mut state = api.0.lock().unwrap();
                state.note_denied = deny_note;
                state.state_denied = !deny_note;
            }
            let status = client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap();
            assert!(matches!(
                status.status,
                ApprovalKind::Unavailable | ApprovalKind::PolicyDenied
            ));
            assert_eq!(api.0.lock().unwrap().rules[0], rule(9, 2));
            assert!(!active(&api));
        }
    }

    #[tokio::test]
    async fn failure_verifying_an_active_override_cancels_it_and_keeps_recovery() {
        let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
        client
            .override_approvals(7, SHA, BRANCH, false)
            .await
            .unwrap();
        api.0.lock().unwrap().state_denied = true;
        assert!(
            client
                .reconcile_approvals(&mr(), BRANCH, true)
                .await
                .is_err()
        );
        assert!(!api.0.lock().unwrap().armed);
        assert!(active(&api));
    }
    #[tokio::test]
    async fn cancellation_error_body_requires_confirmation_that_the_mr_is_unarmed() {
        for armed in [false, true] {
            let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
            {
                let mut state = api.0.lock().unwrap();
                state.armed = armed;
                state.cancel_lying = true;
            }
            assert_eq!(client.cancel_auto_merge(7).await.is_err(), armed);
            assert!(
                api.0
                    .lock()
                    .unwrap()
                    .calls
                    .iter()
                    .any(|s| s == "GET /api/v4/projects/1/merge_requests/7")
            );
        }
    }

    #[tokio::test]
    async fn retained_override_with_a_changed_requirement_is_restored_and_preserves_strengthening()
    {
        for count in [1, 3] {
            let (_server, client, api) = fixture(vec![rule(9, 2)]).await;
            client
                .override_approvals(7, SHA, BRANCH, false)
                .await
                .unwrap();
            api.0.lock().unwrap().rules[0]["approvals_required"] = json!(count);
            assert_eq!(
                client
                    .reconcile_approvals(&mr(), BRANCH, true)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                ApprovalKind::Restored
            );
            assert_eq!(
                api.0.lock().unwrap().rules[0]["approvals_required"],
                count.max(2)
            );
            assert!(!api.0.lock().unwrap().armed);
            assert!(!active(&api));
        }
    }
}
