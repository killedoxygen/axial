use super::*;

pub const INTENT_MIGRATION: Migration = Migration {
    id: "launch_intents.v1",
    sql: "CREATE TABLE launch_intents (
        intent_key TEXT PRIMARY KEY NOT NULL CHECK(length(intent_key)=36),
        payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 16384),
        state TEXT NOT NULL CHECK(state IN ('pending','accepted','rejected','interrupted')),
        error TEXT,
        terminal_ack INTEGER NOT NULL DEFAULT 0 CHECK(terminal_ack IN (0,1)),
        settlement BLOB CHECK(settlement IS NULL OR length(settlement) BETWEEN 1 AND 4096)
    ) STRICT;
    CREATE INDEX launch_intents_unresolved ON launch_intents(intent_key)
        WHERE state IN ('accepted','interrupted') AND terminal_ack=0;
    CREATE INDEX launch_intents_session ON launch_intents(
        json_extract(CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CAST(payload AS TEXT) ELSE '{}' END, '$.session_id'))
        WHERE state IN ('accepted','interrupted');",
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub instance_id: InstanceId,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub max_memory_mb: Option<i32>,
    #[serde(default)]
    pub min_memory_mb: Option<i32>,
    #[serde(default)]
    pub client_started_at_ms: Option<i64>,
    #[serde(default)]
    pub intent_key: Option<String>,
}

impl LaunchRequest {
    pub fn validate(&self) -> Result<(), LaunchError> {
        if let Some(key) = &self.intent_key {
            validate_intent_key(key)?;
        }
        if let Some(name) = &self.username {
            if !name.trim().is_empty() {
                crate::settings::validate_username(name.trim())
                    .map_err(|_| LaunchError::InvalidUsername)?;
            }
        }
        for memory in [self.max_memory_mb, self.min_memory_mb]
            .into_iter()
            .flatten()
        {
            if !(0..=1024 * 1024).contains(&memory) {
                return Err(LaunchError::InvalidMemory);
            }
        }
        if self.client_started_at_ms.is_some_and(|value| value < 0) {
            return Err(LaunchError::InvalidRequest);
        }
        Ok(())
    }

    pub(super) fn same_effect(&self, other: &Self) -> bool {
        self.instance_id == other.instance_id
            && self.version_id == other.version_id
            && self.username == other.username
            && self.max_memory_mb == other.max_memory_mb
            && self.min_memory_mb == other.min_memory_mb
    }
}

