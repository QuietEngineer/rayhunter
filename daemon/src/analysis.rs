use std::sync::Arc;
use std::{future, pin};

use axum::Json;
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use futures::TryStreamExt;
use log::{error, info};
use rayhunter::analysis::analyzer::{AnalyzerConfig, Harness};
use rayhunter::diag::{DataType, MessagesContainer};
use rayhunter::qmdl::QmdlReader;
use serde::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{RwLock, RwLockWriteGuard};
use tokio_util::task::TaskTracker;

use crate::qmdl_store::RecordingStore;
use crate::server::ServerState;

pub struct AnalysisWriter {
    writer: BufWriter<File>,
    harness: Harness,
}

// We write our analysis results to a file immediately to minimize the amount of
// state Rayhunter has to keep track of in memory. The analysis file's format is
// Newline Delimited JSON
// (https://docs.mulesoft.com/dataweave/latest/dataweave-formats-ndjson), which
// lets us simply append new rows to the end without parsing the entire JSON
// object beforehand.
impl AnalysisWriter {
    pub async fn new(file: File, analyzer_config: &AnalyzerConfig) -> Result<Self, std::io::Error> {
        let harness = Harness::new_with_config(analyzer_config);

        let mut result = Self {
            writer: BufWriter::new(file),
            harness,
        };
        let metadata = result.harness.get_metadata();
        result.write(&metadata).await?;
        Ok(result)
    }

    // Runs the analysis harness on the given container, serializing the results
    // to the analysis file, returning the whether any warnings were detected
    pub async fn analyze(&mut self, container: MessagesContainer) -> Result<bool, std::io::Error> {
        let mut warning_detected = false;
        for row in self.harness.analyze_qmdl_messages(container) {
            if !row.is_empty() {
                self.write(&row).await?;
            }
            warning_detected |= row.contains_warnings();
        }
        Ok(warning_detected)
    }

