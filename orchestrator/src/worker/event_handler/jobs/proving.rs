use crate::core::config::Config;
use crate::error::job::proving::ProvingError;
use crate::error::job::JobError;
use crate::error::other::OtherError;
use crate::types::jobs::job_item::JobItem;
use crate::types::jobs::metadata::{JobMetadata, ProvingInputType, ProvingMetadata};
use crate::types::jobs::status::JobVerificationStatus;
use crate::types::jobs::types::{JobStatus, JobType};
use crate::worker::event_handler::jobs::JobHandlerTrait;
use async_trait::async_trait;
use cairo_vm::vm::runners::cairo_pie::CairoPie;
use color_eyre::eyre::eyre;
use orchestrator_prover_client_interface::{Task, TaskStatus};
use std::sync::Arc;

pub struct ProvingJobHandler;

#[async_trait]
impl JobHandlerTrait for ProvingJobHandler {
    #[tracing::instrument(fields(category = "proving"), skip(self, metadata), ret, err)]
    async fn create_job(&self, internal_id: String, metadata: JobMetadata) -> Result<JobItem, JobError> {
        tracing::info!(log_type = "starting", category = "proving", function_type = "create_job",  block_no = %internal_id, "Proving job creation started.");
        let job_item = JobItem::create(internal_id.clone(), JobType::ProofCreation, JobStatus::Created, metadata);
        tracing::info!(log_type = "completed", category = "proving", function_type = "create_job",  block_no = %internal_id, "Proving job created.");
        Ok(job_item)
    }

    #[tracing::instrument(fields(category = "proving"), skip(self, config), ret, err)]
    async fn process_job(&self, config: Arc<Config>, job: &mut JobItem) -> Result<String, JobError> {
        tracing::info!("Proving job processing started");

        // Get proving metadata
        let proving_metadata: ProvingMetadata = job.metadata.specific.clone().try_into().inspect_err(|e| {
            tracing::error!(job_id = %job.internal_id, error = %e, "Failed to convert metadata to ProvingMetadata");
        })?;

        // Get an input path from metadata
        let input_path = match proving_metadata.input_path {
            Some(ProvingInputType::CairoPie(path)) => path,
            Some(ProvingInputType::Proof(_)) => {
                return Err(JobError::Other(OtherError(eyre!("Expected CairoPie input, got Proof"))));
            }
            None => return Err(JobError::Other(OtherError(eyre!("Input path not found in job metadata")))),
        };

        tracing::debug!(job_id = %job.internal_id, %input_path, "Fetching Cairo PIE file");

        // Fetch and parse Cairo PIE
        let cairo_pie_file = config.storage().get_data(&input_path).await.map_err(|e| {
            tracing::error!(job_id = %job.internal_id, error = %e, "Failed to fetch Cairo PIE file");
            ProvingError::CairoPIEFileFetchFailed(e.to_string())
        })?;

        tracing::debug!(job_id = %job.internal_id, "Parsing Cairo PIE file");
        let cairo_pie = Box::new(CairoPie::from_bytes(cairo_pie_file.to_vec().as_slice()).map_err(|e| {
            tracing::error!(job_id = %job.internal_id, error = %e, "Failed to parse Cairo PIE file");
            ProvingError::CairoPIENotReadable(e.to_string())
        })?);

        tracing::debug!(job_id = %job.internal_id, "Submitting task to prover client");

        let external_id = config
            .prover_client()
            .submit_task(Task::CairoPie(cairo_pie), *config.prover_layout_name(), proving_metadata.n_steps)
            .await
            .inspect_err(|e| {
                tracing::error!(job_id = %job.internal_id, error = %e, "Failed to submit task to prover client");
            })?;

        Ok(external_id)
    }

    #[tracing::instrument(fields(category = "proving"), skip(self, config), ret, err)]
    async fn verify_job(&self, config: Arc<Config>, job: &mut JobItem) -> Result<JobVerificationStatus, JobError> {
        let internal_id = job.internal_id.clone();
        tracing::info!(
            log_type = "starting",
            category = "proving",
            function_type = "verify_job",
            job_id = ?job.id,
            block_no = %internal_id,
            "Proving job verification started."
        );

        // Get proving metadata
        let proving_metadata: ProvingMetadata = job.metadata.specific.clone().try_into()?;

        // Get task ID from external_id
        let task_id: String = job
            .external_id
            .unwrap_string()
            .map_err(|e| {
                tracing::error!(job_id = %job.internal_id, error = %e, "Failed to unwrap external_id");
                JobError::Other(OtherError(e))
            })?
            .into();

        // Determine if we need on-chain verification
        let (cross_verify, fact) = match &proving_metadata.ensure_on_chain_registration {
            Some(fact_str) => (true, Some(fact_str.clone())),
            None => (false, None),
        };

        tracing::debug!(
            job_id = %job.internal_id,
            %task_id,
            cross_verify,
            "Getting task status from prover client"
        );

        let task_status =
            config.prover_client().get_task_status(&task_id, fact, cross_verify).await.inspect_err(|e| {
                tracing::error!(
                    job_id = %job.internal_id,
                    error = %e,
                    "Failed to get task status from prover client"
                );
            })?;

        match task_status {
            TaskStatus::Processing => {
                tracing::info!(
                    log_type = "pending",
                    category = "proving",
                    function_type = "verify_job",
                    job_id = ?job.id,
                    block_no = %internal_id,
                    "Proving job verification pending."
                );
                Ok(JobVerificationStatus::Pending)
            }
            TaskStatus::Succeeded => {
                // If proof download path is specified, store the proof
                if let Some(download_path) = &proving_metadata.download_proof {
                    let fetched_proof = config.prover_client().get_proof(&task_id).await.inspect_err(|e| {
                        tracing::error!(
                            job_id = %job.internal_id,
                            error = %e,
                            "Failed to get task status from prover client"
                        );
                    })?;
                    tracing::debug!(
                        job_id = %job.internal_id,
                        "Downloading and storing proof to path: {}",
                        download_path
                    );
                    config.storage().put_data(bytes::Bytes::from(fetched_proof.into_bytes()), download_path).await?;
                }
                tracing::info!(
                    log_type = "completed",
                    category = "proving",
                    function_type = "verify_job",
                    job_id = ?job.id,
                    block_no = %internal_id,
                    "Proving job verification completed."
                );
                Ok(JobVerificationStatus::Verified)
            }
            TaskStatus::Failed(err) => {
                tracing::info!(
                    log_type = "failed",
                    category = "proving",
                    function_type = "verify_job",
                    job_id = ?job.id,
                    block_no = %internal_id,
                    "Proving job verification failed."
                );
                Ok(JobVerificationStatus::Rejected(format!(
                    "Prover job #{} failed with error: {}",
                    job.internal_id, err
                )))
            }
        }
    }

    fn max_process_attempts(&self) -> u64 {
        2
    }

    fn max_verification_attempts(&self) -> u64 {
        300
    }

    fn verification_polling_delay_seconds(&self) -> u64 {
        30
    }
}
