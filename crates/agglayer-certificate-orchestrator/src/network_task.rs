use std::sync::Arc;

use agglayer_clock::ClockRef;
use agglayer_storage::{
    columns::{
        latest_proven_certificate_per_network::ProvenCertificate,
        latest_settled_certificate_per_network::SettledCertificate,
    },
    stores::{PendingCertificateReader, PendingCertificateWriter, StateReader, StateWriter},
};
use agglayer_types::{
    Certificate, CertificateStatus, CertificateStatusError, Hash, Height, LocalNetworkStateData,
    NetworkId,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    error::PreCertificationError, CertResponse, CertResponseSender, CertificationError, Certifier,
    CertifierOutput, Error, InitialCheckError,
};

/// Maximum height distance of future pending certificates.
const MAX_FUTURE_HEIGHT_DISTANCE: u64 = 5;

/// Network task that is responsible to certify the certificates for a network.
pub(crate) struct NetworkTask<CertifierClient, PendingStore, StateStore> {
    /// The network id for the network task.
    network_id: NetworkId,
    /// The pending store to read and write the pending certificates.
    pending_store: Arc<PendingStore>,
    /// The state store to read and write the state of the network.
    state_store: Arc<StateStore>,
    /// The certifier client to certify the certificates.
    certifier_client: Arc<CertifierClient>,
    /// The clock reference to subscribe to the epoch events and check for
    /// current epoch.
    clock_ref: ClockRef,
    /// The local network state of the network task.
    local_state: LocalNetworkStateData,
    /// Handle to the task of the running certifier.
    certifier_task: certifier_job::Manager,
    /// The sender to notify that a certificate has been proven.
    certification_notifier: mpsc::Sender<(
        oneshot::Sender<Result<SettledCertificate, String>>,
        ProvenCertificate,
    )>,
    /// The pending local network state that should be applied on receiving
    /// settlement response.
    pending_state: Option<LocalNetworkStateData>,
    /// The stream of new certificates to certify.
    certificate_stream: mpsc::Receiver<(Certificate, CertResponseSender)>,
    /// Flag to indicate if the network is at capacity for the current epoch.
    at_capacity_for_epoch: bool,
}

impl<CertifierClient, PendingStore, StateStore>
    NetworkTask<CertifierClient, PendingStore, StateStore>
