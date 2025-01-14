use std::sync::Arc;

use apalis::{prelude::Storage, sqlite::SqliteStorage};
use async_graphql::{Context, Enum, InputObject, Object, Result, SimpleObject};
use chrono::{Duration, Utc};
use itertools::Itertools;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait,
    FromJsonQueryResult, QueryFilter,
};
use serde::{Deserialize, Serialize};

use crate::{
    background::ImportMedia,
    entities::{media_import_report, prelude::MediaImportReport},
    migrator::{MediaImportSource, MetadataLot},
    miscellaneous::resolver::MiscellaneousService,
    models::media::{
        AddMediaToCollection, CreateOrUpdateCollectionInput, ImportOrExportItem,
        ImportOrExportItemIdentifier, PostReviewInput, ProgressUpdateInput,
    },
    traits::AuthProvider,
    utils::MemoryDatabase,
};

mod goodreads;
mod media_json;
mod media_tracker;
mod movary;
mod story_graph;
mod trakt;

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployMediaTrackerImportInput {
    /// The base url where the resource is present at
    api_url: String,
    /// An application token generated by an admin
    api_key: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployGoodreadsImportInput {
    // The RSS url that can be found from the user's profile
    rss_url: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployTraktImportInput {
    // The public username in Trakt.
    username: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployMovaryImportInput {
    // The CSV contents of the history file.
    history: String,
    // The CSV contents of the ratings file.
    ratings: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployStoryGraphImportInput {
    // The CSV contents of the export file.
    export: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployMediaJsonImportInput {
    // The contents of the JSON export.
    export: String,
}

#[derive(Debug, InputObject, Serialize, Deserialize, Clone)]
pub struct DeployImportJobInput {
    pub source: MediaImportSource,
    pub media_tracker: Option<DeployMediaTrackerImportInput>,
    pub goodreads: Option<DeployGoodreadsImportInput>,
    pub trakt: Option<DeployTraktImportInput>,
    pub movary: Option<DeployMovaryImportInput>,
    pub story_graph: Option<DeployStoryGraphImportInput>,
    pub media_json: Option<DeployMediaJsonImportInput>,
}

/// The various steps in which media importing can fail
#[derive(Debug, Enum, PartialEq, Eq, Copy, Clone, Serialize, Deserialize)]
pub enum ImportFailStep {
    /// Failed to get details from the source itself (for eg: MediaTracker, Goodreads etc.)
    ItemDetailsFromSource,
    /// Failed to get metadata from the provider (for eg: Openlibrary, IGDB etc.)
    MediaDetailsFromProvider,
    /// Failed to transform the data into the required format
    InputTransformation,
    /// Failed to save a seen history item
    SeenHistoryConversion,
    /// Failed to save a review/rating item
    ReviewConversion,
}

#[derive(
    Debug, SimpleObject, FromJsonQueryResult, Serialize, Deserialize, Eq, PartialEq, Clone,
)]
pub struct ImportFailedItem {
    lot: MetadataLot,
    step: ImportFailStep,
    identifier: String,
    error: Option<String>,
}

#[derive(Debug, SimpleObject, Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct ImportDetails {
    pub total: usize,
}

#[derive(Debug)]
pub struct ImportResult {
    collections: Vec<CreateOrUpdateCollectionInput>,
    media: Vec<ImportOrExportItem<ImportOrExportItemIdentifier>>,
    failed_items: Vec<ImportFailedItem>,
}

#[derive(
    Debug, SimpleObject, Serialize, Deserialize, FromJsonQueryResult, Eq, PartialEq, Clone,
)]
pub struct ImportResultResponse {
    pub source: MediaImportSource,
    pub import: ImportDetails,
    pub failed_items: Vec<ImportFailedItem>,
}

#[derive(Default)]
pub struct ImporterQuery;