    async fn write<T: Serialize>(&mut self, value: &T) -> Result<(), std::io::Error> {
        let mut value_str = serde_json::to_string(value).unwrap();
        value_str.push('\n');
        self.writer.write_all(value_str.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    // Flushes any pending I/O to disk before dropping the writer
    pub async fn close(mut self) -> Result<(), std::io::Error> {
        self.writer.flush().await?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct AnalysisStatus {
    queued: Vec<String>,
    running: Option<String>,
    finished: Vec<String>,
}

impl AnalysisStatus {
    pub fn new(store: &RecordingStore) -> Self {
        let existing_recordings: Vec<String> = store
            .manifest
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        AnalysisStatus {
            queued: Vec::new(),
            running: None,
            finished: existing_recordings,
        }
    }
}

pub enum AnalysisCtrlMessage {
    NewFilesQueued,
    RecordingFinished(String),
    Exit,
}

async fn queued_len(analysis_status_lock: Arc<RwLock<AnalysisStatus>>) -> usize {
    analysis_status_lock.read().await.queued.len()
}

async fn dequeue_to_running(analysis_status_lock: Arc<RwLock<AnalysisStatus>>) -> String {
    let mut analysis_status = analysis_status_lock.write().await;
    let name = analysis_status.queued.remove(0);
    assert!(analysis_status.running.is_none());
    analysis_status.running = Some(name.clone());
    name
}

async fn finish_running_analysis(analysis_status_lock: Arc<RwLock<AnalysisStatus>>) {
    let mut analysis_status = analysis_status_lock.write().await;
    let finished = analysis_status.running.take().unwrap();
    analysis_status.finished.push(finished);
}

async fn perform_analysis(
    name: &str,
    qmdl_store_lock: Arc<RwLock<RecordingStore>>,
    analyzer_config: &AnalyzerConfig,
) -> Result<(), String> {
    info!("Opening QMDL and analysis file for {name}...");
    let (analysis_file, qmdl_file) = {
        let mut qmdl_store = qmdl_store_lock.write().await;
        let (entry_index, _) = qmdl_store
            .entry_for_name(name)
            .ok_or(format!("failed to find QMDL store entry for {name}"))?;
        let analysis_file = qmdl_store
            .clear_and_open_entry_analysis(entry_index)
            .await
            .map_err(|e| format!("{e:?}"))?;
        let qmdl_file = qmdl_store
            .open_entry_qmdl(entry_index)
            .await
            .map_err(|e| format!("{e:?}"))?;

        (analysis_file, qmdl_file)
    };

    let mut analysis_writer = AnalysisWriter::new(analysis_file, analyzer_config)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let file_size = qmdl_file
        .metadata()
        .await
        .expect("failed to get QMDL file metadata")
        .len();
    let mut qmdl_reader = QmdlReader::new(qmdl_file, Some(file_size as usize));
    let mut qmdl_stream = pin::pin!(
        qmdl_reader
            .as_stream()
            .try_filter(|container| future::ready(container.data_type == DataType::UserSpace))
    );

    info!("Starting analysis for {name}...");
    while let Some(container) = qmdl_stream
        .try_next()
        .await
        .expect("failed getting QMDL container")
    {
        let _ = analysis_writer
            .analyze(container)
            .await
            .map_err(|e| format!("{e:?}"))?;
    }

    analysis_writer
        .close()
        .await
        .map_err(|e| format!("{e:?}"))?;
    info!("Analysis for {name} complete!");

    Ok(())
}

pub fn run_analysis_thread(
    task_tracker: &TaskTracker,
    mut analysis_rx: Receiver<AnalysisCtrlMessage>,
    qmdl_store_lock: Arc<RwLock<RecordingStore>>,
    analysis_status_lock: Arc<RwLock<AnalysisStatus>>,
    analyzer_config: AnalyzerConfig,
) {
    task_tracker.spawn(async move {
        loop {
            match analysis_rx.recv().await {
                Some(AnalysisCtrlMessage::NewFilesQueued) => {
                    let count = queued_len(analysis_status_lock.clone()).await;
                    for _ in 0..count {
                        let name = dequeue_to_running(analysis_status_lock.clone()).await;
                        if let Err(err) =
                            perform_analysis(&name, qmdl_store_lock.clone(), &analyzer_config).await
                        {
                            error!("failed to analyze {name}: {err}");
                        }
                        finish_running_analysis(analysis_status_lock.clone()).await;
                    }
                }
                Some(AnalysisCtrlMessage::RecordingFinished(name)) => {
                    let mut status = analysis_status_lock.write().await;
                    status.finished.push(name);
                }
                Some(AnalysisCtrlMessage::Exit) | None => return,
            }
        }
    });
}

pub async fn get_analysis_status(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<AnalysisStatus>, (StatusCode, String)> {
    Ok(Json(state.analysis_status_lock.read().await.clone()))
}

fn queue_qmdl(name: &str, analysis_status: &mut RwLockWriteGuard<AnalysisStatus>) -> bool {
    if analysis_status.queued.iter().any(|n| n == name)
        || analysis_status.running.iter().any(|n| n == name)
    {
        return false;
    }
    analysis_status.queued.push(name.to_string());
    true
}

pub async fn start_analysis(
    State(state): State<Arc<ServerState>>,
    Path(qmdl_name): Path<String>,
) -> Result<(StatusCode, Json<AnalysisStatus>), (StatusCode, String)> {
    let mut analysis_status = state.analysis_status_lock.write().await;
    let store = state.qmdl_store_lock.read().await;
    let queued = if qmdl_name.is_empty() {
        let mut entry_names: Vec<&str> = store
            .manifest
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        if let Some(current_entry) = store.current_entry {
            entry_names.remove(current_entry);
        }
        entry_names
            .iter()
            .any(|name| queue_qmdl(name, &mut analysis_status))
    } else {
        queue_qmdl(&qmdl_name, &mut analysis_status)
    };
    if queued {
        state
            .analysis_sender
            .send(AnalysisCtrlMessage::NewFilesQueued)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to queue new analysis files: {e:?}"),
                )
            })?;
    }
    Ok((StatusCode::ACCEPTED, Json(analysis_status.clone())))
}