where
    CertifierClient: Certifier,
    PendingStore: PendingCertificateReader + PendingCertificateWriter,
    StateStore: StateReader + StateWriter,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pending_store: Arc<PendingStore>,
        state_store: Arc<StateStore>,
        certifier_client: Arc<CertifierClient>,
        certification_notifier: mpsc::Sender<(
            oneshot::Sender<Result<SettledCertificate, String>>,
            ProvenCertificate,
        )>,
        clock_ref: ClockRef,
        network_id: NetworkId,
        certificate_stream: mpsc::Receiver<(Certificate, CertResponseSender)>,
    ) -> Result<Self, Error> {
        info!("Creating a new network task for network {}", network_id);

        let local_state = state_store
            .read_local_network_state(network_id)?
            .unwrap_or_default();

        debug!(
            "Local state for network {}: {}",
            network_id,
            local_state.get_roots().display_to_hex()
        );
        Ok(Self {
            network_id,
            pending_store,
            state_store,
            certifier_client,
            local_state,
            certifier_task: certifier_job::Manager::new(),
            certification_notifier,
            clock_ref,
            pending_state: None,
            certificate_stream,
            at_capacity_for_epoch: false,
        })
    }

    pub(crate) async fn run(
        mut self,
        cancellation_token: CancellationToken,
    ) -> Result<NetworkId, Error> {
        info!("Starting the network task for network {}", self.network_id);

        let mut stream_epoch = self.clock_ref.subscribe()?;

        let current_epoch = self.clock_ref.current_epoch();

        // Start from the latest settled certificate to define the next expected height
        let latest_settled = self
            .state_store
            .get_latest_settled_certificate_per_network(&self.network_id)?
            .map(|(_network_id, settled)| settled);

        let mut next_expected_height =
            if let Some(SettledCertificate(_, current_height, epoch, _)) = latest_settled {
                debug!("Current network height is {}", current_height);
                if epoch == current_epoch {
                    debug!("Already settled for the epoch {current_epoch}");
                    self.at_capacity_for_epoch = true;
                }

                current_height + 1
            } else {
                debug!("Network never settled any certificate");
                0
            };

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    debug!("Network task for network {} has been cancelled", self.network_id);
                    return Ok(self.network_id);
                }

                result = self.make_progress(&mut stream_epoch, &mut next_expected_height) => {
                    if let Err(error)= result {
                        error!("Error during the certification process: {}", error);

                        return Err(error)
                    }
                }
            }
        }
    }

    fn accepts_certificates(&self) -> bool {
        !self.at_capacity_for_epoch && !self.certifier_task.is_running()
    }

    async fn make_progress(
        &mut self,
        stream_epoch: &mut tokio::sync::broadcast::Receiver<agglayer_clock::Event>,
        next_expected_height: &mut u64,
    ) -> Result<(), Error> {
        debug!("Waiting for an event to make progress");

        tokio::select! {
            Ok(agglayer_clock::Event::EpochEnded(epoch)) = stream_epoch.recv() => {
                info!("Received an epoch event: {}", epoch);

                let current_epoch = self.clock_ref.current_epoch();
                if epoch != 0 && epoch < (current_epoch - 1) {
                    debug!("Received an epoch event for epoch {epoch} which is outdated, current epoch is {current_epoch}");

                    return Ok(());
                }

                self.at_capacity_for_epoch = false;
            }

            Some((certificate, response_sender)) = self.certificate_stream.recv() => {
                let certificate_id = certificate.hash();
                let height = certificate.height;
                info!(
                    hash = certificate_id.to_string(),
                    "Received a certificate event for {certificate_id} at height {height}"
                );

                let response = self.process_certificate(&certificate, *next_expected_height);

                if let Err(err) = &response {
                    let cert_id = certificate.hash();
                    warn!("Certificate processing error for {cert_id}: {err}");
                }

                if let Err(response) = response_sender.send(response) {
                    let cert_id = certificate.hash();
                    warn!("Failed to send response ({response:?}) to {cert_id}");
                }
            }

            result = self.certifier_task.join() => {
                let cert_id = result.certificate_id;
                info!("Finished certifying {cert_id}");

                let result = self.process_certifier_result(result, next_expected_height).await;
                if let Err(error) = result {
                    info!("Certifier result processing for {cert_id} error: {error}");
                }
            }
        };

        if !self.accepts_certificates() {
            return Ok(());
        }

        let height = *next_expected_height;

        debug!("Checking the next certificate to process at height {height}");

        // Get the certificate the pending certificate for the network at the height
        let certificate = if let Some(certificate) = self
            .pending_store
            .get_certificate(self.network_id, height)?
        {
            certificate
        } else {
            // There is no certificate to certify at this height for now
            return Ok(());
        };

        let certificate_id = certificate.hash();
        let header =
            if let Some(header) = self.state_store.get_certificate_header(&certificate_id)? {
                header
            } else {
                error!(
                    hash = certificate_id.to_string(),
                    "Certificate header not found for {certificate_id}"
                );

                return Ok(());
            };

        debug!(
            "Found certificate {certificate_id} with status {}",
            header.status
        );

        match header.status {
            CertificateStatus::Pending => {}

            // If the certificate is already proven or candidate, it means that the
            // certification process has already been initiated but not completed.
            // It also means that the proof exists and thus we should redo the native
            // execution to update the local state.
            CertificateStatus::Proven | CertificateStatus::Candidate => {
                // Redo native execution to get the new_state

                error!(
                    hash = certificate_id.to_string(),
                    "CRITICAL: Certificate {certificate_id} is already proven or candidate but we \
                     do not have the new_state anymore...",
                );

                return Ok(());
            }
            CertificateStatus::InError { error } => {
                warn!(
                    hash = certificate_id.to_string(),
                    "Certificate {certificate_id} is in error: {}", error
                );

                return Ok(());
            }
            CertificateStatus::Settled => {
                warn!(
                    hash = certificate_id.to_string(),
                    "Certificate {certificate_id} is already settled while trying to certify the \
                     certificate for network {} at height {}",
                    self.network_id,
                    height - 1
                );

                return Ok(());
            }
        }

        info!(
            hash = certificate_id.to_string(),
            "Certifying the certificate {certificate_id} for network {} at height {}",
            self.network_id,
            height
        );

        match self
            .certifier_client
            .certify(self.local_state.clone(), self.network_id, height)
        {
            Ok(task) => self.certifier_task.try_spawn(certificate_id, task)?,

            // If we received a `CertificateNotFound` error, it means that the certificate was
            // not found in the pending store. This can happen if we try to
            // certify a certificate that has not been received yet. When
            // received, the certificate will be stored in the pending store and
            // the certifier task will be spawned again.
            Err(PreCertificationError::CertificateNotFound(_network_id, _height)) => {}

            Err(PreCertificationError::ProofAlreadyExists(network_id, height, certificate_id)) => {
                warn!(
                    hash = certificate_id.to_string(),
                    "Received a proof certification error for a proof that already exists for \
                     network {} at height {}",
                    network_id,
                    height
                );
            }
            Err(PreCertificationError::Storage(error)) => {
                warn!(
                    hash = certificate_id.to_string(),
                    "Received a storage error while trying to certify the certificate for network \
                     {} at height {}: {:?}",
                    self.network_id,
                    height,
                    error
                );
            }
        };

        Ok(())
    }

    async fn process_certifier_result(
        &mut self,
        result: certifier_job::JobResult,
        next_expected_height: &mut u64,
    ) -> Result<(), Error> {
        let certifier_job::JobResult {
            certificate_id,
            result,
        } = result;
        debug!(
            "Processing certifier result: {:?}",
            result.as_ref().map(|_| certificate_id)
        );

        match result {
            Ok(CertifierOutput {
                height,
                certificate,
                new_state,
                ..
            }) => {
                debug!(
                    hash = certificate_id.to_string(),
                    "Proof certification completed for {certificate_id} for network {}",
                    self.network_id
                );
                if let Err(error) = self
                    .on_proven_certificate(height, certificate, new_state)
                    .await
                {
                    error!(
                        hash = certificate_id.to_string(),
                        "Error during the certification process of {certificate_id} for network \
                         {}: {:?}",
                        self.network_id,
                        error
                    );
                }

                *next_expected_height += 1;

                self.at_capacity_for_epoch = true;
                debug!(
                    hash = certificate_id.to_string(),
                    "Certification process completed for {certificate_id} for network {}",
                    self.network_id
                );

                Ok(())
            }

            Err(error) => {
                warn!(
                    hash = certificate_id.to_string(),
                    "Error during certification process of {certificate_id}: {}", error
                );
                let error: CertificateStatusError = match error {
                    CertificationError::TrustedSequencerNotFound(network) => {
                        CertificateStatusError::TrustedSequencerNotFound(network)
                    }
                    CertificationError::ProofVerificationFailed { source } => source.into(),
                    CertificationError::L1InfoRootNotFound(_certificate_id, l1_leaf_count) => {
                        CertificateStatusError::L1InfoRootNotFound(l1_leaf_count)
                    }

                    CertificationError::ProverExecutionFailed { source } => {
                        CertificateStatusError::ProofGenerationError {
                            generation_type: agglayer_types::GenerationType::Prover,
                            source,
                        }
                    }
                    CertificationError::NativeExecutionFailed { source } => {
                        CertificateStatusError::ProofGenerationError {
                            generation_type: agglayer_types::GenerationType::Native,
                            source,
                        }
                    }

                    CertificationError::Types { source } => source.into(),

                    CertificationError::Storage(error) => {
                        let error = format!(
                            "Storage error happened in the certification process of \
                             {certificate_id}: {:?}",
                            error
                        );
                        warn!(hash = certificate_id.to_string(), error);

                        CertificateStatusError::InternalError(error)
                    }
                    CertificationError::Serialize { source } => {
                        let error = format!(
                            "Serialization error happened in the certification process of \
                             {certificate_id}: {:?}",
                            source
                        );
                        warn!(hash = certificate_id.to_string(), error);

                        CertificateStatusError::InternalError(error)
                    }
                    CertificationError::Deserialize { source } => {
                        let error = format!(
                            "Deserialization error happened in the certification process of \
                             {certificate_id}: {:?}",
                            source
                        );
                        warn!(hash = certificate_id.to_string(), error);
                        CertificateStatusError::InternalError(error)
                    }
                    CertificationError::InternalError(error) => {
                        let error = format!(
                            "Internal error happened in the certification process of \
                             {certificate_id}: {}",
                            error
                        );
                        warn!(hash = certificate_id.to_string(), error);

                        CertificateStatusError::InternalError(error)
                    }
                };

                let status = CertificateStatus::InError { error };

                debug!("Updating status of {certificate_id} to {status:?}");

                if self
                    .state_store
                    .update_certificate_header_status(&certificate_id, &status)
                    .is_err()
                {
                    error!(
                        hash = certificate_id.to_string(),
                        "Certificate {certificate_id} in error and failed to update the \
                         certificate header status"
                    );
                }
                Ok(())
            }
        }
    }

    /// Process single certificate.
    ///
    /// Performs a number of initial checks for the certificate. If these pass,
    /// the certificate is recorded in persistent storage.
    fn process_certificate(&mut self, certificate: &Certificate, next_height: u64) -> CertResponse {
        let height = certificate.height;
        let network_id = certificate.network_id;

        if height < next_height {
            return Err(InitialCheckError::InPast {
                height,
                next_height,
            });
        }

        let max_height = next_height + MAX_FUTURE_HEIGHT_DISTANCE;
        if height > max_height {
            return Err(InitialCheckError::FarFuture { height, max_height });
        }

        // TODO signature check + rate limit

        let existing_header = self
            .state_store
            .get_certificate_header_by_cursor(network_id, height)?;

        if let Some(existing_header) = existing_header {
            use CertificateStatus as CS;

            let status = existing_header.status;
            match status {
                CS::InError { error: _ } => (),
                status @ (CS::Pending | CS::Proven | CS::Candidate | CS::Settled) => {
                    return Err(InitialCheckError::IllegalReplacement { status });
                }
            }
        }

        // TODO: Batch the two queries.
        // Insert the certificate header into the state store.
        self.state_store
            .insert_certificate_header(certificate, CertificateStatus::Pending)?;

        // Insert the certificate into the pending store.
        self.pending_store
            .insert_pending_certificate(network_id, height, certificate)?;

        Ok(())
    }
}