#[Object]
impl ImporterQuery {
    /// Get all the import jobs deployed by the user
    async fn media_import_reports(
        &self,
        gql_ctx: &Context<'_>,
    ) -> Result<Vec<media_import_report::Model>> {
        let service = gql_ctx.data_unchecked::<Arc<ImporterService>>();
        let user_id = service.user_id_from_ctx(gql_ctx).await?;
        service.media_import_reports(user_id).await
    }
}

#[derive(Default)]
pub struct ImporterMutation;

#[Object]
impl ImporterMutation {
    /// Add job to import data from various sources.
    async fn deploy_import_job(
        &self,
        gql_ctx: &Context<'_>,
        input: DeployImportJobInput,
    ) -> Result<String> {
        let service = gql_ctx.data_unchecked::<Arc<ImporterService>>();
        let user_id = service.user_id_from_ctx(gql_ctx).await?;
        service.deploy_import_job(user_id, input).await
    }
}

pub struct ImporterService {
    db: DatabaseConnection,
    media_service: Arc<MiscellaneousService>,
    import_media: SqliteStorage<ImportMedia>,
}

impl AuthProvider for ImporterService {
    fn get_auth_db(&self) -> &MemoryDatabase {
        self.media_service.get_auth_db()
    }
}

impl ImporterService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: &DatabaseConnection,
        media_service: Arc<MiscellaneousService>,
        import_media: &SqliteStorage<ImportMedia>,
    ) -> Self {
        Self {
            db: db.clone(),
            media_service,
            import_media: import_media.clone(),
        }
    }

    pub async fn deploy_import_job(
        &self,
        user_id: i32,
        mut input: DeployImportJobInput,
    ) -> Result<String> {
        let mut storage = self.import_media.clone();
        if let Some(s) = input.media_tracker.as_mut() {
            s.api_url = s.api_url.trim_end_matches('/').to_owned()
        }
        let job = storage.push(ImportMedia { user_id, input }).await.unwrap();
        Ok(job.to_string())
    }

    pub async fn invalidate_import_jobs(&self) -> Result<()> {
        let all_jobs = MediaImportReport::find()
            .filter(media_import_report::Column::Success.is_null())
            .all(&self.db)
            .await?;
        for job in all_jobs {
            if Utc::now() - job.started_on > Duration::hours(24) {
                tracing::trace!("Invalidating job with id = {id}", id = job.id);
                let mut job: media_import_report::ActiveModel = job.into();
                job.success = ActiveValue::Set(Some(false));
                job.save(&self.db).await?;
            }
        }
        Ok(())
    }

    pub async fn media_import_reports(
        &self,
        user_id: i32,
    ) -> Result<Vec<media_import_report::Model>> {
        self.media_service.media_import_reports(user_id).await
    }

    pub async fn import_from_source(
        &self,
        user_id: i32,
        input: DeployImportJobInput,
    ) -> Result<()> {
        let db_import_job = self
            .media_service
            .start_import_job(user_id, input.source)
            .await?;
        let mut import = match input.source {
            MediaImportSource::MediaTracker => {
                media_tracker::import(input.media_tracker.unwrap()).await?
            }
            MediaImportSource::MediaJson => media_json::import(input.media_json.unwrap()).await?,
            MediaImportSource::Goodreads => goodreads::import(input.goodreads.unwrap()).await?,
            MediaImportSource::Trakt => trakt::import(input.trakt.unwrap()).await?,
            MediaImportSource::Movary => movary::import(input.movary.unwrap()).await?,
            MediaImportSource::StoryGraph => {
                story_graph::import(
                    input.story_graph.unwrap(),
                    &self.media_service.openlibrary_service,
                )
                .await?
            }
        };
        import.media = import
            .media
            .into_iter()
            .sorted_unstable_by_key(|m| {
                m.seen_history.len() + m.reviews.len() + m.collections.len()
            })
            .rev()
            .collect_vec();
        for col_details in import.collections.into_iter() {
            self.media_service
                .create_or_update_collection(&user_id, col_details)
                .await?;
        }
        for (idx, item) in import.media.iter().enumerate() {
            tracing::debug!(
                "Importing media with identifier = {iden}",
                iden = item.source_id
            );
            let data = match &item.identifier {
                ImportOrExportItemIdentifier::NeedsDetails(i) => {
                    self.media_service
                        .commit_media(item.lot, item.source, i)
                        .await
                }
                ImportOrExportItemIdentifier::AlreadyFilled(a) => {
                    self.media_service.commit_media_internal(*a.clone()).await
                }
            };
            let metadata = match data {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("{e:?}");
                    import.failed_items.push(ImportFailedItem {
                        lot: item.lot,
                        step: ImportFailStep::MediaDetailsFromProvider,
                        identifier: item.source_id.to_owned(),
                        error: Some(e.message),
                    });
                    continue;
                }
            };
            for seen in item.seen_history.iter() {
                match self
                    .media_service
                    .progress_update(
                        ProgressUpdateInput {
                            metadata_id: metadata.id,
                            progress: Some(100),
                            date: seen.ended_on.map(|d| d.date_naive()),
                            show_season_number: seen.show_season_number,
                            show_episode_number: seen.show_episode_number,
                            podcast_episode_number: seen.podcast_episode_number,
                            change_state: None,
                        },
                        user_id,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => import.failed_items.push(ImportFailedItem {
                        lot: item.lot,
                        step: ImportFailStep::SeenHistoryConversion,
                        identifier: item.source_id.to_owned(),
                        error: Some(e.message),
                    }),
                };
            }
            for review in item.reviews.iter() {
                if review.review.is_none() && review.rating.is_none() {
                    tracing::debug!("Skipping review since it has no content");
                    continue;
                }
                let text = review.review.clone().and_then(|r| r.text);
                let spoiler = review.review.clone().map(|r| r.spoiler.unwrap_or(false));
                let date = review.review.clone().map(|r| r.date);
                match self
                    .media_service
                    .post_review(
                        &user_id,
                        PostReviewInput {
                            rating: review.rating,
                            text,
                            spoiler,
                            date: date.flatten(),
                            visibility: None,
                            metadata_id: metadata.id,
                            review_id: None,
                            show_season_number: review.show_season_number,
                            show_episode_number: review.show_episode_number,
                            podcast_episode_number: review.podcast_episode_number,
                        },
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => import.failed_items.push(ImportFailedItem {
                        lot: item.lot,
                        step: ImportFailStep::ReviewConversion,
                        identifier: item.source_id.to_owned(),
                        error: Some(e.message),
                    }),
                };
            }
            for col in item.collections.iter() {
                self.media_service
                    .create_or_update_collection(
                        &user_id,
                        CreateOrUpdateCollectionInput {
                            name: col.to_string(),
                            ..Default::default()
                        },
                    )
                    .await?;
                self.media_service
                    .add_media_to_collection(
                        &user_id,
                        AddMediaToCollection {
                            collection_name: col.to_string(),
                            media_id: metadata.id,
                        },
                    )
                    .await
                    .ok();
            }
            tracing::debug!(
                "Imported item: {idx}/{total}, lot: {lot}, history count: {hist}, review count: {rev}, collection count: {col}",
                idx = idx,
                total = import.media.len(),
                lot = item.lot,
                hist = item.seen_history.len(),
                rev = item.reviews.len(),
                col = item.collections.len(),
            );
        }
        self.media_service
            .deploy_recalculate_summary_job(user_id)
            .await
            .ok();
        tracing::trace!(
            "Imported {total} media items from {source}",
            total = import.media.len(),
            source = db_import_job.source
        );
        let details = ImportResultResponse {
            source: db_import_job.source,
            import: ImportDetails {
                total: import.media.len() - import.failed_items.len(),
            },
            failed_items: import.failed_items,
        };
        self.media_service
            .finish_import_job(db_import_job, details)
            .await?;
        Ok(())
    }
}
