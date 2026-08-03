use super::{RuntimeStore, StoreError, parse_digest};
use crate::{ClaimAdvisoryPlan, ClaimProofMaterial, ProofCandidate};
use needle_core::claim::Claim as SemanticClaim;
use needle_core::{
    Artifact, ArtifactId, ArtifactValidationCertificate, ClaimKind, ClaimOrigin, ClaimRelation,
    ClaimSetCertificate, ClaimValidationCertificate, Digest, Need, NeedId, Obligation,
};
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};

impl RuntimeStore {
    pub fn semantic_claim(
        &self,
        id: needle_core::ClaimId,
    ) -> Result<Option<SemanticClaim>, StoreError> {
        let connection = self.connection()?;
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT kind, contract_definition_digest, payload_json
                 FROM semantic_claims WHERE claim_id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(kind, contract, payload)| {
            decode_claim(&id.to_string(), &kind, &contract, &payload)
        })
        .transpose()
    }

    pub fn claim_validation_certificate_for_claim(
        &self,
        id: needle_core::ClaimId,
    ) -> Result<Option<ClaimValidationCertificate>, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT certificate_json FROM claim_validation_certificates
                 WHERE claim_id=?1 ORDER BY issued_unix_ms DESC, certificate_id LIMIT 1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(StoreError::from)).transpose()
    }

    pub fn publish_claims_shadow(
        &self,
        artifact: &Artifact,
        artifact_certificate: &ArtifactValidationCertificate,
        claims: &[SemanticClaim],
        origins: &[ClaimOrigin],
        relations: &[ClaimRelation],
        certificates: &[ClaimValidationCertificate],
    ) -> Result<(), StoreError> {
        if claims.is_empty()
            || claims.len() > needle_core::MAX_CLAIMS_PER_ARTIFACT
            || claims.len() != origins.len()
            || claims.len() != certificates.len()
        {
            return Err(StoreError::ArtifactIdentity(
                "claim shadow record violates cardinality bounds".to_owned(),
            ));
        }
        if artifact.id != artifact_certificate.artifact.digest() {
            return Err(StoreError::ArtifactIdentity(
                "claim shadow origin does not match the artifact certificate".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let origin_exists: u64 = transaction.query_row(
            "SELECT COUNT(*) FROM artifact_validation_certificates
             WHERE certificate_id=?1 AND artifact_id=?2",
            params![artifact_certificate.id.to_string(), artifact.id.to_string()],
            |row| row.get(0),
        )?;
        if origin_exists != 1 {
            return Err(StoreError::ArtifactIdentity(
                "claim shadow origin is not persisted".to_owned(),
            ));
        }
        let claim_ids =
            claims.iter().map(|claim| claim.id).collect::<std::collections::BTreeSet<_>>();
        let origin_claim_ids =
            origins.iter().map(|origin| origin.claim).collect::<std::collections::BTreeSet<_>>();
        let certificate_claim_ids = certificates
            .iter()
            .map(|certificate| certificate.claim)
            .collect::<std::collections::BTreeSet<_>>();
        let ordinals =
            origins.iter().map(|origin| origin.ordinal).collect::<std::collections::BTreeSet<_>>();
        if claim_ids.len() != claims.len()
            || origin_claim_ids != claim_ids
            || certificate_claim_ids != claim_ids
            || ordinals.len() != claims.len()
            || relations.len() >= claims.len()
        {
            return Err(StoreError::ArtifactIdentity(
                "claim shadow extraction contains duplicate or unbounded members".to_owned(),
            ));
        }
        for relation in relations {
            if !relation.is_canonical()
                || !claim_ids.contains(&relation.from)
                || !claim_ids.contains(&relation.to)
            {
                return Err(StoreError::ArtifactIdentity(
                    "claim relation is not canonical or references another extraction".to_owned(),
                ));
            }
        }
        for claim in claims {
            if !claim.is_canonical() {
                return Err(StoreError::ArtifactIdentity(
                    "claim identity is not canonical".to_owned(),
                ));
            }
            let origin =
                origins.iter().find(|origin| origin.claim == claim.id).ok_or_else(|| {
                    StoreError::ArtifactIdentity("claim origin is missing".to_owned())
                })?;
            let certificate =
                certificates.iter().find(|certificate| certificate.claim == claim.id).ok_or_else(
                    || StoreError::ArtifactIdentity("claim certificate is missing".to_owned()),
                )?;
            if origin.artifact.digest() != artifact.id
                || origin.validation_certificate != artifact_certificate.id
                || certificate.origin_artifact.digest() != artifact.id
                || certificate.origin_validation_certificate != artifact_certificate.id
                || certificate.subject != origin.subject
                || certificate.world != origin.world
                || !certificate.is_canonical()
            {
                return Err(StoreError::ArtifactIdentity(
                    "claim provenance or certificate is inconsistent".to_owned(),
                ));
            }
            let claim_payload_json = serde_json::to_string(&claim.payload)?;
            transaction.execute(
                "INSERT OR IGNORE INTO semantic_claims(
                    claim_id, kind, contract_definition_digest, payload_json, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    claim.id.to_string(),
                    claim.kind.as_str(),
                    claim.contract_definition.to_string(),
                    claim_payload_json,
                    origin.created_unix_ms,
                ],
            )?;
            let stored_claim: (String, String, String) = transaction.query_row(
                "SELECT kind, contract_definition_digest, payload_json
                 FROM semantic_claims WHERE claim_id=?1",
                [claim.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            if stored_claim.0 != claim.kind.as_str()
                || stored_claim.1 != claim.contract_definition.to_string()
                || stored_claim.2 != claim_payload_json
            {
                return Err(StoreError::ArtifactIdentity(
                    "stored claim content conflicts with its identity".to_owned(),
                ));
            }
            transaction.execute(
                "INSERT OR IGNORE INTO claim_origins(
                    claim_id, artifact_id, validation_certificate_id, subject_id,
                    world_digest, ordinal, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    claim.id.to_string(),
                    artifact.id.to_string(),
                    artifact_certificate.id.to_string(),
                    origin.subject.to_string(),
                    origin.world.to_string(),
                    origin.ordinal,
                    origin.created_unix_ms,
                ],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO claim_validation_certificates(
                    certificate_id, claim_id, origin_artifact_id,
                    origin_validation_certificate_id, subject_id, world_digest,
                    validator_definition_digest, certificate_json, issued_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    certificate.id.to_string(),
                    claim.id.to_string(),
                    artifact.id.to_string(),
                    artifact_certificate.id.to_string(),
                    certificate.subject.to_string(),
                    certificate.world.to_string(),
                    certificate.validator_definition.to_string(),
                    serde_json::to_string(certificate)?,
                    certificate.issued_unix_ms,
                ],
            )?;
            for dependency in &certificate.dependencies {
                transaction.execute(
                    "INSERT OR IGNORE INTO claim_dependencies(
                        claim_id, path, content_digest, byte_start, byte_end
                     ) VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![
                        claim.id.to_string(),
                        dependency.path,
                        dependency.content_digest.to_string(),
                        dependency.byte_start,
                        dependency.byte_end,
                    ],
                )?;
            }
            for obligation in &certificate.obligations {
                transaction.execute(
                    "INSERT OR IGNORE INTO claim_coverage_entries(
                        certificate_id, claim_id, obligation_id, predicate, subject_id,
                        world_digest, coverage_json
                     ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        certificate.id.to_string(),
                        claim.id.to_string(),
                        obligation.id.to_string(),
                        format!("{:?}", obligation.predicate),
                        obligation.subject.to_string(),
                        certificate.world.to_string(),
                        serde_json::to_string(obligation)?,
                    ],
                )?;
            }
        }
        for relation in relations {
            transaction.execute(
                "INSERT OR IGNORE INTO claim_relations(
                    relation_id, from_claim_id, to_claim_id, relation_kind,
                    relation_json, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    relation.id.to_string(),
                    relation.from.to_string(),
                    relation.to.to_string(),
                    relation.kind.as_str(),
                    serde_json::to_string(relation)?,
                    artifact.created_unix_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn semantic_claims_for_artifact(
        &self,
        artifact: ArtifactId,
    ) -> Result<Vec<SemanticClaim>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT c.claim_id, c.kind, c.contract_definition_digest, c.payload_json
             FROM semantic_claims c
             JOIN claim_origins o ON o.claim_id=c.claim_id
             WHERE o.artifact_id=?1
             ORDER BY o.ordinal, c.claim_id
             LIMIT 32",
        )?;
        let rows = statement.query_map([artifact.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut claims = Vec::new();
        for row in rows {
            let (id, kind, contract, payload) = row?;
            let claim = SemanticClaim {
                id: needle_core::ClaimId(Digest::parse(&id).map_err(|_| {
                    StoreError::ArtifactIdentity("stored claim id is invalid".to_owned())
                })?),
                kind: ClaimKind::parse(&kind).ok_or_else(|| {
                    StoreError::ArtifactIdentity("stored claim kind is invalid".to_owned())
                })?,
                contract_definition: Digest::parse(&contract).map_err(|_| {
                    StoreError::ArtifactIdentity(
                        "stored claim contract digest is invalid".to_owned(),
                    )
                })?,
                payload: serde_json::from_str(&payload)?,
            };
            if !claim.is_canonical() {
                return Err(StoreError::ArtifactIdentity(
                    "stored claim identity is not canonical".to_owned(),
                ));
            }
            claims.push(claim);
        }
        Ok(claims)
    }

    pub fn claim_proof_material_for_artifacts(
        &self,
        artifacts: &[ArtifactId],
    ) -> Result<ClaimProofMaterial, StoreError> {
        if artifacts.is_empty() || artifacts.len() > needle_core::MAX_CLAIM_ORIGINS {
            return Err(StoreError::ArtifactIdentity(
                "claim proof artifact origin bound is invalid".to_owned(),
            ));
        }
        let connection = self.connection()?;
        let mut claims = BTreeMap::new();
        let mut certificates = BTreeMap::new();
        for artifact in artifacts {
            let mut statement = connection.prepare_cached(
                "SELECT c.claim_id, c.kind, c.contract_definition_digest, c.payload_json,
                        vc.certificate_id, vc.certificate_json
                 FROM claim_origins o
                 JOIN semantic_claims c ON c.claim_id=o.claim_id
                 JOIN claim_validation_certificates vc
                   ON vc.claim_id=o.claim_id
                  AND vc.origin_artifact_id=o.artifact_id
                  AND vc.origin_validation_certificate_id=o.validation_certificate_id
                 WHERE o.artifact_id=?1
                 ORDER BY o.ordinal, c.claim_id, vc.certificate_id
                 LIMIT 64",
            )?;
            let rows = statement.query_map([artifact.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (claim_id, kind, contract, payload, certificate_id, certificate_json) = row?;
                let claim = decode_claim(&claim_id, &kind, &contract, &payload)?;
                let certificate: ClaimValidationCertificate =
                    serde_json::from_str(&certificate_json)?;
                if certificate.id.to_string() != certificate_id
                    || certificate.claim != claim.id
                    || !certificate.is_canonical()
                {
                    return Err(StoreError::ArtifactIdentity(
                        "stored claim certificate is not canonical".to_owned(),
                    ));
                }
                claims.insert(claim.id, claim);
                certificates.insert(certificate.id, certificate);
            }
        }
        if claims.is_empty()
            || claims.len() > needle_core::MAX_CLAIM_CANDIDATES
            || certificates.len() > needle_core::MAX_CLAIM_CANDIDATES
        {
            return Err(StoreError::ArtifactIdentity(
                "stored claim proof material exceeds its candidate bound".to_owned(),
            ));
        }
        let claim_ids = claims.keys().copied().collect::<BTreeSet<_>>();
        let relations = load_relations(&connection, &claim_ids)?;
        Ok(ClaimProofMaterial {
            claims: claims.into_values().collect(),
            relations,
            certificates: certificates.into_values().collect(),
        })
    }

    pub fn claim_origin_artifacts_for_need(
        &self,
        need: &Need,
    ) -> Result<Vec<ArtifactId>, StoreError> {
        let connection = self.connection()?;
        let mut artifacts = BTreeSet::new();
        let mut statement = connection.prepare_cached(
            "SELECT vc.origin_artifact_id, coverage.coverage_json
             FROM claim_coverage_entries coverage
             JOIN claim_validation_certificates vc
               ON vc.certificate_id=coverage.certificate_id
             WHERE coverage.predicate=?1
               AND coverage.subject_id=?2
               AND coverage.world_digest=?3
             ORDER BY vc.origin_artifact_id, coverage.certificate_id
             LIMIT 64",
        )?;
        for requested in &need.required {
            let rows = statement.query_map(
                params![
                    format!("{:?}", requested.predicate),
                    requested.subject.to_string(),
                    need.world.id().to_string(),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            for row in rows {
                let (artifact, coverage_json) = row?;
                let provided: Obligation = serde_json::from_str(&coverage_json)?;
                if provided.satisfies(requested) {
                    artifacts.insert(ArtifactId(parse_digest(&artifact)?));
                }
            }
        }
        if artifacts.len() > needle_core::MAX_CLAIM_CANDIDATES {
            return Err(StoreError::ArtifactIdentity(
                "claim origin candidates exceed the proof bound".to_owned(),
            ));
        }
        Ok(artifacts.into_iter().collect())
    }

    pub fn publish_claim_set_shadow(
        &self,
        certificate: &ClaimSetCertificate,
    ) -> Result<(), StoreError> {
        if !certificate.is_canonical() {
            return Err(StoreError::ArtifactIdentity(
                "claim-set certificate is not canonical".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let need_exists: u64 = transaction.query_row(
            "SELECT COUNT(*) FROM needs WHERE need_id=?1",
            [certificate.need.to_string()],
            |row| row.get(0),
        )?;
        if need_exists != 1 {
            return Err(StoreError::ArtifactIdentity("claim-set need is not persisted".to_owned()));
        }
        for (claim, validation_certificate) in
            certificate.claims.iter().zip(&certificate.validation_certificates)
        {
            let member_exists: u64 = transaction.query_row(
                "SELECT COUNT(*) FROM claim_validation_certificates
                 WHERE certificate_id=?1 AND claim_id=?2",
                params![validation_certificate.to_string(), claim.to_string()],
                |row| row.get(0),
            )?;
            if member_exists != 1 {
                return Err(StoreError::ArtifactIdentity(
                    "claim-set member is not persisted".to_owned(),
                ));
            }
        }
        let certificate_json = serde_json::to_string(certificate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO claim_set_certificates(
                certificate_id, need_id, engine_definition_digest, world_digest,
                certificate_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                certificate.id.to_string(),
                certificate.need.to_string(),
                certificate.engine_definition.to_string(),
                certificate.world.to_string(),
                certificate_json,
                certificate.created_unix_ms,
            ],
        )?;
        let stored: (String, String, String, String) = transaction.query_row(
            "SELECT need_id, engine_definition_digest, world_digest, certificate_json
             FROM claim_set_certificates WHERE certificate_id=?1",
            [certificate.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let stored_certificate: ClaimSetCertificate = serde_json::from_str(&stored.3)?;
        if stored.0 != certificate.need.to_string()
            || stored.1 != certificate.engine_definition.to_string()
            || stored.2 != certificate.world.to_string()
            || !stored_certificate.is_canonical()
            || stored_certificate.id != certificate.id
            || stored_certificate.need != certificate.need
            || stored_certificate.claims != certificate.claims
            || stored_certificate.validation_certificates != certificate.validation_certificates
            || stored_certificate.obligations != certificate.obligations
            || stored_certificate.world != certificate.world
            || stored_certificate.engine_definition != certificate.engine_definition
        {
            return Err(StoreError::ArtifactIdentity(
                "stored claim-set content conflicts with its identity".to_owned(),
            ));
        }
        for (position, (claim, validation_certificate)) in
            certificate.claims.iter().zip(&certificate.validation_certificates).enumerate()
        {
            transaction.execute(
                "INSERT OR IGNORE INTO claim_set_members(
                    certificate_id, position, claim_id, claim_validation_certificate_id
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    certificate.id.to_string(),
                    position,
                    claim.to_string(),
                    validation_certificate.to_string(),
                ],
            )?;
        }
        validate_claim_set_members(&transaction, certificate)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_claim_advisory_plan(
        &self,
        advisory: &ClaimAdvisoryPlan,
        candidates: &[ProofCandidate],
    ) -> Result<(), StoreError> {
        if candidates.len() > needle_core::MAX_PROOF_CANDIDATES {
            return Err(StoreError::ArtifactIdentity(
                "claim advisory candidates exceed the proof bound".to_owned(),
            ));
        }
        self.publish_claim_set_shadow(&advisory.certificate)?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR REPLACE INTO selected_plans(
                plan_id, need_id, resolution, plan_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                advisory.selected.id.to_string(),
                advisory.selected.need.to_string(),
                advisory.selected.decision_reason,
                serde_json::to_string(&advisory.selected)?,
                super::now_ms(),
            ],
        )?;
        for (position, candidate) in candidates.iter().enumerate() {
            transaction.execute(
                "INSERT OR REPLACE INTO plan_candidates(
                    plan_id, position, candidate_json, selected
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    advisory.selected.id.to_string(),
                    position,
                    serde_json::to_string(candidate)?,
                    advisory.selected_bits & (1_u64 << position) != 0,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn claim_set_certificate_for_need(
        &self,
        need: NeedId,
    ) -> Result<Option<ClaimSetCertificate>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT certificate_json FROM claim_set_certificates
             WHERE need_id=?1 ORDER BY created_unix_ms DESC, certificate_id LIMIT 1",
        )?;
        let mut rows = statement.query([need.to_string()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let certificate: ClaimSetCertificate = serde_json::from_str(&row.get::<_, String>(0)?)?;
        if !certificate.is_canonical() || certificate.need != need {
            return Err(StoreError::ArtifactIdentity(
                "stored claim-set certificate is not canonical".to_owned(),
            ));
        }
        validate_claim_set_members(&connection, &certificate)?;
        Ok(Some(certificate))
    }

    pub fn claim_proof_material_for_certificate(
        &self,
        certificate: &ClaimSetCertificate,
    ) -> Result<ClaimProofMaterial, StoreError> {
        if !certificate.is_canonical() {
            return Err(StoreError::ArtifactIdentity(
                "claim-set certificate is not canonical".to_owned(),
            ));
        }
        let connection = self.connection()?;
        validate_claim_set_members(&connection, certificate)?;
        let mut claims = BTreeMap::new();
        let mut certificates = BTreeMap::new();
        for (claim_id, certificate_id) in
            certificate.claims.iter().zip(&certificate.validation_certificates)
        {
            let (kind, contract, payload, certificate_json): (String, String, String, String) =
                connection.query_row(
                    "SELECT c.kind, c.contract_definition_digest, c.payload_json,
                            vc.certificate_json
                     FROM semantic_claims c
                     JOIN claim_validation_certificates vc ON vc.claim_id=c.claim_id
                     WHERE c.claim_id=?1 AND vc.certificate_id=?2",
                    params![claim_id.to_string(), certificate_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
            let claim = decode_claim(&claim_id.to_string(), &kind, &contract, &payload)?;
            let validation_certificate: ClaimValidationCertificate =
                serde_json::from_str(&certificate_json)?;
            if validation_certificate.id != *certificate_id
                || validation_certificate.claim != *claim_id
                || !validation_certificate.is_canonical()
            {
                return Err(StoreError::ArtifactIdentity(
                    "stored claim-set member is not canonical".to_owned(),
                ));
            }
            claims.insert(claim.id, claim);
            certificates.insert(validation_certificate.id, validation_certificate);
        }
        let claim_ids = claims.keys().copied().collect::<BTreeSet<_>>();
        let relations = load_relations(&connection, &claim_ids)?;
        Ok(ClaimProofMaterial {
            claims: claims.into_values().collect(),
            relations,
            certificates: certificates.into_values().collect(),
        })
    }
}

fn decode_claim(
    id: &str,
    kind: &str,
    contract: &str,
    payload: &str,
) -> Result<SemanticClaim, StoreError> {
    let claim =
        SemanticClaim {
            id: needle_core::ClaimId(Digest::parse(id).map_err(|_| {
                StoreError::ArtifactIdentity("stored claim id is invalid".to_owned())
            })?),
            kind: ClaimKind::parse(kind).ok_or_else(|| {
                StoreError::ArtifactIdentity("stored claim kind is invalid".to_owned())
            })?,
            contract_definition: Digest::parse(contract).map_err(|_| {
                StoreError::ArtifactIdentity("stored claim contract digest is invalid".to_owned())
            })?,
            payload: serde_json::from_str(payload)?,
        };
    if !claim.is_canonical() {
        return Err(StoreError::ArtifactIdentity(
            "stored claim identity is not canonical".to_owned(),
        ));
    }
    Ok(claim)
}

fn load_relations(
    connection: &rusqlite::Connection,
    claim_ids: &BTreeSet<needle_core::ClaimId>,
) -> Result<Vec<ClaimRelation>, StoreError> {
    let mut relations = BTreeMap::new();
    let mut statement = connection.prepare_cached(
        "SELECT relation_id, relation_json FROM claim_relations
         WHERE from_claim_id=?1 ORDER BY relation_id LIMIT 64",
    )?;
    for claim in claim_ids {
        let rows = statement.query_map([claim.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, relation_json) = row?;
            let relation: ClaimRelation = serde_json::from_str(&relation_json)?;
            if relation.id.to_string() != id
                || !relation.is_canonical()
                || !claim_ids.contains(&relation.to)
            {
                continue;
            }
            relations.insert(relation.id, relation);
        }
    }
    if relations.len() > needle_core::MAX_CLAIM_CANDIDATES {
        return Err(StoreError::ArtifactIdentity(
            "stored claim relations exceed the proof bound".to_owned(),
        ));
    }
    Ok(relations.into_values().collect())
}

fn validate_claim_set_members(
    connection: &rusqlite::Connection,
    certificate: &ClaimSetCertificate,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "SELECT position, claim_id, claim_validation_certificate_id
         FROM claim_set_members WHERE certificate_id=?1 ORDER BY position LIMIT 17",
    )?;
    let rows = statement.query_map([certificate.id.to_string()], |row| {
        Ok((row.get::<_, usize>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    let mut observed = 0_usize;
    for row in rows {
        let (position, claim, validation_certificate) = row?;
        if position != observed
            || certificate.claims.get(position).map(ToString::to_string).as_deref()
                != Some(claim.as_str())
            || certificate.validation_certificates.get(position).map(ToString::to_string).as_deref()
                != Some(validation_certificate.as_str())
        {
            return Err(StoreError::ArtifactIdentity(
                "stored claim-set membership conflicts with its certificate".to_owned(),
            ));
        }
        observed += 1;
    }
    if observed != certificate.claims.len() {
        return Err(StoreError::ArtifactIdentity(
            "stored claim-set membership is incomplete".to_owned(),
        ));
    }
    Ok(())
}
