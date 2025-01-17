use crate::pipeline::collector::DefaultCollector;
use crate::pipeline::PipelineContext;
use crate::search::Searcher;
use crate::state::AppState;
use crate::task::CrawlTask;

use entities::models::{crawl_queue, indexed_document};
use shared::config::{Config, LensConfig, PipelineConfiguration};
use tokio::sync::mpsc;

use super::parser::DefaultParser;
use super::PipelineCommand;
use entities::sea_orm::prelude::*;
use entities::sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use url::Url;

// General pipeline loop for configured pipelines. This code is responsible for
// processing pipeline requests against the provided pipeline configuration
// Note: This is still first draft. All stages of collection, parsing, tagging
// and indexing are meant to be configurable and extendable via feature plugins
pub async fn pipeline_loop(
    state: AppState,
    _config: Config,
    pipeline: String,
    _pipeline_cfg: PipelineConfiguration,
    mut pipeline_queue: mpsc::Receiver<PipelineCommand>,
) {
    let mut shutdown_rx = state.shutdown_cmd_tx.lock().await.subscribe();
    log::debug!("Default Pipeline Loop Started for Pipeline: {:?}", pipeline);

    let collector = DefaultCollector::new();
    let parser = DefaultParser::new();
    loop {
        log::debug!("Running pipeline loop");
        let next_thing = tokio::select! {
            res = pipeline_queue.recv() => {
                println!("Got item I think ");
                res

            }
            _ = shutdown_rx.recv() => {
                log::info!("🛑 Shutting down pipeline");
                return;
            }
        };

        if let Some(command) = next_thing {
            match command {
                PipelineCommand::ProcessUrl(pipeline, crawl_task) => {
                    log::debug!(
                        "Processing pipeline crawl command for pipeline {}",
                        pipeline
                    );
                    start_crawl(state.clone(), &pipeline, &collector, &parser, crawl_task).await;
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

// Starts the crawl process. This function will collect, parse, tag and index the
// contents of the requested link
async fn start_crawl(
    state: AppState,
    pipeline_name: &str,
    collector: &DefaultCollector,
    parser: &DefaultParser,
    task: CrawlTask,
) {
    log::debug!("Processing start crawl");

    let mut context = PipelineContext::new(pipeline_name, state.clone());

    let collection_result = collector.collect(&mut context, task.id).await;

    match collection_result {
        Ok(result) => {
            let parse_result = parser.parse(&mut context, task.id, &result.content).await;

            match parse_result {
                Ok(parse_result) => {
                    let crawl_result = parse_result.content;
                    // Update job status
                    let _ = crawl_queue::mark_done(&state.db, task.id, None).await;

                    // Add all valid, non-duplicate, non-indexed links found to crawl queue
                    let to_enqueue: Vec<String> = crawl_result.links.into_iter().collect();

                    // Collect all lenses with a pipeline ... any pipeline since there is only one
                    let lenses: Vec<LensConfig> = state
                        .lenses
                        .iter()
                        .filter(|entry| entry.value().pipeline.is_some())
                        .map(|entry| entry.value().clone())
                        .collect();

                    if let Err(err) = crawl_queue::enqueue_all(
                        &state.db,
                        &to_enqueue,
                        &lenses,
                        &state.user_settings,
                        &Default::default(),
                        Some(pipeline_name.to_owned()),
                    )
                    .await
                    {
                        log::error!("error enqueuing all: {}", err);
                    }

                    // Add / update search index w/ crawl result.
                    if let Some(content) = crawl_result.content {
                        log::debug!("Pipeline got content");
                        let url = Url::parse(&crawl_result.url).expect("Invalid crawl URL");
                        let url_host = url.host_str().expect("Invalid URL host");

                        let existing = indexed_document::Entity::find()
                            .filter(indexed_document::Column::Url.eq(url.as_str()))
                            .one(&state.db)
                            .await
                            .unwrap_or_default();

                        // Delete old document, if any.
                        if let Some(doc) = &existing {
                            let _ = Searcher::delete_by_id(&state, &doc.doc_id).await;
                            let _ = Searcher::save(&state);
                        }

                        // Add document to index
                        let doc_id: Option<String> = {
                            if let Ok(mut index_writer) = state.index.writer.lock() {
                                match Searcher::upsert_document(
                                    &mut index_writer,
                                    existing.clone().map(|f| f.doc_id),
                                    &crawl_result.title.unwrap_or_default(),
                                    &crawl_result.description.unwrap_or_default(),
                                    url_host,
                                    url.as_str(),
                                    &content,
                                ) {
                                    Ok(new_doc_id) => Some(new_doc_id),
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        };

                        if let Some(doc_id) = doc_id {
                            // Update/create index reference in our database
                            let indexed = if let Some(doc) = existing {
                                let mut update: indexed_document::ActiveModel = doc.into();
                                update.doc_id = Set(doc_id);
                                update
                            } else {
                                indexed_document::ActiveModel {
                                    domain: Set(url_host.to_string()),
                                    url: Set(url.as_str().to_string()),
                                    doc_id: Set(doc_id),
                                    ..Default::default()
                                }
                            };

                            if let Err(e) = indexed.save(&state.db).await {
                                log::error!("Unable to save document: {}", e);
                            }
                        }
                    }
                }
                Err(err) => {
                    log::info!("Unable to crawl id: {} - {:?}", task.id, err);
                    // mark crawl as failed
                    crawl_queue::mark_failed(&state.db, task.id, false).await;
                }
            }
        }
        Err(err) => {
            log::info!("Unable to crawl id: {} - {:?}", task.id, err);
            // mark crawl as failed
            crawl_queue::mark_failed(&state.db, task.id, false).await;
        }
    }
}
