use super::*;
use needle_core::{
    CanonicalHasher, ChangeApplyId, ChangeApplyRecord, ChangeApplyStatus, ChangeId, ChangeRequest,
    PatchArtifact, PatchId, VerificationArtifact, VerificationStatus,
};

type PreparedChangeRow = (String, String, String, String, String, String, String, bool, u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchFileBlob {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedChangeRecord {
    pub request: ChangeRequest,
    pub request_digest: Digest,
    pub repository_id: Digest,
    pub source_snapshot: Digest,
    pub state: String,
    pub patch: PatchArtifact,
    pub declared_output: serde_json::Value,
    pub repair_attempted: bool,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeAttemptRecord {
    pub role: String,
    pub patch_id: PatchId,
    pub attempt: serde_json::Value,
    pub usage: serde_json::Value,
    pub cost_microusd: Option<u64>,
    pub created_unix_ms: u64,
}

impl RuntimeStore {
    pub fn record_change_request(
        &self,
        change_id: &ChangeId,
        repository_id: Digest,
        source_snapshot: Digest,
        request_digest: Digest,
        request: &ChangeRequest,
    ) -> Result<(), StoreError> {
        let request_json = serde_json::to_string(request)?;
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String, String)> = transaction
            .query_row(
                "SELECT request_digest, repository_id, source_snapshot_digest, request_json
                 FROM change_requests WHERE change_id=?1",
                [change_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((stored_request, stored_repository, stored_snapshot, stored_json)) = existing {
            if stored_request == request_digest.to_string()
                && stored_repository == repository_id.to_string()
                && stored_snapshot == source_snapshot.to_string()
                && stored_json == request_json
            {
                transaction.commit()?;
                return Ok(());
            }
            return Err(StoreError::ChangeConflict(change_id.to_string()));
        }
        transaction.execute(
            "INSERT INTO change_requests(
                change_id, request_digest, repository_id, source_snapshot_digest,
                state, request_json, latest_patch_revision, repair_attempted,
                created_unix_ms, updated_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, 'requested', ?5, 0, 0, ?6, ?6)",
            params![
                change_id.to_string(),
                request_digest.to_string(),
                repository_id.to_string(),
                source_snapshot.to_string(),
                request_json,
                now
            ],
        )?;
        let event_json = serde_json::to_string(&serde_json::json!({
            "request_digest": request_digest,
            "state": "requested"
        }))?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'requested', ?2, ?3, ?4)",
            params![
                change_id.to_string(),
                Digest::blake3(event_json.as_bytes()).to_string(),
                event_json,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_prepared_change(
        &self,
        repository_id: Digest,
        request_digest: Digest,
        request: &ChangeRequest,
        patch: &PatchArtifact,
        declared_output: &serde_json::Value,
        file_blobs: &[PatchFileBlob],
    ) -> Result<(), StoreError> {
        if patch.id != PatchArtifact::compute_id(patch.source_snapshot, &patch.files) {
            return Err(StoreError::PatchArtifact(
                "patch id does not match the filesystem manifest".to_owned(),
            ));
        }
        if file_blobs.len() != patch.files.len() {
            return Err(StoreError::PatchArtifact(
                "patch blob count does not match the manifest".to_owned(),
            ));
        }
        for file in &patch.files {
            let Some(blob) = file_blobs.iter().find(|blob| blob.path == file.path) else {
                return Err(StoreError::PatchArtifact(format!(
                    "patch blob is missing for `{}`",
                    file.path
                )));
            };
            let before_digest = blob.before.as_deref().map(Digest::blake3);
            let after_digest = blob.after.as_deref().map(Digest::blake3);
            if before_digest != file.before_digest || after_digest != file.after_digest {
                return Err(StoreError::PatchArtifact(format!(
                    "patch blob digest does not match `{}`",
                    file.path
                )));
            }
        }

        let request_json = serde_json::to_string(request)?;
        let artifact_json = serde_json::to_string(patch)?;
        let manifest_json = serde_json::to_string(&patch.files)?;
        let declared_output_json = serde_json::to_string(declared_output)?;
        let discrepancies_json = serde_json::to_string(&patch.discrepancies)?;
        let event_payload = serde_json::json!({
            "patch_id": patch.id,
            "revision": patch.revision,
            "state": "prepared"
        });
        let event_json = serde_json::to_string(&event_payload)?;
        let event_digest = Digest::blake3(event_json.as_bytes());
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing: Option<(String, String, String, u32, bool)> = transaction
            .query_row(
                "SELECT request_digest, source_snapshot_digest, state,
                        latest_patch_revision, repair_attempted
                 FROM change_requests WHERE change_id=?1",
                [patch.change_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        match &existing {
            None if patch.revision != 1 => {
                return Err(StoreError::PatchArtifact(
                    "a new change must start at patch revision 1".to_owned(),
                ));
            }
            Some((existing_request, existing_snapshot, state, 1, true))
                if existing_request == &request_digest.to_string()
                    && existing_snapshot == &patch.source_snapshot.to_string()
                    && state == "repairing"
                    && patch.revision == 2 => {}
            Some((existing_request, existing_snapshot, state, 0, false))
                if existing_request == &request_digest.to_string()
                    && existing_snapshot == &patch.source_snapshot.to_string()
                    && state == "requested"
                    && patch.revision == 1 => {}
            Some(_) => {
                return Err(StoreError::ChangeConflict(patch.change_id.to_string()));
            }
            None => {}
        }
        transaction.execute(
            "INSERT INTO change_requests(
                change_id, request_digest, repository_id, source_snapshot_digest,
                state, request_json, latest_patch_revision, created_unix_ms, updated_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, 'prepared', ?5, ?6, ?7, ?7)
             ON CONFLICT(change_id) DO NOTHING",
            params![
                patch.change_id.to_string(),
                request_digest.to_string(),
                repository_id.to_string(),
                patch.source_snapshot.to_string(),
                request_json,
                patch.revision,
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO patch_artifacts(
                patch_id, change_id, revision, source_snapshot_digest, patch_digest,
                artifact_json, manifest_json, declared_output_json, discrepancies_json,
                created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?1, ?5, ?6, ?7, ?8, ?9)",
            params![
                patch.id.to_string(),
                patch.change_id.to_string(),
                patch.revision,
                patch.source_snapshot.to_string(),
                artifact_json,
                manifest_json,
                declared_output_json,
                discrepancies_json,
                now
            ],
        )?;
        for blob in file_blobs {
            let manifest = patch
                .files
                .iter()
                .find(|file| file.path == blob.path)
                .expect("patch blob presence was validated");
            transaction.execute(
                "INSERT INTO patch_files(
                    patch_id, path, operation, before_digest, after_digest,
                    before_blob, after_blob
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    patch.id.to_string(),
                    blob.path,
                    serde_json::to_value(manifest.operation)?.as_str().unwrap_or_default(),
                    manifest.before_digest.map(|digest| digest.to_string()),
                    manifest.after_digest.map(|digest| digest.to_string()),
                    blob.before,
                    blob.after
                ],
            )?;
        }
        transaction.execute(
            "UPDATE change_requests
             SET state='prepared', latest_patch_revision=?2, updated_unix_ms=?3
             WHERE change_id=?1",
            params![patch.change_id.to_string(), patch.revision, now],
        )?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'prepared', ?2, ?3, ?4)",
            params![patch.change_id.to_string(), event_digest.to_string(), event_json, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn prepared_change(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<PreparedChangeRecord>, StoreError> {
        let connection = self.connection()?;
        let row: Option<PreparedChangeRow> = connection
            .query_row(
                "SELECT c.request_json, c.request_digest, c.repository_id,
                        c.source_snapshot_digest, c.state, p.artifact_json,
                        p.declared_output_json, c.repair_attempted, c.created_unix_ms
                 FROM change_requests c
                 JOIN patch_artifacts p
                   ON p.change_id=c.change_id AND p.revision=c.latest_patch_revision
                 WHERE c.change_id=?1",
                [change_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            request,
            request_digest,
            repository_id,
            snapshot,
            state,
            patch,
            output,
            repair_attempted,
            created,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(PreparedChangeRecord {
            request: serde_json::from_str(&request)?,
            request_digest: Digest::parse(&request_digest)
                .map_err(|_| StoreError::Digest(request_digest))?,
            repository_id: Digest::parse(&repository_id)
                .map_err(|_| StoreError::Digest(repository_id))?,
            source_snapshot: Digest::parse(&snapshot).map_err(|_| StoreError::Digest(snapshot))?,
            state,
            patch: serde_json::from_str(&patch)?,
            declared_output: serde_json::from_str(&output)?,
            repair_attempted,
            created_unix_ms: created,
        }))
    }

    pub fn changes(&self, limit: usize) -> Result<Vec<PreparedChangeRecord>, StoreError> {
        let limit = limit.clamp(1, 100) as u64;
        let ids = {
            let connection = self.connection()?;
            let mut statement = connection.prepare(
                "SELECT change_id FROM change_requests WHERE latest_patch_revision > 0
                 ORDER BY updated_unix_ms DESC, change_id LIMIT ?1",
            )?;
            statement
                .query_map([limit], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| {
                let id = ChangeId::parse(&id)
                    .map_err(|error| StoreError::PatchArtifact(error.to_owned()))?;
                self.prepared_change(&id)?.ok_or_else(|| {
                    StoreError::PatchArtifact(format!("change `{id}` has no current patch"))
                })
            })
            .collect()
    }

    pub fn record_change_failure(
        &self,
        change_id: &ChangeId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let payload = serde_json::json!({"reason": reason});
        let payload_json = serde_json::to_string(&payload)?;
        let payload_digest = Digest::blake3(payload_json.as_bytes());
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE change_requests SET state='failed', updated_unix_ms=?2 WHERE change_id=?1",
            params![change_id.to_string(), now],
        )?;
        if changed != 1 {
            return Err(StoreError::ChangeConflict(change_id.to_string()));
        }
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'failed', ?2, ?3, ?4)",
            params![change_id.to_string(), payload_digest.to_string(), payload_json, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_change_repair(
        &self,
        change_id: &ChangeId,
        patch_id: PatchId,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let verification_json: Option<String> = transaction
            .query_row(
                "SELECT v.artifact_json
                 FROM change_requests c
                 JOIN patch_artifacts p
                   ON p.change_id=c.change_id AND p.revision=c.latest_patch_revision
                 JOIN verification_artifacts v
                   ON v.change_id=c.change_id AND v.patch_id=p.patch_id
                 WHERE c.change_id=?1 AND c.state='repairable'
                   AND c.latest_patch_revision=1 AND c.repair_attempted=0
                   AND p.patch_id=?2
                 ORDER BY v.created_unix_ms DESC, v.verification_id LIMIT 1",
                params![change_id.to_string(), patch_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let verification = verification_json
            .map(|json| serde_json::from_str::<VerificationArtifact>(&json))
            .transpose()?
            .filter(|artifact| {
                artifact.change_id == *change_id
                    && artifact.patch_id == patch_id
                    && artifact.verdict == VerificationStatus::Repairable
                    && artifact.is_canonical()
            })
            .ok_or_else(|| StoreError::ChangeConflict(change_id.to_string()))?;
        let changed = transaction.execute(
            "UPDATE change_requests
             SET state='repairing', repair_attempted=1, updated_unix_ms=?2
             WHERE change_id=?1 AND state='repairable'
               AND latest_patch_revision=1 AND repair_attempted=0",
            params![change_id.to_string(), now],
        )?;
        if changed != 1 {
            return Err(StoreError::ChangeConflict(change_id.to_string()));
        }
        let payload_json = serde_json::to_string(&serde_json::json!({
            "patch_id": patch_id,
            "verification_id": verification.id,
            "state": "repairing"
        }))?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'repair_started', ?2, ?3, ?4)",
            params![
                change_id.to_string(),
                Digest::blake3(payload_json.as_bytes()).to_string(),
                payload_json,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn patch_file_blobs(&self, patch_id: PatchId) -> Result<Vec<PatchFileBlob>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT path, before_blob, after_blob FROM patch_files
             WHERE patch_id=?1 ORDER BY path",
        )?;
        statement
            .query_map([patch_id.to_string()], |row| {
                Ok(PatchFileBlob { path: row.get(0)?, before: row.get(1)?, after: row.get(2)? })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn record_verification_artifact(
        &self,
        artifact: &VerificationArtifact,
        attempt: &serde_json::Value,
        usage: &serde_json::Value,
        cost_microusd: Option<u64>,
    ) -> Result<(), StoreError> {
        if !artifact.is_canonical() || artifact.verdict == VerificationStatus::NotRequested {
            return Err(StoreError::PatchArtifact(
                "verification artifact is not canonical".to_owned(),
            ));
        }
        let artifact_json = serde_json::to_string(artifact)?;
        let attempt_json = serde_json::to_string(attempt)?;
        let usage_json = serde_json::to_string(usage)?;
        let verdict =
            serde_json::to_value(artifact.verdict)?.as_str().unwrap_or("inconclusive").to_owned();
        let event_payload = serde_json::json!({
            "verification_id": artifact.id,
            "patch_id": artifact.patch_id,
            "verdict": verdict
        });
        let event_json = serde_json::to_string(&event_payload)?;
        let event_digest = Digest::blake3(event_json.as_bytes());
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let patch_exists: u64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM patch_artifacts p
             JOIN change_requests c
               ON c.change_id=p.change_id AND c.latest_patch_revision=p.revision
             WHERE p.patch_id=?1 AND p.change_id=?2",
            params![artifact.patch_id.to_string(), artifact.change_id.to_string()],
            |row| row.get(0),
        )?;
        if patch_exists != 1 {
            return Err(StoreError::PatchArtifact(
                "verification references an unknown patch".to_owned(),
            ));
        }
        transaction.execute(
            "INSERT INTO verification_artifacts(
                verification_id, change_id, patch_id, verdict, artifact_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                artifact.id.to_string(),
                artifact.change_id.to_string(),
                artifact.patch_id.to_string(),
                verdict,
                artifact_json,
                artifact.created_unix_ms
            ],
        )?;
        transaction.execute(
            "INSERT INTO change_attempts(
                change_id, patch_id, role, attempt_json, usage_json, cost_microusd,
                created_unix_ms
             ) VALUES(?1, ?2, 'verifier', ?3, ?4, ?5, ?6)",
            params![
                artifact.change_id.to_string(),
                artifact.patch_id.to_string(),
                attempt_json,
                usage_json,
                cost_microusd,
                artifact.created_unix_ms
            ],
        )?;
        transaction.execute(
            "UPDATE change_requests SET state=?2, updated_unix_ms=?3 WHERE change_id=?1",
            params![artifact.change_id.to_string(), verdict, artifact.created_unix_ms],
        )?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'verification_completed', ?2, ?3, ?4)",
            params![
                artifact.change_id.to_string(),
                event_digest.to_string(),
                event_json,
                artifact.created_unix_ms
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_patch_attempt(
        &self,
        change_id: &ChangeId,
        patch_id: PatchId,
        attempt: &serde_json::Value,
        usage: &serde_json::Value,
        cost_microusd: Option<u64>,
        created_unix_ms: u64,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        let patch_exists: u64 = connection.query_row(
            "SELECT COUNT(*) FROM patch_artifacts WHERE patch_id=?1 AND change_id=?2",
            params![patch_id.to_string(), change_id.to_string()],
            |row| row.get(0),
        )?;
        if patch_exists != 1 {
            return Err(StoreError::PatchArtifact(
                "patch attempt references an unknown patch".to_owned(),
            ));
        }
        connection.execute(
            "INSERT INTO change_attempts(
                change_id, patch_id, role, attempt_json, usage_json, cost_microusd,
                created_unix_ms
             ) VALUES(?1, ?2, 'patcher', ?3, ?4, ?5, ?6)",
            params![
                change_id.to_string(),
                patch_id.to_string(),
                serde_json::to_string(attempt)?,
                serde_json::to_string(usage)?,
                cost_microusd,
                created_unix_ms
            ],
        )?;
        Ok(())
    }

    pub fn change_attempts(
        &self,
        change_id: &ChangeId,
    ) -> Result<Vec<ChangeAttemptRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT role, patch_id, attempt_json, usage_json, cost_microusd, created_unix_ms
             FROM change_attempts WHERE change_id=?1 ORDER BY attempt_id",
        )?;
        let rows = statement.query_map([change_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, u64>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (role, patch_id, attempt, usage, cost_microusd, created_unix_ms) = row?;
            Ok(ChangeAttemptRecord {
                role,
                patch_id: PatchId(
                    Digest::parse(&patch_id).map_err(|_| StoreError::Digest(patch_id))?,
                ),
                attempt: serde_json::from_str(&attempt)?,
                usage: serde_json::from_str(&usage)?,
                cost_microusd,
                created_unix_ms,
            })
        })
        .collect()
    }

    pub fn latest_verification_artifact(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<VerificationArtifact>, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT v.artifact_json
                 FROM verification_artifacts v
                 JOIN change_requests c ON c.change_id=v.change_id
                 JOIN patch_artifacts p
                   ON p.change_id=c.change_id AND p.revision=c.latest_patch_revision
                 WHERE v.change_id=?1 AND v.patch_id=p.patch_id
                 ORDER BY v.created_unix_ms DESC, v.verification_id LIMIT 1",
                [change_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(StoreError::from)).transpose()
    }

    pub fn change_digest(&self, change_id: &ChangeId) -> Result<Option<Digest>, StoreError> {
        let Some(prepared) = self.prepared_change(change_id)? else {
            return Ok(None);
        };
        let verification = self.latest_verification_artifact(change_id)?;
        Ok(Some(change_state_digest(&prepared, verification.as_ref())))
    }

    pub fn begin_change_apply(
        &self,
        record: &ChangeApplyRecord,
        journal: &serde_json::Value,
        expected_change_digest: Digest,
    ) -> Result<(), StoreError> {
        if record.status != ChangeApplyStatus::Applying
            || record.post_snapshot.is_some()
            || record.completed_unix_ms.is_some()
        {
            return Err(StoreError::PatchArtifact("apply record is not pending".to_owned()));
        }
        let status = apply_status_name(record.status);
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current_row: Option<PreparedChangeRow> = transaction
            .query_row(
                "SELECT c.request_json, c.request_digest, c.repository_id,
                        c.source_snapshot_digest, c.state, p.artifact_json,
                        p.declared_output_json, c.repair_attempted, c.created_unix_ms
                 FROM change_requests c
                 JOIN patch_artifacts p
                   ON p.change_id=c.change_id AND p.revision=c.latest_patch_revision
                 WHERE c.change_id=?1",
                [record.change_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            request,
            request_digest,
            repository_id,
            snapshot,
            state,
            patch,
            output,
            repair_attempted,
            created,
        )) = current_row
        else {
            return Err(StoreError::ChangeConflict(record.change_id.to_string()));
        };
        let prepared = PreparedChangeRecord {
            request: serde_json::from_str(&request)?,
            request_digest: Digest::parse(&request_digest)
                .map_err(|_| StoreError::Digest(request_digest))?,
            repository_id: Digest::parse(&repository_id)
                .map_err(|_| StoreError::Digest(repository_id))?,
            source_snapshot: Digest::parse(&snapshot).map_err(|_| StoreError::Digest(snapshot))?,
            state,
            patch: serde_json::from_str(&patch)?,
            declared_output: serde_json::from_str(&output)?,
            repair_attempted,
            created_unix_ms: created,
        };
        let verification_json: Option<String> = transaction
            .query_row(
                "SELECT artifact_json FROM verification_artifacts
                 WHERE change_id=?1 AND patch_id=?2
                 ORDER BY created_unix_ms DESC, verification_id LIMIT 1",
                params![record.change_id.to_string(), prepared.patch.id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let verification = verification_json
            .map(|json| serde_json::from_str::<VerificationArtifact>(&json))
            .transpose()?;
        if prepared.state != "verified"
            || verification.as_ref().is_none_or(|artifact| {
                artifact.patch_id != prepared.patch.id
                    || artifact.verdict != VerificationStatus::Verified
                    || !artifact.is_canonical()
            })
            || change_state_digest(&prepared, verification.as_ref()) != expected_change_digest
        {
            return Err(StoreError::ChangeConflict(record.change_id.to_string()));
        }
        if prepared.patch.id != record.patch_id || prepared.source_snapshot != record.pre_snapshot {
            return Err(StoreError::ChangeConflict(record.change_id.to_string()));
        }
        let existing: u64 = transaction.query_row(
            "SELECT COUNT(*) FROM change_applies
             WHERE change_id=?1 AND status IN ('applying', 'applied')",
            [record.change_id.to_string()],
            |row| row.get(0),
        )?;
        if existing != 0 {
            return Err(StoreError::ChangeConflict(record.change_id.to_string()));
        }
        transaction.execute(
            "INSERT INTO change_applies(
                apply_id, change_id, patch_id, repository_root, pre_snapshot_digest,
                post_snapshot_digest, status, journal_json, created_unix_ms,
                completed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, NULL)",
            params![
                record.id.to_string(),
                record.change_id.to_string(),
                record.patch_id.to_string(),
                record.repository_root,
                record.pre_snapshot.to_string(),
                status,
                serde_json::to_string(journal)?,
                record.created_unix_ms
            ],
        )?;
        transaction.execute(
            "UPDATE change_requests SET state='applying', updated_unix_ms=?2 WHERE change_id=?1",
            params![record.change_id.to_string(), record.created_unix_ms],
        )?;
        let event_json = serde_json::to_string(&serde_json::json!({
            "apply_id": record.id,
            "patch_id": record.patch_id,
            "state": "applying"
        }))?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'apply_started', ?2, ?3, ?4)",
            params![
                record.change_id.to_string(),
                Digest::blake3(event_json.as_bytes()).to_string(),
                event_json,
                record.created_unix_ms
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_change_apply(
        &self,
        apply_id: ChangeApplyId,
        status: ChangeApplyStatus,
        post_snapshot: Option<Digest>,
        completed_unix_ms: u64,
    ) -> Result<(), StoreError> {
        if status == ChangeApplyStatus::Applying {
            return Err(StoreError::PatchArtifact("cannot finish an apply as pending".to_owned()));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let change_id: String = transaction.query_row(
            "SELECT change_id FROM change_applies WHERE apply_id=?1 AND status='applying'",
            [apply_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE change_applies
             SET status=?2, post_snapshot_digest=?3, completed_unix_ms=?4
             WHERE apply_id=?1 AND status='applying'",
            params![
                apply_id.to_string(),
                apply_status_name(status),
                post_snapshot.map(|digest| digest.to_string()),
                completed_unix_ms
            ],
        )?;
        let change_state = match status {
            ChangeApplyStatus::Applied => "applied",
            ChangeApplyStatus::RolledBack => "verified",
            ChangeApplyStatus::RollbackFailed | ChangeApplyStatus::RecoveryConflict => "failed",
            ChangeApplyStatus::Applying => unreachable!(),
        };
        transaction.execute(
            "UPDATE change_requests SET state=?2, updated_unix_ms=?3 WHERE change_id=?1",
            params![change_id, change_state, completed_unix_ms],
        )?;
        let event_json = serde_json::to_string(&serde_json::json!({
            "apply_id": apply_id,
            "state": apply_status_name(status),
            "post_snapshot": post_snapshot
        }))?;
        transaction.execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json, created_unix_ms
             ) VALUES(?1, 'apply_completed', ?2, ?3, ?4)",
            params![
                change_id,
                Digest::blake3(event_json.as_bytes()).to_string(),
                event_json,
                completed_unix_ms
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pending_change_applies(
        &self,
        repository_root: &str,
    ) -> Result<Vec<ChangeApplyRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT apply_id, change_id, patch_id, repository_root,
                    pre_snapshot_digest, post_snapshot_digest, status,
                    created_unix_ms, completed_unix_ms
             FROM change_applies WHERE repository_root=?1 AND status='applying'
             ORDER BY created_unix_ms, apply_id",
        )?;
        let rows = statement.query_map([repository_root], decode_apply_record)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
    }

    pub fn change_apply(
        &self,
        apply_id: ChangeApplyId,
    ) -> Result<Option<ChangeApplyRecord>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT apply_id, change_id, patch_id, repository_root,
                        pre_snapshot_digest, post_snapshot_digest, status,
                        created_unix_ms, completed_unix_ms
                 FROM change_applies WHERE apply_id=?1",
                [apply_id.to_string()],
                decode_apply_record,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn change_applies(
        &self,
        change_id: &ChangeId,
    ) -> Result<Vec<ChangeApplyRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT apply_id, change_id, patch_id, repository_root,
                    pre_snapshot_digest, post_snapshot_digest, status,
                    created_unix_ms, completed_unix_ms
             FROM change_applies WHERE change_id=?1 ORDER BY created_unix_ms, apply_id",
        )?;
        statement
            .query_map([change_id.to_string()], decode_apply_record)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

fn change_state_digest(
    prepared: &PreparedChangeRecord,
    verification: Option<&VerificationArtifact>,
) -> Digest {
    let mut hasher = CanonicalHasher::new(b"needle-change-state");
    hasher.field_str(prepared.patch.change_id.as_str());
    hasher.field_digest(prepared.request_digest);
    hasher.field_digest(prepared.source_snapshot);
    hasher.field_digest(prepared.patch.id.0);
    hasher.field_u32(prepared.patch.revision);
    hasher.field_str(&prepared.state);
    if let Some(verification) = verification {
        hasher.field_u8(1);
        hasher.field_digest(verification.id.0);
        hasher.field_u8(match verification.verdict {
            VerificationStatus::NotRequested => 0,
            VerificationStatus::Verified => 1,
            VerificationStatus::Rejected => 2,
            VerificationStatus::Repairable => 3,
            VerificationStatus::Inconclusive => 4,
        });
    } else {
        hasher.field_u8(0);
    }
    hasher.finish()
}

fn apply_status_name(status: ChangeApplyStatus) -> &'static str {
    match status {
        ChangeApplyStatus::Applying => "applying",
        ChangeApplyStatus::Applied => "applied",
        ChangeApplyStatus::RolledBack => "rolled_back",
        ChangeApplyStatus::RollbackFailed => "rollback_failed",
        ChangeApplyStatus::RecoveryConflict => "recovery_conflict",
    }
}

fn decode_apply_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeApplyRecord> {
    let digest = |value: String| {
        Digest::parse(&value).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                "invalid digest".into(),
            )
        })
    };
    let status: String = row.get(6)?;
    let status = match status.as_str() {
        "applying" => ChangeApplyStatus::Applying,
        "applied" => ChangeApplyStatus::Applied,
        "rolled_back" => ChangeApplyStatus::RolledBack,
        "rollback_failed" => ChangeApplyStatus::RollbackFailed,
        "recovery_conflict" => ChangeApplyStatus::RecoveryConflict,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                "invalid apply status".into(),
            ));
        }
    };
    let apply_id = ChangeApplyId(digest(row.get(0)?)?);
    let change_id = ChangeId::parse(&row.get::<_, String>(1)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, error.into())
    })?;
    let patch_id = PatchId(digest(row.get(2)?)?);
    let pre_snapshot = digest(row.get(4)?)?;
    let post_snapshot = row.get::<_, Option<String>>(5)?.map(digest).transpose()?;
    Ok(ChangeApplyRecord {
        id: apply_id,
        change_id,
        patch_id,
        repository_root: row.get(3)?,
        pre_snapshot,
        post_snapshot,
        status,
        created_unix_ms: row.get(7)?,
        completed_unix_ms: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        AcceptanceCoverage, AcceptanceStatus, AllowedPath, AllowedPathScope, PatchFile,
        PatchOperation,
    };
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn failed_request_is_audited_before_any_patch_exists() {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir()
            .join(format!("needle-change-request-audit-{}-{suffix}.sqlite3", std::process::id()));
        let store = RuntimeStore::new(&path);
        store.initialize().unwrap();
        let request = ChangeRequest {
            task: "Update the fixture.".to_owned(),
            acceptance_criteria: vec!["The fixture changes.".to_owned()],
            allowed_paths: vec![AllowedPath {
                path: "fixture.txt".to_owned(),
                scope: AllowedPathScope::Exact,
            }],
            artifact_ids: Vec::new(),
            claim_ids: Vec::new(),
            constraints: Vec::new(),
        };
        let source = Digest::blake3(b"source");
        let request_digest = request.digest(source);
        let change_id = ChangeId::from_digest(Digest::blake3(b"failed-change"));
        store
            .record_change_request(
                &change_id,
                Digest::blake3(b"repository"),
                source,
                request_digest,
                &request,
            )
            .unwrap();
        store.record_change_failure(&change_id, "worker failed").unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        let (state, revision): (String, u32) = connection
            .query_row(
                "SELECT state, latest_patch_revision FROM change_requests WHERE change_id=?1",
                [change_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(revision, 0);
        let events: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM change_events WHERE change_id=?1",
                [change_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 2);
        drop(connection);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn repair_reservation_is_atomic_across_store_connections() {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir()
            .join(format!("needle-change-repair-race-{}-{suffix}.sqlite3", std::process::id()));
        let store = RuntimeStore::new(&path);
        store.initialize().unwrap();
        let request = ChangeRequest {
            task: "Update the fixture.".to_owned(),
            acceptance_criteria: vec!["The fixture changes.".to_owned()],
            allowed_paths: vec![AllowedPath {
                path: "fixture.txt".to_owned(),
                scope: AllowedPathScope::Exact,
            }],
            artifact_ids: Vec::new(),
            claim_ids: Vec::new(),
            constraints: Vec::new(),
        };
        let source = Digest::blake3(b"source");
        let repository = Digest::blake3(b"repository");
        let request_digest = request.digest(source);
        let change_id = ChangeId::from_digest(Digest::blake3(b"repair-race"));
        store
            .record_change_request(&change_id, repository, source, request_digest, &request)
            .unwrap();
        let before = b"before\n".to_vec();
        let after = b"after\n".to_vec();
        let files = vec![PatchFile {
            path: "fixture.txt".to_owned(),
            operation: PatchOperation::Update,
            before_digest: Some(Digest::blake3(&before)),
            after_digest: Some(Digest::blake3(&after)),
            before_bytes: before.len() as u64,
            after_bytes: after.len() as u64,
        }];
        let patch_id = PatchArtifact::compute_id(source, &files);
        let coverage = vec![AcceptanceCoverage {
            criterion: request.acceptance_criteria[0].clone(),
            status: AcceptanceStatus::Partial,
            evidence: "requires one repair".to_owned(),
        }];
        let patch = PatchArtifact {
            id: patch_id,
            change_id: change_id.clone(),
            revision: 1,
            source_snapshot: source,
            files,
            summary: "Initial patch".to_owned(),
            acceptance_coverage: coverage.clone(),
            residual_risks: Vec::new(),
            declared_output_digest: Digest::blake3(b"declared"),
            discrepancies: Vec::new(),
        };
        store
            .record_prepared_change(
                repository,
                request_digest,
                &request,
                &patch,
                &serde_json::json!({"summary": "Initial patch"}),
                &[PatchFileBlob {
                    path: "fixture.txt".to_owned(),
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        let definition = Digest::blake3(b"verifier");
        let findings = vec!["repair this".to_owned()];
        let verification = VerificationArtifact {
            id: VerificationArtifact::compute_id(
                &change_id,
                patch_id,
                VerificationStatus::Repairable,
                &coverage,
                &findings,
                &[],
                definition,
            ),
            change_id: change_id.clone(),
            patch_id,
            verdict: VerificationStatus::Repairable,
            acceptance_coverage: coverage,
            findings,
            test_evidence_ids: Vec::new(),
            test_plan_results: Vec::new(),
            test_plans_over_cap: false,
            verifier_definition: definition,
            created_unix_ms: 1,
        };
        store
            .record_verification_artifact(
                &verification,
                &serde_json::json!({}),
                &serde_json::json!({}),
                None,
            )
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let change_id = change_id.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let store = RuntimeStore::new(path);
                    store.initialize().unwrap();
                    barrier.wait();
                    store.begin_change_repair(&change_id, patch_id)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_err()).count(), 1);
        let prepared = store.prepared_change(&change_id).unwrap().unwrap();
        assert_eq!(prepared.state, "repairing");
        assert!(prepared.repair_attempted);

        drop(store);
        fs::remove_file(path).unwrap();
    }
}