impl<CertifierClient, PendingStore, StateStore>
    NetworkTask<CertifierClient, PendingStore, StateStore>
where
    CertifierClient: Certifier,
    PendingStore: PendingCertificateReader + PendingCertificateWriter,
    StateStore: StateWriter,
{
    /// Context:
    ///
    /// At one point in time, there is at most one certifier task per network
    /// running. The certifier task try to generate a proof based on
    /// a certificate. The certifier task doesn't know about other tasks nor if
    /// the certificate will be included in an epoch. The Orchestrator is
    /// the one that is responsible to decide if a proof is valid and should
    /// be included in an epoch.
    ///
    /// Based on the current context of the orchestrator, we can
    /// determine the following:
    ///
    /// 1. If the state doesn't know the network and the height is 0, we update
    ///    the state. This is the first certificate for this network.
    /// 2. If the state knows the network and the height is the next one, we
    ///    update the state. This is the next certificate for this network.
    /// 3. If the state doesn't know the network and the height is not 0, we
    ///    ignore the proof.
    /// 4. If the state knows the network and the height is not the next
    ///    expected one, we ignore the proof.
    ///
    /// When a generated proof is accepted:
    /// - We update the cursor for the network.
    /// - We update the latest proven certificate for the network.
    /// - We do not remove the pending certificate. (as it needs to be included
    ///   in an epoch)
    /// - We spawn the next certificate for the network.
    async fn on_proven_certificate(
        &mut self,
        height: Height,
        certificate: Certificate,
        new_state: LocalNetworkStateData,
    ) -> Result<(), Error> {
        let certificate_id = certificate.hash();
        if let Err(error) = self
            .pending_store
            .set_latest_proven_certificate_per_network(&self.network_id, &height, &certificate_id)
        {
            error!(
                hash = certificate_id.to_string(),
                "Failed to set the latest proven certificate per network: {:?}", error
            );
        }

        if let Err(error) = self
            .state_store
            .update_certificate_header_status(&certificate_id, &CertificateStatus::Proven)
        {
            error!(
                hash = certificate_id.to_string(),
                "Failed to update the certificate header status: {:?}", error
            );
        }

        self.pending_state = Some(new_state);

        let (sender, receiver) = oneshot::channel();

        if self
            .certification_notifier
            .send((
                sender,
                ProvenCertificate(certificate_id, self.network_id, height),
            ))
            .await
            .is_err()
        {
            error!("Failed to send the proven certificate notification");
        }

        if let Ok(result) = receiver.await {
            match result {
                Ok(SettledCertificate(certificate_id, _height, _epoch, _index)) => {
                    info!(
                        hash = certificate_id.to_string(),
                        "Received a certificate settlement notification"
                    );
                    if let Some(new) = self.pending_state.take() {
                        debug!(
                            "Updated the state for network {} with the new state {} > {}",
                            self.network_id,
                            self.local_state.get_roots().display_to_hex(),
                            new.get_roots().display_to_hex()
                        );

                        self.local_state = new;

                        // Store the current state
                        let new_leaves = certificate
                            .bridge_exits
                            .iter()
                            .map(|exit| exit.hash().into())
                            .collect::<Vec<Hash>>();

                        self.state_store
                            .write_local_network_state(
                                &certificate.network_id,
                                &self.local_state,
                                new_leaves.as_slice(),
                            )
                            .map_err(|e| Error::PersistenceError {
                                certificate_id,
                                error: e.to_string(),
                            })?;
                    } else {
                        error!(
                            "Missing pending state for network {} needed upon settlement, current \
                             state: {}",
                            self.network_id,
                            self.local_state.get_roots().display_to_hex()
                        );
                    }
                }
                Err(error) => {
                    error!(
                        hash = certificate_id.to_string(),
                        "Failed to settle the certificate: {}", error
                    );

                    if self
                        .state_store
                        .update_certificate_header_status(
                            &certificate_id,
                            &CertificateStatus::InError {
                                error: CertificateStatusError::SettlementError(error),
                            },
                        )
                        .is_err()
                    {
                        error!(
                            hash = certificate_id.to_string(),
                            "Certificate {certificate_id} in error and failed to update the \
                             certificate header status"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

// TODO: Move into a separate file. Inline for now to avoid merge conflicts.
mod certifier_job {
    use std::{future::Future, pin::pin, task::Poll};

    use tokio::task::JoinHandle;

    use super::{CertificationError, CertifierOutput, Hash};

    pub type CertifierResult = std::result::Result<CertifierOutput, CertificationError>;

    pub enum Manager {
        Idle,
        Running(Job),
    }

    impl Manager {
        /// New certifier task manager.
        pub fn new() -> Self {
            Self::Idle
        }

        /// Check if a certification job is running.
        pub fn is_running(&self) -> bool {
            matches!(self, Self::Running(_))
        }

        /// Spawn a certifier task if not running.
        pub fn try_spawn(
            &mut self,
            certificate_id: Hash,
            task: impl Future<Output = CertifierResult> + Send + 'static,
        ) -> Result<(), CertificationError> {
            match self {
                Self::Idle => {
                    *self = Self::Running(Job {
                        task_handle: tokio::spawn(task),
                        certificate_id,
                    });
                    Ok(())
                }
                Self::Running(_) => Err(CertificationError::InternalError(
                    "Certifier task in progress".into(),
                )),
            }
        }

        /// Wait for a job to finish (pending if no job is running).
        pub fn join(&mut self) -> impl Future<Output = JobResult> + '_ {
            std::future::poll_fn(|cx| self.poll_join(cx))
        }

        fn poll_join(&mut self, cx: &mut std::task::Context) -> Poll<JobResult> {
            match self {
                Self::Idle => Poll::Pending,
                Self::Running(job) => match pin!(&mut job.task_handle).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        // Extract result.
                        let result = result.unwrap_or_else(|join_error| {
                            Err(CertificationError::InternalError(join_error.to_string()))
                        });
                        let certificate_id = job.certificate_id;

                        // Mark the manager as idle.
                        *self = Self::Idle;

                        Poll::Ready(JobResult {
                            certificate_id,
                            result,
                        })
                    }
                },
            }
        }
    }

    pub struct Job {
        certificate_id: Hash,
        task_handle: JoinHandle<CertifierResult>,
    }

    pub struct JobResult {
        pub certificate_id: Hash,
        pub result: CertifierResult,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agglayer_storage::tests::mocks::{MockPendingStore, MockStateStore};
    use agglayer_types::CertificateHeader;
    use mockall::predicate::{always, eq};
    use rstest::rstest;

    use super::*;
    use crate::tests::{clock, mocks::MockCertifier};

    #[rstest]
    #[tokio::test]
    #[timeout(Duration::from_secs(1))]
    async fn start_from_zero() {
        let mut pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let mut certifier = MockCertifier::new();
        let (certification_notifier, mut receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (sender, certificate_stream) = mpsc::channel(1);

        let certificate = Certificate::new_for_test(network_id, 0);
        let certificate_id = certificate.hash();

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|_, _| Ok(None));

        state
            .expect_insert_certificate_header()
            .once()
            .returning(|_, _| Ok(()));

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        state
            .expect_write_local_network_state()
            .returning(|_, _, _| Ok(()));

        pending
            .expect_insert_pending_certificate()
            .once()
            .returning(|_, _, _| Ok(()));

        pending
            .expect_get_certificate()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });

        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id))
            .returning(|certificate_id| {
                Ok(Some(agglayer_types::CertificateHeader {
                    network_id: 1.into(),
                    height: 0,
                    epoch_number: None,
                    certificate_index: None,
                    certificate_id: *certificate_id,
                    prev_local_exit_root: [1; 32].into(),
                    new_local_exit_root: [0; 32].into(),
                    metadata: [0; 32].into(),
                    status: CertificateStatus::Pending,
                }))
            });

        certifier
            .expect_certify()
            .once()
            .with(always(), eq(network_id), eq(0))
            .return_once(move |new_state, network_id, _height| {
                Ok(Box::pin(async move {
                    let result = crate::CertifierOutput {
                        certificate,
                        height: 0,
                        new_state,
                        network: network_id,
                    };

                    Ok(result)
                }))
            });

        pending
            .expect_set_latest_proven_certificate_per_network()
            .once()
            .with(eq(network_id), eq(0), eq(certificate_id))
            .returning(|_, _, _| Ok(()));
        state
            .expect_update_certificate_header_status()
            .once()
            .with(eq(certificate_id), eq(CertificateStatus::Proven))
            .returning(|_, _| Ok(()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref,
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let mut epochs = task.clock_ref.subscribe().unwrap();
        let mut next_expected_height = 0;

        let (reply_sx, reply_rx) = oneshot::channel();
        let _ = sender
            .send((Certificate::new_for_test(network_id, 0), reply_sx))
            .await;

        tokio::spawn(async move {
            let (sender, cert) = receiver.recv().await.unwrap();

            _ = sender.send(Ok(SettledCertificate(cert.0, cert.2, 0, 0)));
        });

        // Initial certificate processing + pass it to the certifier.
        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 0);
        assert!(reply_rx.await.unwrap().is_ok());

        // Process the result of the certifier.
        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 1);
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(1))]
    async fn one_per_epoch() {
        let mut pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let mut certifier = MockCertifier::new();
        let (certification_notifier, mut receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (sender, certificate_stream) = mpsc::channel(100);

        let certificate = Certificate::new_for_test(network_id, 0);
        let certificate2 = Certificate::new_for_test(network_id, 1);
        let certificate_id = certificate.hash();
        let certificate_id2 = certificate2.hash();

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|_, _| Ok(None));

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(1))
            .returning(|_, _| Ok(None));

        state
            .expect_insert_certificate_header()
            .times(2)
            .returning(|_, _| Ok(()));

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        state
            .expect_write_local_network_state()
            .returning(|_, _, _| Ok(()));

        pending
            .expect_insert_pending_certificate()
            .times(2)
            .returning(|_, _, _| Ok(()));

        pending
            .expect_get_certificate()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });

        pending
            .expect_get_certificate()
            .never()
            .with(eq(network_id), eq(1))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });
        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id))
            .returning(|certificate_id| {
                Ok(Some(agglayer_types::CertificateHeader {
                    network_id: 1.into(),
                    height: 0,
                    epoch_number: None,
                    certificate_index: None,
                    certificate_id: *certificate_id,
                    prev_local_exit_root: [1; 32].into(),
                    new_local_exit_root: [0; 32].into(),
                    metadata: [0; 32].into(),
                    status: CertificateStatus::Pending,
                }))
            });

        state
            .expect_get_certificate_header()
            .never()
            .with(eq(certificate_id2))
            .returning(|certificate_id| {
                Ok(Some(agglayer_types::CertificateHeader {
                    network_id: 1.into(),
                    height: 1,
                    epoch_number: None,
                    certificate_index: None,
                    certificate_id: *certificate_id,
                    prev_local_exit_root: [1; 32].into(),
                    new_local_exit_root: [0; 32].into(),
                    metadata: [0; 32].into(),
                    status: CertificateStatus::Pending,
                }))
            });
        certifier
            .expect_certify()
            .once()
            .with(always(), eq(network_id), eq(0))
            .return_once(move |new_state, network_id, _height| {
                Ok(Box::pin(async move {
                    let result = crate::CertifierOutput {
                        certificate,
                        height: 0,
                        new_state,
                        network: network_id,
                    };

                    Ok(result)
                }))
            });

        certifier
            .expect_certify()
            .never()
            .with(always(), eq(network_id), eq(1))
            .return_once(move |new_state, network_id, _height| {
                Ok(Box::pin(async move {
                    let result = crate::CertifierOutput {
                        certificate: certificate2,
                        height: 1,
                        new_state,
                        network: network_id,
                    };

                    Ok(result)
                }))
            });
        pending
            .expect_set_latest_proven_certificate_per_network()
            .once()
            .with(eq(network_id), eq(0), eq(certificate_id))
            .returning(|_, _, _| Ok(()));
        state
            .expect_update_certificate_header_status()
            .once()
            .with(eq(certificate_id), eq(CertificateStatus::Proven))
            .returning(|_, _| Ok(()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref,
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let mut epochs = task.clock_ref.subscribe().unwrap();
        let mut next_expected_height = 0;

        let (reply0_sx, reply0_rx) = oneshot::channel();
        sender
            .send((Certificate::new_for_test(network_id, 0), reply0_sx))
            .await
            .expect("Failed to send the certificate");

        let (reply1_sx, mut reply1_rx) = oneshot::channel();
        sender
            .send((Certificate::new_for_test(network_id, 1), reply1_sx))
            .await
            .expect("Failed to send the certificate");

        tokio::spawn(async move {
            let (sender, cert) = receiver.recv().await.unwrap();

            sender
                .send(Ok(SettledCertificate(cert.0, cert.2, 0, 0)))
                .expect("Failed to send");
        });

        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 0);
        assert!(reply0_rx.await.unwrap().is_ok());

        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();
        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 1);

        tokio::time::timeout(
            Duration::from_millis(100),
            task.make_progress(&mut epochs, &mut next_expected_height),
        )
        .await
        .expect_err("Should have timed out");

        assert_eq!(next_expected_height, 1);

        assert!(reply1_rx.try_recv().is_ok());
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(1))]
    async fn changing_epoch_triggers_certify() {
        let mut pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let mut certifier = MockCertifier::new();
        let (certification_notifier, mut receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (sender, certificate_stream) = mpsc::channel(100);

        let certificate = Certificate::new_for_test(network_id, 0);
        let certificate2 = Certificate::new_for_test(network_id, 1);
        let certificate_id = certificate.hash();
        let certificate_id2 = certificate2.hash();

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|_, _| Ok(None));

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(1))
            .returning(|_, _| Ok(None));

        state
            .expect_insert_certificate_header()
            .returning(|_, _| Ok(()));

        pending
            .expect_insert_pending_certificate()
            .returning(|_, _, _| Ok(()));

        pending
            .expect_get_certificate()
            .once()
            .with(eq(network_id), eq(0))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });

        pending
            .expect_get_certificate()
            .once()
            .with(eq(network_id), eq(1))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        state
            .expect_write_local_network_state()
            .returning(|_, _, _| Ok(()));

        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id))
            .returning(|certificate_id| {
                Ok(Some(agglayer_types::CertificateHeader {
                    network_id: 1.into(),
                    height: 0,
                    epoch_number: None,
                    certificate_index: None,
                    certificate_id: *certificate_id,
                    new_local_exit_root: [0; 32].into(),
                    prev_local_exit_root: [1; 32].into(),
                    metadata: [0; 32].into(),
                    status: CertificateStatus::Pending,
                }))
            });

        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id2))
            .returning(|certificate_id| {
                Ok(Some(agglayer_types::CertificateHeader {
                    network_id: 1.into(),
                    height: 1,
                    epoch_number: None,
                    certificate_index: None,
                    certificate_id: *certificate_id,
                    prev_local_exit_root: [1; 32].into(),
                    new_local_exit_root: [0; 32].into(),
                    metadata: [0; 32].into(),
                    status: CertificateStatus::Pending,
                }))
            });
        certifier
            .expect_certify()
            .once()
            .with(always(), eq(network_id), eq(0))
            .return_once(move |new_state, network_id, _height| {
                Ok(Box::pin(async move {
                    let result = crate::CertifierOutput {
                        certificate,
                        height: 0,
                        new_state,
                        network: network_id,
                    };

                    Ok(result)
                }))
            });

        certifier
            .expect_certify()
            .once()
            .with(always(), eq(network_id), eq(1))
            .return_once(move |new_state, network_id, _height| {
                Ok(Box::pin(async move {
                    let result = crate::CertifierOutput {
                        certificate: certificate2,
                        height: 1,
                        new_state,
                        network: network_id,
                    };

                    Ok(result)
                }))
            });

        pending
            .expect_set_latest_proven_certificate_per_network()
            .once()
            .with(eq(network_id), eq(0), eq(certificate_id))
            .returning(|_, _, _| Ok(()));
        pending
            .expect_set_latest_proven_certificate_per_network()
            .once()
            .with(eq(network_id), eq(1), eq(certificate_id2))
            .returning(|_, _, _| Ok(()));

        state
            .expect_update_certificate_header_status()
            .once()
            .with(eq(certificate_id), eq(CertificateStatus::Proven))
            .returning(|_, _| Ok(()));

        state
            .expect_update_certificate_header_status()
            .once()
            .with(eq(certificate_id2), eq(CertificateStatus::Proven))
            .returning(|_, _| Ok(()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref.clone(),
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let mut epochs = task.clock_ref.subscribe().unwrap();
        let mut next_expected_height = 0;

        let (reply0_sx, reply0_rx) = oneshot::channel();
        sender
            .send((Certificate::new_for_test(network_id, 0), reply0_sx))
            .await
            .expect("Failed to send the certificate");

        let (reply1_sx, reply1_rx) = oneshot::channel();
        sender
            .send((Certificate::new_for_test(network_id, 1), reply1_sx))
            .await
            .expect("Failed to send the certificate");

        tokio::spawn(async move {
            let (sender, cert) = receiver.recv().await.unwrap();

            sender
                .send(Ok(SettledCertificate(cert.0, cert.2, 0, 0)))
                .expect("Failed to send");

            let (sender, cert) = receiver.recv().await.unwrap();

            sender
                .send(Ok(SettledCertificate(cert.0, cert.2, 1, 0)))
                .expect("Failed to send");
        });

        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 0);

        for _ in 0..2 {
            task.make_progress(&mut epochs, &mut next_expected_height)
                .await
                .unwrap();
        }

        assert_eq!(next_expected_height, 1);

        tokio::time::timeout(
            Duration::from_millis(100),
            task.make_progress(&mut epochs, &mut next_expected_height),
        )
        .await
        .expect_err("Should have timed out");

        assert_eq!(next_expected_height, 1);

        clock_ref
            .get_sender()
            .send(agglayer_clock::Event::EpochEnded(0))
            .expect("Failed to send");

        for _ in 0..2 {
            task.make_progress(&mut epochs, &mut next_expected_height)
                .await
                .unwrap();
        }

        assert_eq!(next_expected_height, 2);

        assert!(reply0_rx.await.unwrap().is_ok());
        assert!(reply1_rx.await.unwrap().is_ok());
    }

    const fn dummy_cert_header(
        network_id: NetworkId,
        height: u64,
        status: CertificateStatus,
    ) -> CertificateHeader {
        CertificateHeader {
            network_id,
            height,
            epoch_number: None,
            certificate_index: None,
            certificate_id: agglayer_types::Hash([0xab; 32]),
            prev_local_exit_root: agglayer_types::Hash([0xbc; 32]),
            new_local_exit_root: agglayer_types::Hash([0xcd; 32]),
            metadata: agglayer_types::Hash([0xef; 32]),
            status,
        }
    }

    #[rstest]
    #[case(None)]
    #[case(Some(dummy_cert_header(1.into(), 0, CertificateStatus::InError {
        error: CertificateStatusError::TrustedSequencerNotFound(1.into())
    })))]
    #[case(Some(dummy_cert_header(1.into(), 0, CertificateStatus::InError {
        error: CertificateStatusError::ProofVerificationFailed(
            agglayer_types::ProofVerificationError::InvalidPublicValues
        )
    })))]
    #[tokio::test]
    #[timeout(Duration::from_secs(1))]
    async fn process_certificate_ok(#[case] existing_cert_header: Option<CertificateHeader>) {
        let mut pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let certifier = MockCertifier::new();
        let (certification_notifier, mut _receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (_sender, certificate_stream) = mpsc::channel(100);

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(move |_network_id, _height| Ok(existing_cert_header.clone()));

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        state
            .expect_insert_certificate_header()
            .once()
            .returning(|_cert, _status| Ok(()));

        pending
            .expect_insert_pending_certificate()
            .once()
            .returning(|_net, _ht, _cert| Ok(()));

        state
            .expect_write_local_network_state()
            .returning(|_, _, _| Ok(()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref.clone(),
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let certificate = Certificate::new_for_test(network_id, 0);
        let result = task.process_certificate(&certificate, 0);
        assert!(result.is_ok());
    }

    #[rstest]
    #[case(CertificateStatus::Proven)]
    #[case(CertificateStatus::Pending)]
    #[case(CertificateStatus::Candidate)]
    #[case(CertificateStatus::Settled)]
    #[tokio::test]
    #[timeout(Duration::from_secs(1))]
    async fn replace_certificate_illegally(#[case] cur_cert_status: CertificateStatus) {
        let pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let certifier = MockCertifier::new();
        let (certification_notifier, mut _receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (_sender, certificate_stream) = mpsc::channel(100);

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(move |network_id, height| {
                Ok(Some(dummy_cert_header(
                    network_id,
                    height,
                    cur_cert_status.clone(),
                )))
            });

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref.clone(),
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let certificate = Certificate::new_for_test(network_id, 0);
        let result = task.process_certificate(&certificate, 0);
        assert!(matches!(
            result.unwrap_err(),
            InitialCheckError::IllegalReplacement { .. }
        ));
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(1))]
    async fn timeout_certifier() {
        let mut pending = MockPendingStore::new();
        let mut state = MockStateStore::new();
        let mut certifier = MockCertifier::new();
        let (certification_notifier, mut receiver) = mpsc::channel(1);
        let clock_ref = clock();
        let network_id = 1.into();
        let (sender, certificate_stream) = mpsc::channel(100);

        let certificate = Certificate::new_for_test(network_id, 0);
        let certificate_id = certificate.hash();

        state
            .expect_get_certificate_header_by_cursor()
            .once()
            .with(eq(network_id), eq(0))
            .returning(move |_network_id, _height| Ok(None));

        state
            .expect_insert_certificate_header()
            .once()
            .returning(|_cert, _status| Ok(()));

        pending
            .expect_insert_pending_certificate()
            .once()
            .returning(|_net, _ht, _cert| Ok(()));

        pending
            .expect_get_certificate()
            .times(2)
            .with(eq(network_id), eq(0))
            .returning(|network_id, height| {
                Ok(Some(Certificate::new_for_test(network_id, height)))
            });

        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id))
            .returning(move |_id| {
                Ok(Some(dummy_cert_header(
                    network_id,
                    0,
                    CertificateStatus::Pending,
                )))
            });

        state
            .expect_get_certificate_header()
            .once()
            .with(eq(certificate_id))
            .returning(move |_id| {
                Ok(Some(dummy_cert_header(
                    network_id,
                    0,
                    CertificateStatus::InError {
                        error: CertificateStatusError::InternalError("foo".into()),
                    },
                )))
            });

        certifier
            .expect_certify()
            .once()
            .with(always(), eq(network_id), eq(0))
            .return_once(move |_new_state, _network_id, _height| {
                Ok(Box::pin(async move {
                    Err(CertificationError::InternalError("TimedOut".to_string()))
                }))
            });

        let expected_error = format!(
            "Internal error happened in the certification process of {}: TimedOut",
            certificate_id
        );

        state
            .expect_update_certificate_header_status()
            .once()
            .with(
                eq(certificate_id),
                eq(CertificateStatus::InError {
                    error: CertificateStatusError::InternalError(expected_error),
                }),
            )
            .returning(|_, _| Ok(()));

        state
            .expect_read_local_network_state()
            .returning(|_| Ok(Default::default()));

        let mut task = NetworkTask::new(
            Arc::new(pending),
            Arc::new(state),
            Arc::new(certifier),
            certification_notifier,
            clock_ref.clone(),
            network_id,
            certificate_stream,
        )
        .expect("Failed to create a new network task");

        let mut epochs = task.clock_ref.subscribe().unwrap();
        let mut next_expected_height = 0;

        let (reply_sx, reply_rx) = oneshot::channel();
        sender
            .send((certificate, reply_sx))
            .await
            .expect("Failed to send the certificate");

        tokio::spawn(async move {
            let (sender, cert) = receiver.recv().await.unwrap();

            sender
                .send(Ok(SettledCertificate(cert.0, cert.2, 0, 0)))
                .expect("Failed to send");
        });

        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 0);

        task.make_progress(&mut epochs, &mut next_expected_height)
            .await
            .unwrap();

        assert_eq!(next_expected_height, 0);
        assert!(reply_rx.await.unwrap().is_ok());
    }
}
