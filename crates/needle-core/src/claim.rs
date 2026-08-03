use crate::{
    ArtifactId, ArtifactValidationCertificateId, CanonicalHasher, Dependency, Digest, FlowStepRole,
    LocationRole, NeedId, Obligation, ObligationId, SemanticFlowStep, SemanticLocation, SubjectId,
};
use arrayvec::ArrayVec;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_CLAIMS_PER_ARTIFACT: usize = 32;
pub const MAX_CLAIM_CANDIDATES: usize = 64;
pub const MAX_SELECTED_CLAIMS: usize = 16;
pub const MAX_CLAIM_ORIGINS: usize = 8;

macro_rules! claim_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Digest);

        impl $name {
            pub const fn new(digest: Digest) -> Self {
                Self(digest)
            }

            pub const fn digest(self) -> Digest {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

claim_id!(ClaimId);
claim_id!(ClaimValidationCertificateId);
claim_id!(ClaimSetCertificateId);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimKind {
    ImplementationLocation,
    RuntimeFlowStep,
    FocusedTest,
}

impl ClaimKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImplementationLocation => "implementation-location",
            Self::RuntimeFlowStep => "runtime-flow-step",
            Self::FocusedTest => "focused-test",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "implementation-location" => Some(Self::ImplementationLocation),
            "runtime-flow-step" => Some(Self::RuntimeFlowStep),
            "focused-test" => Some(Self::FocusedTest),
            _ => None,
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::ImplementationLocation => 0,
            Self::RuntimeFlowStep => 1,
            Self::FocusedTest => 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClaimPayload {
    ImplementationLocation {
        location: SemanticLocation,
    },
    RuntimeFlowStep {
        scenario: String,
        flow_anchor: Digest,
        step: SemanticFlowStep,
    },
    FocusedTest {
        runner: String,
        argv: Vec<String>,
        cwd_relative: String,
        identifier: String,
        selection: String,
        evidence_paths: Vec<String>,
    },
}

impl ClaimPayload {
    pub const fn claim_kind(&self) -> ClaimKind {
        match self {
            Self::ImplementationLocation { .. } => ClaimKind::ImplementationLocation,
            Self::RuntimeFlowStep { .. } => ClaimKind::RuntimeFlowStep,
            Self::FocusedTest { .. } => ClaimKind::FocusedTest,
        }
    }
}

pub fn runtime_flow_anchor(scenario: &str, steps: &[SemanticFlowStep]) -> Option<Digest> {
    if scenario.is_empty() || steps.is_empty() || steps.len() > MAX_CLAIMS_PER_ARTIFACT {
        return None;
    }
    let mut hasher = CanonicalHasher::new(b"runtime-flow-anchor");
    hasher.field_str(scenario);
    for step in steps {
        hash_flow_step(&mut hasher, step);
    }
    Some(hasher.finish())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub id: ClaimId,
    pub kind: ClaimKind,
    pub contract_definition: Digest,
    pub payload: ClaimPayload,
}

impl Claim {
    pub fn new(contract_definition: Digest, mut payload: ClaimPayload) -> Option<Self> {
        canonicalize_claim_payload(&mut payload)?;
        let kind = payload.claim_kind();
        let id = compute_claim_id(kind, contract_definition, &payload)?;
        Some(Self { id, kind, contract_definition, payload })
    }

    pub fn is_canonical(&self) -> bool {
        self.kind == self.payload.claim_kind()
            && claim_payload_is_canonical(&self.payload)
            && compute_claim_id(self.kind, self.contract_definition, &self.payload) == Some(self.id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimOrigin {
    pub claim: ClaimId,
    pub artifact: ArtifactId,
    pub validation_certificate: ArtifactValidationCertificateId,
    pub subject: SubjectId,
    pub world: Digest,
    pub ordinal: u16,
    pub created_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimRelationKind {
    Precedes,
}

impl ClaimRelationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Precedes => "precedes",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Precedes => 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRelation {
    pub id: Digest,
    pub from: ClaimId,
    pub to: ClaimId,
    pub kind: ClaimRelationKind,
}

impl ClaimRelation {
    pub fn new(from: ClaimId, to: ClaimId, kind: ClaimRelationKind) -> Self {
        let mut hasher = CanonicalHasher::new(b"claim-relation");
        hasher.field_digest(from.digest());
        hasher.field_digest(to.digest());
        hasher.field_u8(kind.tag());
        Self { id: hasher.finish(), from, to, kind }
    }

    pub fn is_canonical(&self) -> bool {
        Self::new(self.from, self.to, self.kind).id == self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimValidationCertificate {
    pub id: ClaimValidationCertificateId,
    pub claim: ClaimId,
    pub origin_artifact: ArtifactId,
    pub origin_validation_certificate: ArtifactValidationCertificateId,
    pub subject: SubjectId,
    pub world: Digest,
    pub validator_definition: Digest,
    pub dependencies: Vec<Dependency>,
    pub obligations: Vec<Obligation>,
    pub issued_unix_ms: u64,
}

impl ClaimValidationCertificate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        claim: ClaimId,
        origin_artifact: ArtifactId,
        origin_validation_certificate: ArtifactValidationCertificateId,
        subject: SubjectId,
        world: Digest,
        validator_definition: Digest,
        mut dependencies: Vec<Dependency>,
        mut obligations: Vec<Obligation>,
        issued_unix_ms: u64,
    ) -> Option<Self> {
        if dependencies.is_empty() || dependencies.len() > MAX_CLAIMS_PER_ARTIFACT {
            return None;
        }
        for dependency in &mut dependencies {
            dependency.claims.clear();
        }
        dependencies.sort_by(|left, right| {
            (&left.path, left.content_digest, left.byte_start, left.byte_end).cmp(&(
                &right.path,
                right.content_digest,
                right.byte_start,
                right.byte_end,
            ))
        });
        dependencies.dedup_by(|left, right| {
            left.path == right.path
                && left.content_digest == right.content_digest
                && left.byte_start == right.byte_start
                && left.byte_end == right.byte_end
        });
        obligations.sort_by_key(|obligation| obligation.id);
        obligations.dedup_by_key(|obligation| obligation.id);
        if obligations.is_empty() || obligations.len() > 16 {
            return None;
        }
        let id = compute_claim_validation_certificate_id(
            claim,
            origin_artifact,
            origin_validation_certificate,
            subject,
            world,
            validator_definition,
            &dependencies,
            &obligations,
        );
        Some(Self {
            id,
            claim,
            origin_artifact,
            origin_validation_certificate,
            subject,
            world,
            validator_definition,
            dependencies,
            obligations,
            issued_unix_ms,
        })
    }

    pub fn is_canonical(&self) -> bool {
        self.dependencies.iter().all(|dependency| dependency.claims.is_empty())
            && self
                .dependencies
                .windows(2)
                .all(|pair| dependency_key(&pair[0]) < dependency_key(&pair[1]))
            && self.obligations.windows(2).all(|pair| pair[0].id < pair[1].id)
            && self.obligations.iter().all(|obligation| {
                crate::Obligation::new(
                    obligation.predicate,
                    obligation.subject,
                    obligation.facets.clone(),
                ) == *obligation
            })
            && compute_claim_validation_certificate_id(
                self.claim,
                self.origin_artifact,
                self.origin_validation_certificate,
                self.subject,
                self.world,
                self.validator_definition,
                &self.dependencies,
                &self.obligations,
            ) == self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimSetCertificate {
    pub id: ClaimSetCertificateId,
    pub need: NeedId,
    pub claims: Vec<ClaimId>,
    pub validation_certificates: Vec<ClaimValidationCertificateId>,
    pub obligations: Vec<ObligationId>,
    pub world: Digest,
    pub engine_definition: Digest,
    pub created_unix_ms: u64,
}

impl ClaimSetCertificate {
    pub fn new(
        need: NeedId,
        mut members: Vec<(ClaimId, ClaimValidationCertificateId)>,
        mut obligations: Vec<ObligationId>,
        world: Digest,
        engine_definition: Digest,
        created_unix_ms: u64,
    ) -> Option<Self> {
        members.sort_unstable();
        obligations.sort_unstable();
        if members.is_empty()
            || members.len() > MAX_SELECTED_CLAIMS
            || obligations.is_empty()
            || obligations.len() > 16
            || members.windows(2).any(|pair| pair[0].0 == pair[1].0)
            || obligations.windows(2).any(|pair| pair[0] == pair[1])
        {
            return None;
        }
        let (claims, validation_certificates): (Vec<_>, Vec<_>) = members.into_iter().unzip();
        let id = compute_claim_set_certificate_id(
            need,
            &claims,
            &validation_certificates,
            &obligations,
            world,
            engine_definition,
        );
        Some(Self {
            id,
            need,
            claims,
            validation_certificates,
            obligations,
            world,
            engine_definition,
            created_unix_ms,
        })
    }

    pub fn is_canonical(&self) -> bool {
        if self.claims.is_empty()
            || self.claims.len() > MAX_SELECTED_CLAIMS
            || self.claims.len() != self.validation_certificates.len()
            || self.obligations.is_empty()
            || self.obligations.len() > 16
            || self.claims.windows(2).any(|pair| pair[0] >= pair[1])
            || self.obligations.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return false;
        }
        compute_claim_set_certificate_id(
            self.need,
            &self.claims,
            &self.validation_certificates,
            &self.obligations,
            self.world,
            self.engine_definition,
        ) == self.id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProofComponent {
    Artifact { artifact: ArtifactId, validation_certificate: ArtifactValidationCertificateId },
    Claim { claim: ClaimId, validation_certificate: ClaimValidationCertificateId },
}

fn compute_claim_id(
    kind: ClaimKind,
    contract_definition: Digest,
    payload: &ClaimPayload,
) -> Option<ClaimId> {
    if kind != payload.claim_kind() {
        return None;
    }
    let mut hasher = CanonicalHasher::new(b"semantic-claim");
    hasher.field_u8(kind.tag());
    hasher.field_digest(contract_definition);
    match payload {
        ClaimPayload::ImplementationLocation { location } => {
            hash_location(&mut hasher, location);
        }
        ClaimPayload::RuntimeFlowStep { scenario, flow_anchor, step } => {
            hasher.field_str(scenario);
            hasher.field_digest(*flow_anchor);
            hash_flow_step(&mut hasher, step);
        }
        ClaimPayload::FocusedTest {
            runner,
            argv,
            cwd_relative,
            identifier,
            selection,
            evidence_paths,
        } => {
            if argv.len() > 16 || evidence_paths.len() > 8 {
                return None;
            }
            hasher.field_str(runner);
            for argument in argv {
                hasher.field_str(argument);
            }
            hasher.field_str(cwd_relative);
            hasher.field_str(identifier);
            hasher.field_str(selection);
            let mut ordered = ArrayVec::<_, 8>::new();
            for path in evidence_paths {
                ordered.try_push(path).ok()?;
            }
            ordered.sort();
            for path in ordered {
                hasher.field_str(path);
            }
        }
    }
    Some(ClaimId(hasher.finish()))
}

fn canonicalize_claim_payload(payload: &mut ClaimPayload) -> Option<()> {
    if let ClaimPayload::FocusedTest { argv, evidence_paths, .. } = payload {
        if argv.len() > 16 || evidence_paths.len() > 8 {
            return None;
        }
        evidence_paths.sort();
        evidence_paths.dedup();
        if evidence_paths.is_empty() {
            return None;
        }
    }
    Some(())
}

fn claim_payload_is_canonical(payload: &ClaimPayload) -> bool {
    match payload {
        ClaimPayload::FocusedTest { argv, evidence_paths, .. } => {
            argv.len() <= 16
                && !evidence_paths.is_empty()
                && evidence_paths.len() <= 8
                && evidence_paths.windows(2).all(|pair| pair[0] < pair[1])
        }
        _ => true,
    }
}

fn dependency_key(dependency: &Dependency) -> (&str, Digest, Option<u64>, Option<u64>) {
    (
        dependency.path.as_str(),
        dependency.content_digest,
        dependency.byte_start,
        dependency.byte_end,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_claim_validation_certificate_id(
    claim: ClaimId,
    origin_artifact: ArtifactId,
    origin_validation_certificate: ArtifactValidationCertificateId,
    subject: SubjectId,
    world: Digest,
    validator_definition: Digest,
    dependencies: &[Dependency],
    obligations: &[Obligation],
) -> ClaimValidationCertificateId {
    let mut hasher = CanonicalHasher::new(b"claim-validation-certificate");
    hasher.field_digest(claim.digest());
    hasher.field_digest(origin_artifact.digest());
    hasher.field_digest(origin_validation_certificate.digest());
    hasher.field_digest(subject.digest());
    hasher.field_digest(world);
    hasher.field_digest(validator_definition);
    for dependency in dependencies {
        hasher.field_str(&dependency.path);
        hasher.field_digest(dependency.content_digest);
        hash_optional_u64(&mut hasher, dependency.byte_start);
        hash_optional_u64(&mut hasher, dependency.byte_end);
    }
    for obligation in obligations {
        hasher.field_digest(obligation.id.digest());
    }
    ClaimValidationCertificateId(hasher.finish())
}

fn compute_claim_set_certificate_id(
    need: NeedId,
    claims: &[ClaimId],
    validation_certificates: &[ClaimValidationCertificateId],
    obligations: &[ObligationId],
    world: Digest,
    engine_definition: Digest,
) -> ClaimSetCertificateId {
    let mut hasher = CanonicalHasher::new(b"claim-set-certificate");
    hasher.field_digest(need.digest());
    hasher.field_digest(world);
    hasher.field_digest(engine_definition);
    for (claim, certificate) in claims.iter().zip(validation_certificates) {
        hasher.field_digest(claim.digest());
        hasher.field_digest(certificate.digest());
    }
    for obligation in obligations {
        hasher.field_digest(obligation.digest());
    }
    ClaimSetCertificateId(hasher.finish())
}

fn hash_flow_step(hasher: &mut CanonicalHasher, step: &SemanticFlowStep) {
    hasher.field_u8(match step.role {
        FlowStepRole::Producer => 0,
        FlowStepRole::Carrier => 1,
        FlowStepRole::Transformation => 2,
        FlowStepRole::Precedence => 3,
        FlowStepRole::Consumer => 4,
    });
    hash_location(hasher, &step.location);
    hasher.field_str(&step.description);
}

fn hash_location(hasher: &mut CanonicalHasher, location: &SemanticLocation) {
    hasher.field_u8(match location.role {
        LocationRole::Primary => 0,
        LocationRole::Supporting => 1,
    });
    hasher.field_str(&location.path);
    match &location.symbol {
        Some(symbol) => {
            hasher.field_u8(1);
            hasher.field_str(symbol);
        }
        None => hasher.field_u8(0),
    }
    hash_optional_u64(hasher, location.byte_start);
    hash_optional_u64(hasher, location.byte_end);
}

fn hash_optional_u64(hasher: &mut CanonicalHasher, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.field_u8(1);
            hasher.field_bytes(&value.to_le_bytes());
        }
        None => hasher.field_u8(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> SemanticLocation {
        SemanticLocation {
            role: LocationRole::Primary,
            path: "src/lib.rs".to_owned(),
            symbol: Some("answer".to_owned()),
            byte_start: Some(10),
            byte_end: Some(20),
        }
    }

    #[test]
    fn claim_identity_excludes_origin_and_orders_focused_evidence() {
        let contract = Digest::blake3(b"contract");
        let first = Claim::new(
            contract,
            ClaimPayload::FocusedTest {
                runner: "cargo".to_owned(),
                argv: vec!["cargo".to_owned(), "test".to_owned(), "answer".to_owned()],
                cwd_relative: ".".to_owned(),
                identifier: "answer".to_owned(),
                selection: "representative".to_owned(),
                evidence_paths: vec!["tests/b.rs".to_owned(), "tests/a.rs".to_owned()],
            },
        )
        .unwrap();
        let second = Claim::new(
            contract,
            ClaimPayload::FocusedTest {
                runner: "cargo".to_owned(),
                argv: vec!["cargo".to_owned(), "test".to_owned(), "answer".to_owned()],
                cwd_relative: ".".to_owned(),
                identifier: "answer".to_owned(),
                selection: "representative".to_owned(),
                evidence_paths: vec!["tests/a.rs".to_owned(), "tests/b.rs".to_owned()],
            },
        )
        .unwrap();
        assert_eq!(first, second);
        assert!(first.is_canonical());
    }

    #[test]
    fn claim_payload_and_contract_change_identity() {
        let first = Claim::new(
            Digest::blake3(b"contract-a"),
            ClaimPayload::ImplementationLocation { location: location() },
        )
        .unwrap();
        let mut changed_location = location();
        changed_location.symbol = Some("other".to_owned());
        let changed_payload = Claim::new(
            Digest::blake3(b"contract-a"),
            ClaimPayload::ImplementationLocation { location: changed_location },
        )
        .unwrap();
        let changed_contract = Claim::new(
            Digest::blake3(b"contract-b"),
            ClaimPayload::ImplementationLocation { location: location() },
        )
        .unwrap();
        assert_ne!(first.id, changed_payload.id);
        assert_ne!(first.id, changed_contract.id);
    }

    #[test]
    fn claim_set_identity_orders_members_and_excludes_creation_time() {
        let first_claim = ClaimId(Digest::blake3(b"claim-a"));
        let second_claim = ClaimId(Digest::blake3(b"claim-b"));
        let first_certificate = ClaimValidationCertificateId(Digest::blake3(b"certificate-a"));
        let second_certificate = ClaimValidationCertificateId(Digest::blake3(b"certificate-b"));
        let obligation = ObligationId(Digest::blake3(b"obligation"));
        let members = vec![(second_claim, second_certificate), (first_claim, first_certificate)];
        let first = ClaimSetCertificate::new(
            NeedId(Digest::blake3(b"need")),
            members.clone(),
            vec![obligation],
            Digest::blake3(b"world"),
            Digest::blake3(b"engine"),
            1,
        )
        .unwrap();
        let second = ClaimSetCertificate::new(
            NeedId(Digest::blake3(b"need")),
            members.into_iter().rev().collect(),
            vec![obligation],
            Digest::blake3(b"world"),
            Digest::blake3(b"engine"),
            2,
        )
        .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.claims, second.claims);
        assert!(first.is_canonical());
        assert!(second.is_canonical());
    }

    #[test]
    fn runtime_flow_anchor_binds_the_ordered_step_graph() {
        let first = SemanticFlowStep {
            role: FlowStepRole::Producer,
            location: location(),
            description: "produce".to_owned(),
        };
        let mut changed = first.clone();
        changed.role = FlowStepRole::Consumer;
        assert_ne!(
            runtime_flow_anchor("default", std::slice::from_ref(&first)),
            runtime_flow_anchor("default", std::slice::from_ref(&changed))
        );
        assert_ne!(
            runtime_flow_anchor("default", std::slice::from_ref(&first)),
            runtime_flow_anchor("recovery", std::slice::from_ref(&first))
        );
    }
}
