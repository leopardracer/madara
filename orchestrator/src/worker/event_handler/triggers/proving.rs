use std::sync::Arc;

use crate::core::config::Config;
use crate::types::constant::PROOF_FILE_NAME;
use crate::types::jobs::metadata::{
    CommonMetadata, JobMetadata, JobSpecificMetadata, ProvingInputType, ProvingMetadata, SnosMetadata,
};
use crate::types::jobs::types::{JobStatus, JobType};
use crate::utils::metrics::ORCHESTRATOR_METRICS;
use crate::worker::event_handler::service::JobHandlerService;
use crate::worker::event_handler::triggers::JobTrigger;
use async_trait::async_trait;
use opentelemetry::KeyValue;
use orchestrator_utils::layer::Layer;

pub struct ProvingJobTrigger;

#[async_trait]
impl JobTrigger for ProvingJobTrigger {
    /// 1. Fetch all successful SNOS job runs that don't have a proving job
    /// 2. Create a proving job for each SNOS job run
    async fn run_worker(&self, config: Arc<Config>) -> color_eyre::Result<()> {
        tracing::info!(log_type = "starting", category = "ProvingWorker", "ProvingWorker started.");

        let successful_snos_jobs = config
            .database()
            .get_jobs_without_successor(JobType::SnosRun, JobStatus::Completed, JobType::ProofCreation)
            .await?;

        tracing::debug!("Found {} successful SNOS jobs without proving jobs", successful_snos_jobs.len());

        for snos_job in successful_snos_jobs {
            // Extract SNOS metadata
            let snos_metadata: SnosMetadata = snos_job.metadata.specific.try_into().map_err(|e| {
                tracing::error!(job_id = %snos_job.internal_id, error = %e, "Invalid metadata type for SNOS job");
                e
            })?;

            // Get SNOS fact early to handle the error case
            let snos_fact = match &snos_metadata.snos_fact {
                Some(fact) => fact.clone(),
                None => {
                    tracing::error!(job_id = %snos_job.internal_id, "SNOS fact not found in metadata");
                    continue;
                }
            };

            let download_proof = match config.layer() {
                Layer::L2 => None,
                Layer::L3 => Some(format!("{}/{}", snos_job.internal_id, PROOF_FILE_NAME)),
            };

            // Create proving job metadata
            let proving_metadata = JobMetadata {
                common: CommonMetadata::default(),
                specific: JobSpecificMetadata::Proving(ProvingMetadata {
                    block_number: snos_metadata.block_number,
                    // Set input path as CairoPie type
                    input_path: snos_metadata.cairo_pie_path.map(ProvingInputType::CairoPie),
                    // Set a download path if needed
                    download_proof,
                    // Set SNOS fact for on-chain verification
                    ensure_on_chain_registration: Some(snos_fact),
                    n_steps: snos_metadata.snos_n_steps,
                }),
            };

            tracing::debug!(job_id = %snos_job.internal_id, "Creating proof creation job for SNOS job");
            match JobHandlerService::create_job(
                JobType::ProofCreation,
                snos_job.internal_id.clone(),
                proving_metadata,
                config.clone(),
            )
            .await
            {
                Ok(_) => tracing::info!(block_id = %snos_job.internal_id, "Successfully created new proving job"),
                Err(e) => {
                    tracing::warn!(job_id = %snos_job.internal_id, error = %e, "Failed to create new proving job");
                    let attributes = [
                        KeyValue::new("operation_job_type", format!("{:?}", JobType::ProofCreation)),
                        KeyValue::new("operation_type", format!("{:?}", "create_job")),
                    ];
                    ORCHESTRATOR_METRICS.failed_job_operations.add(1.0, &attributes);
                }
            }
        }

        tracing::trace!(log_type = "completed", category = "ProvingWorker", "ProvingWorker completed.");
        Ok(())
    }
}