pub(super) fn validate_intent_key(key: &str) -> Result<(), LaunchError> {
    let id = uuid::Uuid::parse_str(key).map_err(|_| LaunchError::InvalidIntent)?;
    if id.is_nil() || id.to_string() != key {
        return Err(LaunchError::InvalidIntent);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchIntentStatus {
    Preparing,
    Accepted { session: SessionSnapshot },
    Rejected { error: LaunchErrorResponse },
    Interrupted { session_id: String },
}

/// The public form of an intent; an accepted session is its latest observation.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchIntentView {
    Preparing,
    Accepted {
        session: SessionSnapshot,
    },
    Rejected {
        error: String,
        code: LaunchError,
    },
    Interrupted {
        session_id: String,
        error: String,
        code: LaunchError,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum IntentRejection {
    Runtime { runtime_failure: JavaDiscoveryError },
    Code(LaunchError),
}

impl From<&LaunchError> for IntentRejection {
    fn from(error: &LaunchError) -> Self {
        match error {
            LaunchError::RuntimeFailure(cause) => Self::Runtime {
                runtime_failure: cause.clone(),
            },
            code => Self::Code(code.clone()),
        }
    }
}

impl From<IntentRejection> for LaunchError {
    fn from(error: IntentRejection) -> Self {
        match error {
            IntentRejection::Runtime { runtime_failure } => Self::RuntimeFailure(runtime_failure),
            IntentRejection::Code(code) => code,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IntentRecord {
    pub(super) request: LaunchRequest,
    pub(super) context: Option<String>,
    pub(super) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) binding: Option<IntentBinding>,
}

/// Exact accepted-row authority, handed only to the owning session before spawn.
#[derive(Clone)]
pub(in crate::launch) struct AcceptedIntent {
    pub(super) storage: Option<Arc<MetadataStore>>,
    pub(super) key: String,
    pub(super) payload: Vec<u8>,
    pub(super) record: IntentRecord,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::launch) enum ObservationError {
    Invalid,
    Conflict,
    Storage,
}

impl From<StorageError> for ObservationError {
    fn from(_: StorageError) -> Self {
        Self::Storage
    }
}

impl From<crate::storage::rusqlite::Error> for ObservationError {
    fn from(_: crate::storage::rusqlite::Error) -> Self {
        Self::Storage
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettlementRecord {
    pub(super) version: u8,
    pub(super) accepted_payload: [u8; 32],
    pub(super) observation: crate::launch::session::ObservedSettlement,
}

impl AcceptedIntent {
    pub(in crate::launch) fn matches_session(
        &self,
        session_id: &str,
        instance_id: &InstanceId,
    ) -> bool {
        self.record.session_id == session_id && self.record.request.instance_id == *instance_id
    }

    pub(in crate::launch) fn observe(
        &self,
        observation: &crate::launch::session::ObservedSettlement,
    ) -> Result<bool, ObservationError> {
        let settlement = serde_json::to_vec(&SettlementRecord {
            version: 1,
            accepted_payload: Sha256::digest(&self.payload).into(),
            observation: observation.clone(),
        })
        .map_err(|_| ObservationError::Invalid)?;
        decode_settlement(&self.record, &self.payload, &settlement)
            .map_err(|_| ObservationError::Invalid)?;
        let Some(storage) = &self.storage else {
            return Ok(false);
        };
        storage.transaction(|tx| -> Result<(), ObservationError> {
            let read = || -> Result<_, ObservationError> {
                Ok(tx.query_row(
                    "SELECT payload,state,settlement FROM launch_intents WHERE intent_key=?1",
                    [&self.key],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<Vec<u8>>>(2)?)),
                ).optional()?)
            };
            let Some((payload, state, prior)) = read()? else { return Err(ObservationError::Conflict); };
            if payload != self.payload || state != "accepted" {
                return Err(ObservationError::Conflict);
            }
            if let Some(prior) = prior {
                return if prior == settlement { Ok(()) } else { Err(ObservationError::Conflict) };
            }
            if tx.execute(
                "UPDATE launch_intents SET settlement=?3 WHERE intent_key=?1 AND payload=?2 AND state='accepted' AND settlement IS NULL",
                params![self.key, self.payload, settlement],
            )? != 1 || read()? != Some((self.payload.clone(), "accepted".into(), Some(settlement.clone()))) {
                return Err(ObservationError::Conflict);
            }
            Ok(())
        }).map(|()| true)
    }
}

pub(super) fn decode_settlement(
    record: &IntentRecord,
    accepted_payload: &[u8],
    bytes: &[u8],
) -> Result<crate::launch::session::ObservedSettlement, LaunchError> {
    if bytes.len() > 4096
        || record
            .binding
            .as_ref()
            .is_none_or(|binding| binding.version != 1)
    {
        return Err(LaunchError::IntentUnavailable);
    }
    let settlement: SettlementRecord =
        serde_json::from_slice(bytes).map_err(|_| LaunchError::IntentUnavailable)?;
    if settlement.version != 1
        || settlement.accepted_payload != <[u8; 32]>::from(Sha256::digest(accepted_payload))
        || !settlement.observation.validate()
        || !settlement
            .observation
            .matches_requested_version(record.request.version_id.as_deref())
        || serde_json::to_vec(&settlement).map_err(|_| LaunchError::IntentUnavailable)? != bytes
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(settlement.observation)
}

/// Equality evidence from admitted capabilities, never authority to open a path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::launch) struct IntentBinding {
    pub(super) version: u8,
    pub(super) library_id: String,
    pub(super) library_root: [u8; 32],
    pub(super) application_root: [u8; 32],
    pub(super) directory_name: String,
    pub(super) directory_receipt: String,
}

impl IntentBinding {
    pub(in crate::launch) fn capture(
        instance: &RegisteredInstance,
        application: &ApplicationRootPin,
    ) -> Result<Self, LaunchError> {
        instance.validate_current().map_err(instance_error)?;
        let binding = Self {
            version: 1,
            library_id: instance.generation().library_id().to_string(),
            library_root: instance
                .generation()
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness(),
            application_root: application
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness(),
            directory_name: instance.record().directory_name.clone(),
            directory_receipt: instance
                .directory()
                .receipt()
                .map_err(|_| LaunchError::InstanceChanged)?,
        };
        instance.validate_current().map_err(instance_error)?;
        application
            .revalidate()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        Ok(binding)
    }
}

pub(super) struct IntentEntry {
    pub(super) record: IntentRecord,
    pub(super) claimed: bool,
    pub(super) accepted: bool,
    pub(super) status: watch::Sender<LaunchIntentStatus>,
}

/// The bounded live cache retains waiters. Durable identities survive process
/// restart and can never authorize a second process.
#[derive(Clone)]
pub struct LaunchIntents {
    pub(super) entries: Arc<Mutex<BTreeMap<String, IntentEntry>>>,
    pub(super) capacity: usize,
    pub(super) storage: Option<Arc<MetadataStore>>,
    pub(super) reports: Option<crate::launch::reports::LaunchReportStore>,
    pub(super) restored: Arc<Vec<(RegisteredInstance, ApplicationRootPin)>>,
}

pub(super) const MAX_RECOVERY_INTENTS: usize = 4096;
pub(super) const MAX_RECOVERY_BYTES: usize = 32 * 1024 * 1024;

pub(super) struct PreparationSentinel {
    pub(super) intents: LaunchIntents,
    pub(super) key: String,
    pub(super) settled: bool,
    pub(super) telemetry: Arc<crate::launch::session::LaunchAttemptTelemetry>,
}
impl Drop for PreparationSentinel {
    fn drop(&mut self) {
        if !self.settled {
            self.telemetry.failure(None);
            self.intents.abandon(&self.key);
        }
    }
}

pub(super) enum ReservedIntent {
    Existing(watch::Receiver<LaunchIntentStatus>),
    New {
        key: String,
        session_id: String,
        receiver: watch::Receiver<LaunchIntentStatus>,
    },
}

impl LaunchIntents {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            capacity,
            storage: None,
            reports: None,
            restored: Arc::new(Vec::new()),
        }
    }

    pub(super) fn with_storage(
        storage: Arc<MetadataStore>,
        reports: crate::launch::reports::LaunchReportStore,
        capacity: usize,
    ) -> Result<Self, LaunchError> {
        Ok(Self {
            storage: Some(storage),
            reports: Some(reports),
            ..Self::new(capacity)
        })
    }

    #[cfg(test)]
    pub(super) fn needs_recovery(
        storage: Arc<MetadataStore>,
        reports: crate::launch::reports::LaunchReportStore,
    ) -> Result<bool, LaunchError> {
        Ok(!Self::with_storage(storage, reports, MAX_RECOVERY_INTENTS)?
            .interrupted_records()?
            .is_empty())
    }

    /// Restore before constructing effect-producing services. No process is
    /// adopted, signalled or declared settled by restoring these reservations.
    pub fn restore(
        storage: Arc<MetadataStore>,
        reports: crate::launch::reports::LaunchReportStore,
        directories: &InstanceDirectories,
    ) -> Result<Self, LaunchError> {
        let mut intents = Self::with_storage(storage, reports, MAX_RECOVERY_INTENTS)?;
        let records = match intents.interrupted_records() {
            Ok(records) => records,
            Err(error) => {
                directories.library().fence_interrupted_launch();
                return Err(error);
            }
        };
        if records.is_empty() {
            return Ok(intents);
        }
        // The lifecycle fence deliberately outlives this owner or a failed
        // constructor. Only a future exact process-settlement owner may clear it.
        directories.library().fence_interrupted_launch();
        let application = directories
            .library()
            .admit_application_root()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let mut restored = Vec::with_capacity(records.len());
        for record in records {
            let binding = record
                .binding
                .as_ref()
                .ok_or(LaunchError::IntentUnavailable)?;
            let pin = directories
                .library()
                .admit()
                .map_err(|_| LaunchError::LibraryUnavailable)?;
            let library_root = pin
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness();
            let application_root = application
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness();
            if binding.version != 1
                || binding.library_id != pin.library_id().to_string()
                || binding.library_root != library_root
                || binding.application_root != application_root
            {
                return Err(LaunchError::IntentUnavailable);
            }
            let captured = directories
                .registry()
                .get_live(&record.request.instance_id)
                .map_err(|_| LaunchError::IntentUnavailable)?;
            if captured.library_id != binding.library_id
                || captured.directory_name != binding.directory_name
                || captured.directory_receipt.as_deref() != Some(binding.directory_receipt.as_str())
            {
                return Err(LaunchError::IntentUnavailable);
            }
            let lease = directories
                .exclusions()
                .try_acquire_read_artifacts(
                    [record.request.instance_id.as_str()],
                    [crate::install::queue::library_artifact(&binding.library_id)],
                )
                .map_err(|_| LaunchError::InstanceBusy)?;
            let instance = InstanceDirectories::admit_record(
                directories.registry().clone(),
                captured,
                pin,
                lease,
            )
            .map_err(|_| LaunchError::IntentUnavailable)?;
            if IntentBinding::capture(&instance, &application)? != *binding {
                return Err(LaunchError::IntentUnavailable);
            }
            restored.push((instance, application.clone()));
        }
        intents.restored = Arc::new(restored);
        Ok(intents)
    }

    #[cfg(test)]
    pub(super) fn has_interrupted_launches(&self) -> bool {
        !self.restored.is_empty()
    }

    pub(super) fn interrupted_records(&self) -> Result<Vec<IntentRecord>, LaunchError> {
        self.interrupted_records_with_limits(MAX_RECOVERY_INTENTS, MAX_RECOVERY_BYTES)
    }

    pub(super) fn interrupted_records_with_limits(
        &self,
        record_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<IntentRecord>, LaunchError> {
        let storage = self
            .storage
            .as_ref()
            .ok_or(LaunchError::IntentUnavailable)?;
        let reports = self
            .reports
            .as_ref()
            .ok_or(LaunchError::IntentUnavailable)?;
        let mut interrupted = Vec::new();
        let mut after = String::new();
        let mut bytes = 0usize;
        let mut examined = 0usize;
        let mut acknowledged = false;
        loop {
            // Acknowledged history is excluded. Reportless observations still
            // consume the bounded scan and require exact validation.
            let row = storage
                .read(|db| -> Result<_, StorageError> {
                    Ok(db
                        .query_row(
                            "SELECT CASE WHEN length(intent_key)=36 THEN intent_key END,
                     CASE WHEN length(payload)<=16384 THEN payload END,settlement
                     FROM launch_intents WHERE state IN ('accepted','interrupted')
                     AND terminal_ack=0 AND intent_key>?1 ORDER BY intent_key LIMIT 1",
                            [&after],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Vec<u8>>(1)?,
                                    row.get::<_, Option<Vec<u8>>>(2)?,
                                ))
                            },
                        )
                        .optional()?)
                })
                .map_err(|_| LaunchError::IntentUnavailable)?;
            let Some((key, payload, settlement)) = row else {
                return Ok(interrupted);
            };
            let row_bytes = payload
                .len()
                .saturating_add(settlement.as_ref().map_or(0, Vec::len));
            if examined == record_limit || bytes.saturating_add(row_bytes) > byte_limit {
                return Err(if acknowledged {
                    LaunchError::RecoveryIncomplete
                } else {
                    LaunchError::AtCapacity
                });
            }
            examined += 1;
            bytes += row_bytes;
            let record = decode_intent(&key, &payload)?;
            if let Some(bytes) = settlement {
                // This proof is independent of optional report persistence.
                decode_settlement(&record, &payload, &bytes)?;
                after = key;
                continue;
            }
            if let Some(context) = &record.context {
                serde_json::from_str::<crate::launch::reports::LaunchProofScenario>(context)
                    .map_err(|_| LaunchError::IntentUnavailable)?;
            }
            // Account for terminal proof bytes before asking its existing owner
            // to decode them; the whole restart scan has one work budget.
            let report_bytes = storage
                .read(|db| -> Result<Option<usize>, StorageError> {
                    Ok(db
                        .query_row(
                            "SELECT length(payload) FROM launch_reports WHERE session_id=?1",
                            [&record.session_id],
                            |row| row.get(0),
                        )
                        .optional()?)
                })
                .map_err(|_| LaunchError::IntentUnavailable)?
                .unwrap_or(0);
            if bytes.saturating_add(report_bytes) > byte_limit {
                return Err(if acknowledged {
                    LaunchError::RecoveryIncomplete
                } else {
                    LaunchError::AtCapacity
                });
            }
            bytes += report_bytes;
            if let Some(report) = reports
                .get(&record.session_id)
                .map_err(|_| LaunchError::IntentUnavailable)?
            {
                terminal_snapshot(&record, &report)?;
                // Revalidate the immutable proof and acknowledge atomically.
                reports
                    .acknowledge_intent(&record.session_id)
                    .map_err(|_| LaunchError::IntentUnavailable)?;
                acknowledged = true;
            } else {
                interrupted.push(record);
            }
            after = key;
        }
    }

    #[cfg(test)]
    pub(super) fn reserve(&self, request: &LaunchRequest) -> Result<ReservedIntent, LaunchError> {
        self.reserve_context(request, None)
    }

    pub(super) fn reserve_context(
        &self,
        request: &LaunchRequest,
        context: Option<String>,
    ) -> Result<ReservedIntent, LaunchError> {
        let mut request = request.clone();
        request
            .intent_key
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        self.reserve_identity(&request, context)?;
        let key = request.intent_key.expect("reserved launch intent");
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        let entry = entries
            .get_mut(&key)
            .ok_or(LaunchError::IntentUnavailable)?;
        if entry.claimed || !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing) {
            return Ok(ReservedIntent::Existing(entry.status.subscribe()));
        }
        entry.claimed = true;
        Ok(ReservedIntent::New {
            key,
            session_id: entry.record.session_id.clone(),
            receiver: entry.status.subscribe(),
        })
    }

    pub(super) fn reserve_identity(
        &self,
        request: &LaunchRequest,
        context: Option<String>,
    ) -> Result<String, LaunchError> {
        request.validate()?;
        let key = request
            .intent_key
            .as_ref()
            .ok_or(LaunchError::InvalidIntent)?;
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if !entries.contains_key(key) {
            if entries.len() >= self.capacity {
                return Err(LaunchError::AtCapacity);
            }
            if let Some(entry) = self.load(key)? {
                entries.insert(key.clone(), entry);
            }
        }
        if let Some(entry) = entries.get(key) {
            if !request.same_effect(&entry.record.request) || context != entry.record.context {
                return Err(LaunchError::IntentConflict);
            }
            return Ok(entry.record.session_id.clone());
        }
        let record = IntentRecord {
            request: request.clone(),
            context,
            session_id: uuid::Uuid::new_v4().to_string(),
            binding: None,
        };
        if let Some(storage) = &self.storage {
            let payload =
                serde_json::to_vec(&record).map_err(|_| LaunchError::IntentUnavailable)?;
            storage.transaction(|tx| -> Result<(), StorageError> {
                tx.execute("INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'pending')", params![key, payload])?;
                Ok(())
            }).map_err(|_| LaunchError::IntentUnavailable)?;
        }
        let session_id = record.session_id.clone();
        entries.insert(
            key.clone(),
            IntentEntry {
                record,
                claimed: false,
                accepted: false,
                status: watch::channel(LaunchIntentStatus::Preparing).0,
            },
        );
        Ok(session_id)
    }

    pub(super) fn load(&self, key: &str) -> Result<Option<IntentEntry>, LaunchError> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        let row = storage
            .read(|connection| -> Result<_, StorageError> {
                Ok(connection
                    .query_row(
                        "SELECT payload,state,error,settlement FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<Vec<u8>>>(3)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .map_err(|_| LaunchError::IntentUnavailable)?;
        row.map(|(payload, state, error, settlement)| {
            let record = decode_intent(key, &payload)?;
            if settlement.is_some() && !matches!(state.as_str(), "accepted" | "interrupted") {
                return Err(LaunchError::IntentUnavailable);
            }
            let status = match state.as_str() {
                "pending" if record.context.is_some() => LaunchIntentStatus::Preparing,
                "pending" => {
                    // No process was authorized. Ordinary launch recovery only
                    // reads status; do not leave it waiting for a lost worker.
                    self.reject(key, &LaunchError::PreparationFailed)?;
                    LaunchIntentStatus::Rejected {
                        error: LaunchError::PreparationFailed.into(),
                    }
                }
                "accepted" | "interrupted" => {
                    match self.terminal_session(&record, &payload, settlement.as_deref())? {
                        Some(session) => LaunchIntentStatus::Accepted { session },
                        None => LaunchIntentStatus::Interrupted {
                            session_id: record.session_id.clone(),
                        },
                    }
                }
                "rejected" => LaunchIntentStatus::Rejected {
                    error: LaunchError::from(
                        serde_json::from_str::<IntentRejection>(
                            &error.ok_or(LaunchError::IntentUnavailable)?,
                        )
                        .map_err(|_| LaunchError::IntentUnavailable)?,
                    )
                    .into(),
                },
                _ => return Err(LaunchError::IntentUnavailable),
            };
            Ok(IntentEntry {
                record,
                claimed: false,
                accepted: false,
                status: watch::channel(status).0,
            })
        })
        .transpose()
    }

    pub(super) fn terminal_session(
        &self,
        record: &IntentRecord,
        payload: &[u8],
        settlement: Option<&[u8]>,
    ) -> Result<Option<SessionSnapshot>, LaunchError> {
        let observation = settlement
            .map(|bytes| decode_settlement(record, payload, bytes))
            .transpose()?;
        if let Some(report) = self
            .reports
            .as_ref()
            .map(|reports| reports.get(&record.session_id))
            .transpose()
            .map_err(|_| LaunchError::IntentUnavailable)?
            .flatten()
        {
            if observation
                .as_ref()
                .is_some_and(|value| !value.matches_report(&report))
            {
                return Err(LaunchError::IntentUnavailable);
            }
            return terminal_snapshot(record, &report).map(Some);
        }
        Ok(observation.map(|value| {
            value.snapshot(
                record.session_id.clone(),
                record.request.instance_id.clone(),
            )
        }))
    }

    pub(super) fn accept(
        &self,
        key: &str,
        binding: IntentBinding,
    ) -> Result<AcceptedIntent, LaunchError> {
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        let entry = entries.get_mut(key).ok_or(LaunchError::IntentUnavailable)?;
        if !entry.claimed
            || entry.accepted
            || !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing)
            || binding.version != 1
            || binding.directory_name != entry.record.request.instance_id.as_str()
        {
            return Err(LaunchError::IntentConflict);
        }
        let mut record = entry.record.clone();
        record.binding = Some(binding);
        let payload = serde_json::to_vec(&record).map_err(|_| LaunchError::IntentUnavailable)?;
        if let Some(storage) = &self.storage {
            storage.transaction(|tx| -> Result<(), StorageError> {
                let prior: Vec<u8> = tx.query_row(
                    "SELECT CASE WHEN length(payload)<=16384 THEN payload END FROM launch_intents WHERE intent_key=?1 AND state='pending' AND settlement IS NULL",
                    [key], |row| row.get(0),
                )?;
                if decode_intent(key, &prior).map_err(|_| StorageError::Corrupt)? != entry.record {
                    return Err(StorageError::Corrupt);
                }
                if tx.execute("UPDATE launch_intents SET payload=?2,state='accepted' WHERE intent_key=?1 AND state='pending' AND payload=?3 AND settlement IS NULL", params![key, payload, prior])? != 1 {
                    return Err(StorageError::Corrupt);
                }
                let saved: (Vec<u8>, String, Option<Vec<u8>>) = tx.query_row(
                    "SELECT payload,state,settlement FROM launch_intents WHERE intent_key=?1", [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if saved != (payload.clone(), "accepted".into(), None) { return Err(StorageError::Corrupt); }
                Ok(())
            }).map_err(|_| LaunchError::IntentUnavailable)?;
        }
        entry.record = record.clone();
        entry.accepted = true;
        Ok(AcceptedIntent {
            storage: self.storage.clone(),
            key: key.into(),
            payload,
            record,
        })
    }

    pub(super) fn abandon(&self, key: &str) {
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        let Some(entry) = entries.get(key) else {
            return;
        };
        if entry.accepted {
            entry.status.send_replace(LaunchIntentStatus::Interrupted {
                session_id: entry.record.session_id.clone(),
            });
            // Keep durable acceptance: startup converts it to interruption.
        } else {
            drop(entries);
            self.settle(key, Err(LaunchError::PreparationFailed));
        }
    }

    pub(super) fn settle(&self, key: &str, result: Result<SessionSnapshot, LaunchError>) {
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        let Some(entry) = entries.get(key) else {
            return;
        };
        if !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing) {
            return;
        }
        let result = match result {
            Err(error) => self.reject(key, &error).and(Err(error)),
            success => success,
        };
        entry.status.send_replace(match result {
            Ok(session) => LaunchIntentStatus::Accepted { session },
            Err(error) => LaunchIntentStatus::Rejected {
                error: error.into(),
            },
        });
    }

    pub(super) fn reject(&self, key: &str, error: &LaunchError) -> Result<(), LaunchError> {
        if let Some(storage) = &self.storage {
            let error = serde_json::to_string(&IntentRejection::from(error))
                .map_err(|_| LaunchError::IntentUnavailable)?;
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    if tx.execute(
                        "UPDATE launch_intents SET state='rejected',error=?2 WHERE intent_key=?1",
                        params![key, error],
                    )? != 1
                    {
                        return Err(StorageError::Corrupt);
                    }
                    let saved: (String, String) = tx.query_row(
                        "SELECT state,error FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    if saved != ("rejected".into(), error) {
                        return Err(StorageError::Corrupt);
                    }
                    Ok(())
                })
                .map_err(|_| LaunchError::IntentUnavailable)?;
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, key: &str) -> Result<Option<LaunchIntentStatus>, LaunchError> {
        validate_intent_key(key)?;
        let entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if let Some(entry) = entries.get(key) {
            if entry.accepted {
                if let Some(storage) = &self.storage {
                    let settled = storage.read(|db| -> Result<bool, StorageError> {
                        Ok(db.query_row("SELECT settlement IS NOT NULL FROM launch_intents WHERE intent_key=?1", [key], |row| row.get(0))?)
                    }).map_err(|_| LaunchError::IntentUnavailable)?;
                    if settled {
                        return Ok(self.load(key)?.map(|loaded| loaded.status.borrow().clone()));
                    }
                }
            }
            return Ok(Some(entry.status.borrow().clone()));
        }
        Ok(self.load(key)?.map(|entry| entry.status.borrow().clone()))
    }

    pub(super) fn record(&self, key: &str) -> Result<Option<IntentRecord>, LaunchError> {
        validate_intent_key(key)?;
        let entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if let Some(entry) = entries.get(key) {
            return Ok(Some(entry.record.clone()));
        }
        Ok(self.load(key)?.map(|entry| entry.record))
    }
}

pub(super) fn terminal_snapshot(
    record: &IntentRecord,
    report: &crate::launch::reports::LaunchProofRecord,
) -> Result<SessionSnapshot, LaunchError> {
    let scenario = record
        .context
        .as_deref()
        .map(serde_json::from_str::<crate::launch::reports::LaunchProofScenario>)
        .transpose()
        .map_err(|_| LaunchError::IntentUnavailable)?
        .unwrap_or_default();
    // scenario_id is derived after preparation. The captured benchmark
    // dimensions, requested version and memory are the durable request identity.
    if report.session_id != record.session_id
        || report.instance_id != record.request.instance_id.as_str()
        || report.scenario.benchmark_id != scenario.benchmark_id
        || report.scenario.benchmark_profile != scenario.benchmark_profile
        || report.scenario.benchmark_run_type != scenario.benchmark_run_type
        || report.scenario.benchmark_mode != scenario.benchmark_mode
        || report.scenario.version_id.as_deref() != Some(report.version_id.as_str())
        || record
            .request
            .version_id
            .as_ref()
            .is_some_and(|version| version != &report.version_id)
        || record
            .request
            .max_memory_mb
            .filter(|value| *value > 0)
            .is_some_and(|memory| report.scenario.requested_memory_mb != Some(memory))
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(SessionSnapshot::from_report(
        report,
        record.request.instance_id.clone(),
    ))
}

/// Called only inside the report owner's transaction with canonical, persisted
/// terminal proof. This does not create an intent or authorize a process.
pub(in crate::launch) fn acknowledge_terminal(
    tx: &crate::storage::rusqlite::Transaction<'_>,
    report: &crate::launch::reports::LaunchProofRecord,
) -> Result<(), crate::launch::reports::ReportError> {
    use crate::launch::reports::ReportError;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='launch_intents')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let mut query = tx.prepare(
        "SELECT CASE WHEN length(intent_key)=36 THEN intent_key END,
         CASE WHEN length(payload)<=16384 THEN payload END,terminal_ack,settlement FROM launch_intents
         WHERE state IN ('accepted','interrupted') AND
         json_extract(CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CAST(payload AS TEXT) ELSE '{}' END, '$.session_id')=?1 LIMIT 2",
    )?;
    let rows = query
        .query_map([&report.session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > 1 {
        return Err(ReportError::Invalid);
    }
    if let Some((key, payload, acknowledged, settlement)) = rows.into_iter().next() {
        let record = decode_intent(&key, &payload).map_err(|_| ReportError::Invalid)?;
        terminal_snapshot(&record, report).map_err(|_| ReportError::Invalid)?;
        if let Some(bytes) = &settlement {
            let observation =
                decode_settlement(&record, &payload, bytes).map_err(|_| ReportError::Invalid)?;
            if !observation.matches_report(report) {
                return Err(ReportError::Invalid);
            }
        }
        if !acknowledged
            && tx.execute(
                "UPDATE launch_intents SET terminal_ack=1 WHERE intent_key=?1 AND payload=?2
             AND state IN ('accepted','interrupted') AND terminal_ack=0",
                params![key, payload],
            )? != 1
        {
            return Err(ReportError::Invalid);
        }
        let saved: (Vec<u8>, bool, Option<Vec<u8>>) = tx.query_row(
            "SELECT payload,terminal_ack,settlement FROM launch_intents WHERE intent_key=?1 AND state IN ('accepted','interrupted')",
            [&key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if saved != (payload, true, settlement) {
            return Err(ReportError::Invalid);
        }
    }
    Ok(())
}

pub(super) fn decode_intent(key: &str, payload: &[u8]) -> Result<IntentRecord, LaunchError> {
    if payload.len() > 16_384 {
        return Err(LaunchError::IntentUnavailable);
    }
    let record: IntentRecord =
        serde_json::from_slice(payload).map_err(|_| LaunchError::IntentUnavailable)?;
    record
        .request
        .validate()
        .map_err(|_| LaunchError::IntentUnavailable)?;
    if record.request.intent_key.as_deref() != Some(key)
        || validate_intent_key(&record.session_id).is_err()
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(record)
}

#[cfg(test)]
pub(in crate::launch) fn accepted_for_test(
    storage: Arc<MetadataStore>,
) -> (AcceptedIntent, String, InstanceId) {
    let intents = super::tests::durable_intents(storage);
    let request = super::tests::request();
    let ReservedIntent::New {
        key, session_id, ..
    } = intents.reserve(&request).unwrap()
    else {
        panic!()
    };
    let acceptance = intents
        .accept(&key, super::tests::binding(&request))
        .unwrap();
    (acceptance, session_id, request.instance_id)
}
