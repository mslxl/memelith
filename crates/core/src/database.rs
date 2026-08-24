use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use image::{DynamicImage, ImageFormat as DecodedImageFormat, ImageReader};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CollectorContent, CollectorDuplicate, CollectorDuplicateSource, CollectorDuplicateTarget,
    CollectorItem, EffectiveTag, EmbeddingProvider, Error, ImageDuplicate, ImageFormat, Meme,
    MemeContent, MemeImage, MemeMotion, MemePack, MemeText, MotionFormat, NewMeme, NewMemeContent,
    NewMemeFromCollector, NewMemePack, NewTag, Result, SimilarMemeImage, Tag, UpdateMemeMetadata,
    UpdateMemePack,
};

const SCHEMA_VERSION: i64 = 1;
const CONTENT_HASH_BYTES: usize = 32;
const DATABASE_FILENAME: &str = "memelith.sqlite3";
const MEDIA_DIRECTORY: &str = "media/images";
const STAGING_DIRECTORY: &str = ".staging";

pub const COLLECTOR_DUPLICATE_MAX_COSINE_DISTANCE: f32 = 0.05;

pub struct MemeDatabase {
    storage_root: PathBuf,
    database_path: PathBuf,
    connection: Connection,
    embedding_provider: Box<dyn EmbeddingProvider>,
    embedding_dimension: usize,
}

impl std::fmt::Debug for MemeDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemeDatabase")
            .field("storage_root", &self.storage_root)
            .field("database_path", &self.database_path)
            .field("embedding_model", &self.embedding_provider.model_id())
            .field("embedding_dimension", &self.embedding_dimension)
            .finish_non_exhaustive()
    }
}

impl MemeDatabase {
    pub fn open(
        storage_root: impl AsRef<Path>,
        embedding_provider: impl EmbeddingProvider + 'static,
    ) -> Result<Self> {
        let model_id = embedding_provider.model_id().trim();
        if model_id.is_empty() {
            return Err(Error::EmptyEmbeddingModelId);
        }
        let embedding_dimension = embedding_provider.dimension();
        if embedding_dimension == 0 {
            return Err(Error::EmptyEmbeddingDimension);
        }

        let requested_root = storage_root.as_ref();
        if requested_root.exists() && !requested_root.is_dir() {
            return Err(Error::InvalidStorageRoot(requested_root.to_path_buf()));
        }
        fs::create_dir_all(requested_root)?;
        let storage_root = fs::canonicalize(requested_root)?;
        fs::create_dir_all(storage_root.join(MEDIA_DIRECTORY))?;
        fs::create_dir_all(storage_root.join(STAGING_DIRECTORY))?;

        let database_path = storage_root.join(DATABASE_FILENAME);
        let mut connection = Connection::open(&database_path)?;
        configure_connection(&connection)?;
        initialize_database(&mut connection, model_id, embedding_dimension)?;

        let database = Self {
            storage_root,
            database_path,
            connection,
            embedding_provider: Box::new(embedding_provider),
            embedding_dimension,
        };
        database.clean_staging_directory()?;
        database.collect_orphaned_media()?;
        Ok(database)
    }

    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn resolve_media_path(&self, relative_path: impl AsRef<Path>) -> Result<PathBuf> {
        resolve_media_path(&self.storage_root, relative_path.as_ref())
    }

    pub fn resolve_media_path_from_root(
        storage_root: impl AsRef<Path>,
        relative_path: impl AsRef<Path>,
    ) -> Result<PathBuf> {
        resolve_media_path(storage_root.as_ref(), relative_path.as_ref())
    }

    pub fn find_similar_images(
        &mut self,
        source_path: impl AsRef<Path>,
        max_cosine_distance: f32,
    ) -> Result<Vec<SimilarMemeImage>> {
        if !max_cosine_distance.is_finite() || !(0.0..=2.0).contains(&max_cosine_distance) {
            return Err(Error::InvalidCosineDistanceThreshold(max_cosine_distance));
        }

        let analyzed = self.analyze_image_source(source_path.as_ref().to_path_buf())?;
        let mut statement = self.connection.prepare(
            "SELECT c.id, c.meme_id, m.name,
                    CASE WHEN c.kind = 'motion' THEN c.preview_relative_path ELSE c.relative_path END,
                    c.embedding
             FROM meme_contents c
             JOIN memes m ON m.id = c.meme_id
             WHERE c.kind IN ('image', 'motion') AND c.embedding IS NOT NULL",
        )?;
        let candidates = statement
            .query_map([], |row| {
                Ok((
                    uuid_from_column(row, 0)?,
                    uuid_from_column(row, 1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut matches = Vec::new();
        for (content_id, meme_id, meme_name, relative_path, embedding) in candidates {
            let Some(relative_path) = relative_path else {
                continue;
            };
            let relative_path = PathBuf::from(relative_path);
            let absolute_path = resolve_media_path(&self.storage_root, &relative_path)?;
            if !absolute_path.is_file() {
                return Err(Error::MissingMedia(absolute_path));
            }
            let distance = cosine_distance_between_encoded_embeddings(
                &analyzed.embedding,
                &embedding,
                self.embedding_dimension,
            )?;
            if distance <= max_cosine_distance {
                matches.push(SimilarMemeImage {
                    content_id,
                    meme_id,
                    meme_name,
                    relative_path,
                    cosine_distance: distance,
                });
            }
        }
        matches.sort_by(|left, right| left.cosine_distance.total_cmp(&right.cosine_distance));
        Ok(matches)
    }

    /// Finds memes whose stored CLIP embeddings are closest to a text query.
    /// A meme is represented by the best matching embedding across its own
    /// metadata, text contents, media previews, and (for text-only memes) tags.
    pub fn search_memes_semantic(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::SemanticMemeMatch>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let vector = self
            .embedding_provider
            .embed_text(query)
            .map_err(Error::EmbeddingProvider)?;
        let query_embedding = encode_embedding("semantic query", vector, self.embedding_dimension)?;
        let mut scores = std::collections::HashMap::<Uuid, f32>::new();
        let mut content_memes = std::collections::HashSet::<Uuid>::new();
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.name_embedding, m.description_embedding, c.embedding
             FROM memes m
             LEFT JOIN meme_contents c ON c.meme_id = m.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                uuid_from_column(row, 0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?;
        for row in rows {
            let (meme_id, name, description, content) = row?;
            if let Some(embedding) = content {
                let similarity = cosine_similarity_between_encoded_embeddings(
                    &query_embedding,
                    &embedding,
                    self.embedding_dimension,
                )?;
                scores
                    .entry(meme_id)
                    .and_modify(|score| *score = score.max(similarity))
                    .or_insert(similarity);
                content_memes.insert(meme_id);
            } else {
                for embedding in [name, description].into_iter().flatten() {
                    let similarity = cosine_similarity_between_encoded_embeddings(
                        &query_embedding,
                        &embedding,
                        self.embedding_dimension,
                    )?;
                    scores
                        .entry(meme_id)
                        .and_modify(|score| *score = score.max(similarity))
                        .or_insert(similarity);
                }
            }
        }

        let mut statement = self.connection.prepare(
            "SELECT mt.meme_id, t.name_embedding
             FROM meme_tags mt JOIN tags t ON t.id = mt.tag_id
             UNION ALL
             SELECT m.id, t.name_embedding
             FROM memes m
             JOIN meme_pack_tags pt ON pt.meme_pack_id = m.meme_pack_id
             JOIN tags t ON t.id = pt.tag_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((uuid_from_column(row, 0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (meme_id, embedding) = row?;
            if content_memes.contains(&meme_id) {
                continue;
            }
            let similarity = cosine_similarity_between_encoded_embeddings(
                &query_embedding,
                &embedding,
                self.embedding_dimension,
            )?;
            scores
                .entry(meme_id)
                .and_modify(|score| *score = score.max(similarity))
                .or_insert(similarity);
        }

        let mut matches = scores
            .into_iter()
            .map(|(meme_id, similarity)| crate::SemanticMemeMatch {
                meme_id,
                similarity,
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| left.meme_id.cmp(&right.meme_id))
        });
        if limit > 0 {
            matches.truncate(limit);
        }
        Ok(matches)
    }

    /// Finds exact and visually similar images without changing the database.
    ///
    /// This is used by integrations that need to ask for confirmation before
    /// inserting a new Collector item. Meme contents and existing Collector
    /// images are both considered candidates.
    pub fn find_image_duplicates(
        &mut self,
        source_path: impl AsRef<Path>,
        max_cosine_distance: f32,
    ) -> Result<Vec<ImageDuplicate>> {
        let source_path = source_path.as_ref();
        self.find_media_duplicates(source_path, Some(source_path), max_cosine_distance)
    }

    pub fn find_media_duplicates(
        &mut self,
        source_path: impl AsRef<Path>,
        preview_path: Option<&Path>,
        max_cosine_distance: f32,
    ) -> Result<Vec<ImageDuplicate>> {
        if !max_cosine_distance.is_finite() || !(0.0..=2.0).contains(&max_cosine_distance) {
            return Err(Error::InvalidCosineDistanceThreshold(max_cosine_distance));
        }

        let source_path = source_path.as_ref();
        if !fs::metadata(source_path)?.is_file() {
            return Err(Error::InvalidMotionMedia(source_path.to_path_buf()));
        }
        let incoming_content_hash = sha256_file(source_path)?;
        let analyzed_preview = preview_path
            .map(|path| self.analyze_image_source(path.to_path_buf()))
            .transpose()?;
        let mut candidates = Vec::new();
        {
            let mut statement = self.connection.prepare(
                "SELECT c.id, c.meme_id, m.name,
                        CASE WHEN c.kind = 'motion' THEN c.preview_relative_path ELSE c.relative_path END,
                        c.content_hash, c.embedding
                 FROM meme_contents c
                 JOIN memes m ON m.id = c.meme_id
                 WHERE c.kind IN ('image', 'motion')",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    uuid_from_column(row, 0)?,
                    CollectorDuplicateSource::Meme,
                    Some(uuid_from_column(row, 1)?),
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })?;
            candidates.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }
        {
            let mut statement = self.connection.prepare(
                "SELECT id,
                        CASE WHEN kind = 'motion' THEN preview_relative_path ELSE relative_path END,
                        content_hash, embedding
                 FROM collector_items
                 WHERE kind IN ('image', 'motion')",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    uuid_from_column(row, 0)?,
                    CollectorDuplicateSource::Collector,
                    None,
                    None,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })?;
            candidates.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }

        let mut matches = Vec::new();
        for (
            content_id,
            source,
            meme_id,
            meme_name,
            preview_relative_path,
            content_hash,
            embedding,
        ) in candidates
        {
            let preview_relative_path = preview_relative_path.map(PathBuf::from);
            if let Some(preview_relative_path) = &preview_relative_path {
                let absolute_path = resolve_media_path(&self.storage_root, preview_relative_path)?;
                if !absolute_path.is_file() {
                    return Err(Error::MissingMedia(absolute_path));
                }
            }
            let cosine_distance = if content_hash == incoming_content_hash {
                None
            } else {
                let Some(analyzed_preview) = analyzed_preview.as_ref() else {
                    continue;
                };
                let Some(embedding) = embedding.as_deref() else {
                    continue;
                };
                let distance = cosine_distance_between_encoded_embeddings(
                    &analyzed_preview.embedding,
                    embedding,
                    self.embedding_dimension,
                )?;
                if distance > max_cosine_distance {
                    continue;
                }
                Some(distance)
            };
            matches.push(ImageDuplicate {
                content_id,
                source,
                meme_id,
                meme_name,
                preview_relative_path,
                cosine_distance,
            });
        }
        matches.sort_by(
            |left, right| match (left.cosine_distance, right.cosine_distance) {
                (None, None) => left.content_id.cmp(&right.content_id),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(left), Some(right)) => left.total_cmp(&right),
            },
        );
        Ok(matches)
    }

    pub fn collect_image(&mut self, source_path: impl AsRef<Path>) -> Result<CollectorItem> {
        let content = self.prepare_content(NewMemeContent::Image {
            source_path: source_path.as_ref().to_path_buf(),
        })?;
        self.collect_prepared_content(content)
    }

    pub fn collect_motion(
        &mut self,
        source_path: impl AsRef<Path>,
        preview_path: Option<PathBuf>,
        width: u32,
        height: u32,
        format: MotionFormat,
    ) -> Result<CollectorItem> {
        let content = self.prepare_content(NewMemeContent::Motion {
            source_path: source_path.as_ref().to_path_buf(),
            preview_path,
            width,
            height,
            format,
        })?;
        self.collect_prepared_content(content)
    }

    pub fn collect_text(&mut self, text: String) -> Result<CollectorItem> {
        let content = self.prepare_content(NewMemeContent::Text { text })?;
        self.collect_prepared_content(content)
    }

    pub fn get_collector_item(&self, id: Uuid) -> Result<CollectorItem> {
        let raw = self
            .connection
            .query_row(
                "SELECT id, kind, text, relative_path, preview_relative_path, width, height,
                        byte_size, image_format, motion_format, content_hash, embedding,
                        duplicate_kind, duplicate_target_source, duplicate_target_id,
                        duplicate_distance, duplicate_dismissed
                 FROM collector_items
                 WHERE id = ?1",
                [id.to_string()],
                map_raw_collector_item,
            )
            .optional()?
            .ok_or(Error::CollectorItemNotFound(id))?;
        raw.into_domain(&self.connection, &self.storage_root)
    }

    pub fn list_collector_items(&self) -> Result<Vec<CollectorItem>> {
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT id, kind, text, relative_path, preview_relative_path, width, height,
                        byte_size, image_format, motion_format, content_hash, embedding,
                        duplicate_kind, duplicate_target_source, duplicate_target_id,
                        duplicate_distance, duplicate_dismissed
                 FROM collector_items
                 ORDER BY rowid DESC",
            )?;
            statement
                .query_map([], map_raw_collector_item)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|row| row.into_domain(&self.connection, &self.storage_root))
            .collect()
    }

    pub fn recheck_collector_items(&mut self) -> Result<Vec<CollectorItem>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT id, kind, text, relative_path, preview_relative_path, width, height,
                        byte_size, image_format, motion_format, content_hash, embedding,
                        duplicate_kind, duplicate_target_source, duplicate_target_id,
                        duplicate_distance, duplicate_dismissed
                 FROM collector_items
                 ORDER BY rowid DESC",
            )?;
            statement
                .query_map([], map_raw_collector_item)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for row in &rows {
            Self::recheck_collector_duplicate(&transaction, self.embedding_dimension, row)?;
        }
        transaction.commit()?;
        self.list_collector_items()
    }

    pub fn dismiss_collector_similarity(&mut self, id: Uuid) -> Result<CollectorItem> {
        let duplicate_kind = self
            .connection
            .query_row(
                "SELECT duplicate_kind FROM collector_items WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or(Error::CollectorItemNotFound(id))?;
        if duplicate_kind.as_deref() != Some("similarity") {
            return Err(Error::CollectorSimilarityNotDismissible(id));
        }
        self.connection.execute(
            "UPDATE collector_items
             SET duplicate_kind = NULL, duplicate_target_source = NULL,
                 duplicate_target_id = NULL, duplicate_distance = NULL,
                 duplicate_dismissed = 1
             WHERE id = ?1",
            [id.to_string()],
        )?;
        self.get_collector_item(id)
    }

    pub fn delete_collector_item(&mut self, id: Uuid) -> Result<()> {
        let item = self.get_collector_item(id)?;
        let media = match item.content {
            CollectorContent::Image { relative_path, .. } => {
                vec![resolve_media_path(&self.storage_root, &relative_path)?]
            }
            CollectorContent::Motion {
                relative_path,
                preview_relative_path,
                ..
            } => {
                let mut paths = vec![resolve_media_path(&self.storage_root, &relative_path)?];
                if let Some(preview_relative_path) = preview_relative_path {
                    paths.push(resolve_media_path(
                        &self.storage_root,
                        &preview_relative_path,
                    )?);
                }
                paths
            }
            CollectorContent::Text { .. } => Vec::new(),
        };
        let transaction = self.connection.transaction()?;
        let deleted = transaction.execute(
            "DELETE FROM collector_items WHERE id = ?1",
            [id.to_string()],
        )?;
        if deleted != 1 {
            return Err(Error::CollectorItemNotFound(id));
        }
        transaction.commit()?;
        remove_managed_files(&media)?;
        Ok(())
    }

    pub fn promote_collector_items(
        &mut self,
        meme_pack_id: Uuid,
        item_ids: Vec<Uuid>,
        input: NewMemeFromCollector,
    ) -> Result<Meme> {
        if item_ids.is_empty() {
            return Err(Error::EmptyCollectorSelection);
        }
        self.ensure_meme_pack(meme_pack_id)?;
        let mut unique_ids = HashSet::with_capacity(item_ids.len());
        for id in &item_ids {
            if !unique_ids.insert(*id) {
                return Err(Error::DuplicateCollectorSelection(*id));
            }
        }
        let NewMemeFromCollector {
            name,
            description,
            tags,
        } = input;
        let (name, description) = validate_meme_metadata(name, description)?;
        let name_embedding = self.embed_optional_text("Meme name", name.as_deref())?;
        let description_embedding =
            self.embed_optional_text("Meme description", description.as_deref())?;
        let tags = self.prepare_tags(tags)?;
        let meme_id = Uuid::new_v4();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut promoted_contents = Vec::with_capacity(item_ids.len());

        for id in &item_ids {
            let raw = transaction
                .query_row(
                    "SELECT id, kind, text, relative_path, preview_relative_path, width, height,
                            byte_size, image_format, motion_format, content_hash, embedding,
                            duplicate_kind, duplicate_target_source, duplicate_target_id,
                            duplicate_distance, duplicate_dismissed
                     FROM collector_items
                     WHERE id = ?1",
                    [id.to_string()],
                    map_raw_collector_item,
                )
                .optional()?
                .ok_or(Error::CollectorItemNotFound(*id))?;
            let duplicate =
                Self::recheck_collector_duplicate(&transaction, self.embedding_dimension, &raw)?;
            if duplicate.is_some() {
                transaction.commit()?;
                return Err(Error::DuplicateCollectorItem(*id));
            }
            let item = raw.into_domain(&transaction, &self.storage_root)?;
            promoted_contents.push(collector_item_into_meme_content(item));
        }

        transaction.execute(
            "INSERT INTO memes(
                id, meme_pack_id, name, name_embedding, description, description_embedding
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                meme_id.to_string(),
                meme_pack_id.to_string(),
                name,
                name_embedding,
                description,
                description_embedding,
            ],
        )?;
        for (position, id) in item_ids.iter().enumerate() {
            let inserted = transaction.execute(
                "INSERT INTO meme_contents(
                    id, meme_id, position, kind, text, relative_path, preview_relative_path,
                    width, height, byte_size, image_format, motion_format, content_hash, embedding
                 )
                 SELECT id, ?1, ?2, kind, text, relative_path, preview_relative_path,
                        width, height, byte_size, image_format, motion_format, content_hash, embedding
                 FROM collector_items
                 WHERE id = ?3 AND duplicate_kind IS NULL",
                params![
                    meme_id.to_string(),
                    usize_to_i64(position, "Meme content position")?,
                    id.to_string(),
                ],
            )?;
            if inserted != 1 {
                return Err(Error::InvalidDatabase(format!(
                    "Collector item {id} changed while it was being promoted"
                )));
            }
            transaction.execute(
                "UPDATE collector_items
                 SET duplicate_target_source = 'meme'
                 WHERE duplicate_target_source = 'collector' AND duplicate_target_id = ?1",
                [id.to_string()],
            )?;
            let deleted = transaction.execute(
                "DELETE FROM collector_items WHERE id = ?1",
                [id.to_string()],
            )?;
            if deleted != 1 {
                return Err(Error::InvalidDatabase(format!(
                    "Collector item {id} disappeared while it was being promoted"
                )));
            }
        }
        attach_prepared_tags(&transaction, meme_id, &tags)?;
        transaction.commit()?;
        Ok(Meme {
            id: meme_id,
            meme_pack_id,
            name,
            description,
            contents: promoted_contents,
        })
    }

    pub fn create_meme_pack(&mut self, input: NewMemePack) -> Result<MemePack> {
        let input = validate_meme_pack_input(input)?;
        let name_embedding = self.embed_text("MemePack name", &input.name)?;
        let description_embedding =
            self.embed_optional_text("MemePack description", input.description.as_deref())?;
        let id = Uuid::new_v4();

        self.connection.execute(
            "INSERT INTO meme_packs(
                id, name, name_embedding, description, description_embedding, author, source
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.to_string(),
                input.name,
                name_embedding,
                input.description,
                description_embedding,
                input.author,
                input.source,
            ],
        )?;

        self.get_meme_pack(id)
    }

    pub fn get_meme_pack(&self, id: Uuid) -> Result<MemePack> {
        self.connection
            .query_row(
                "SELECT id, name, description, author, source FROM meme_packs WHERE id = ?1",
                [id.to_string()],
                map_meme_pack,
            )
            .optional()?
            .ok_or(Error::MemePackNotFound(id))
    }

    pub fn list_meme_packs(&self) -> Result<Vec<MemePack>> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, description, author, source
             FROM meme_packs
             ORDER BY name COLLATE NOCASE, id",
        )?;
        statement
            .query_map([], map_meme_pack)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn update_meme_pack(&mut self, id: Uuid, input: UpdateMemePack) -> Result<MemePack> {
        self.ensure_meme_pack(id)?;
        let input = validate_update_meme_pack_input(input)?;
        let name_embedding = self.embed_text("MemePack name", &input.name)?;
        let description_embedding =
            self.embed_optional_text("MemePack description", input.description.as_deref())?;
        self.connection.execute(
            "UPDATE meme_packs
             SET name = ?2, name_embedding = ?3, description = ?4,
                 description_embedding = ?5, author = ?6, source = ?7
             WHERE id = ?1",
            params![
                id.to_string(),
                input.name,
                name_embedding,
                input.description,
                description_embedding,
                input.author,
                input.source,
            ],
        )?;
        self.get_meme_pack(id)
    }

    pub fn delete_meme_pack(&mut self, id: Uuid) -> Result<()> {
        self.ensure_meme_pack(id)?;
        let media = self.media_paths_for_meme_pack(id)?;
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM meme_packs WHERE id = ?1", [id.to_string()])?;
        transaction.commit()?;
        remove_managed_files(&media)
    }

    pub fn create_meme(&mut self, meme_pack_id: Uuid, input: NewMeme) -> Result<Meme> {
        self.ensure_meme_pack(meme_pack_id)?;
        let (name, description) = validate_meme_metadata(input.name, input.description)?;
        if input.contents.is_empty() {
            return Err(Error::EmptyMemeContents);
        }
        let name_embedding = self.embed_optional_text("Meme name", name.as_deref())?;
        let description_embedding =
            self.embed_optional_text("Meme description", description.as_deref())?;
        let contents = self.prepare_contents(input.contents)?;
        let mut pending_files =
            PendingFiles::stage(&self.storage_root.join(STAGING_DIRECTORY), &contents)?;
        let id = Uuid::new_v4();

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO memes(
                id, meme_pack_id, name, name_embedding, description, description_embedding
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                meme_pack_id.to_string(),
                name,
                name_embedding,
                description,
                description_embedding,
            ],
        )?;
        insert_contents(&transaction, id, &contents)?;
        pending_files.promote()?;
        transaction.commit()?;
        pending_files.disarm();
        self.get_meme(id)
    }

    pub fn get_meme(&self, id: Uuid) -> Result<Meme> {
        let raw = self
            .connection
            .query_row(
                "SELECT id, meme_pack_id, name, description FROM memes WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        uuid_from_column(row, 0)?,
                        uuid_from_column(row, 1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::MemeNotFound(id))?;
        let contents = self.contents_for_meme(id)?;
        if contents.is_empty() {
            return Err(Error::InvalidDatabase(format!(
                "Meme {id} has no content blocks"
            )));
        }
        Ok(Meme {
            id: raw.0,
            meme_pack_id: raw.1,
            name: raw.2,
            description: raw.3,
            contents,
        })
    }

    pub fn list_memes(&self, meme_pack_id: Uuid) -> Result<Vec<Meme>> {
        self.ensure_meme_pack(meme_pack_id)?;
        let ids = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM memes WHERE meme_pack_id = ?1 ORDER BY rowid")?;
            statement
                .query_map([meme_pack_id.to_string()], |row| uuid_from_column(row, 0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        ids.into_iter().map(|id| self.get_meme(id)).collect()
    }

    pub fn find_meme_with_media_in_pack(
        &self,
        meme_pack_id: Uuid,
        source_path: impl AsRef<Path>,
    ) -> Result<Option<Uuid>> {
        self.ensure_meme_pack(meme_pack_id)?;
        let source_path = source_path.as_ref();
        if !fs::metadata(source_path)?.is_file() {
            return Err(Error::InvalidMotionMedia(source_path.to_path_buf()));
        }
        let content_hash = sha256_file(source_path)?;
        self.connection
            .query_row(
                "SELECT m.id
                 FROM meme_contents c
                 JOIN memes m ON m.id = c.meme_id
                 WHERE m.meme_pack_id = ?1
                   AND c.kind IN ('image', 'motion')
                   AND c.content_hash = ?2
                 ORDER BY m.rowid
                 LIMIT 1",
                params![meme_pack_id.to_string(), content_hash],
                |row| uuid_from_column(row, 0),
            )
            .optional()
            .map_err(Error::from)
    }

    /// Lists every Meme independently of its MemePack, newest first.
    pub fn list_all_memes(&self) -> Result<Vec<Meme>> {
        let ids = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM memes ORDER BY rowid DESC")?;
            statement
                .query_map([], |row| uuid_from_column(row, 0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        ids.into_iter().map(|id| self.get_meme(id)).collect()
    }

    pub fn update_meme_metadata(&mut self, id: Uuid, input: UpdateMemeMetadata) -> Result<Meme> {
        self.ensure_meme(id)?;
        let (name, description) = validate_meme_metadata(input.name, input.description)?;
        let name_embedding = self.embed_optional_text("Meme name", name.as_deref())?;
        let description_embedding =
            self.embed_optional_text("Meme description", description.as_deref())?;
        self.connection.execute(
            "UPDATE memes
             SET name = ?2, name_embedding = ?3,
                 description = ?4, description_embedding = ?5
             WHERE id = ?1",
            params![
                id.to_string(),
                name,
                name_embedding,
                description,
                description_embedding,
            ],
        )?;
        self.get_meme(id)
    }

    pub fn move_meme(&mut self, id: Uuid, destination_meme_pack_id: Uuid) -> Result<Meme> {
        self.ensure_meme(id)?;
        self.ensure_meme_pack(destination_meme_pack_id)?;
        self.connection.execute(
            "UPDATE memes SET meme_pack_id = ?2 WHERE id = ?1",
            params![id.to_string(), destination_meme_pack_id.to_string()],
        )?;
        self.get_meme(id)
    }

    pub fn replace_meme_contents(
        &mut self,
        id: Uuid,
        contents: Vec<NewMemeContent>,
    ) -> Result<Meme> {
        self.ensure_meme(id)?;
        if contents.is_empty() {
            return Err(Error::EmptyMemeContents);
        }
        let old_media = self.media_paths_for_meme(id)?;
        let contents = self.prepare_contents(contents)?;
        let mut pending_files =
            PendingFiles::stage(&self.storage_root.join(STAGING_DIRECTORY), &contents)?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM meme_contents WHERE meme_id = ?1",
            [id.to_string()],
        )?;
        insert_contents(&transaction, id, &contents)?;
        pending_files.promote()?;
        transaction.commit()?;
        pending_files.disarm();
        remove_managed_files(&old_media)?;
        self.get_meme(id)
    }

    pub fn delete_meme(&mut self, id: Uuid) -> Result<()> {
        self.ensure_meme(id)?;
        let media = self.media_paths_for_meme(id)?;
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM memes WHERE id = ?1", [id.to_string()])?;
        transaction.commit()?;
        remove_managed_files(&media)
    }

    pub fn create_tag(&mut self, input: NewTag) -> Result<Tag> {
        let name = required_text(input.name, "Tag name")?;
        let normalized_name = normalize_tag_name(&name);
        if self.tag_name_exists(&normalized_name, None)? {
            return Err(Error::DuplicateTag(name));
        }
        let name_embedding = self.embed_text("Tag name", &name)?;
        let id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO tags(id, name, normalized_name, name_embedding)
             VALUES (?1, ?2, ?3, ?4)",
            params![id.to_string(), name, normalized_name, name_embedding],
        )?;
        self.get_tag(id)
    }

    pub fn get_tag(&self, id: Uuid) -> Result<Tag> {
        self.connection
            .query_row(
                "SELECT id, name FROM tags WHERE id = ?1",
                [id.to_string()],
                map_tag,
            )
            .optional()?
            .ok_or(Error::TagNotFound(id))
    }

    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, name FROM tags ORDER BY normalized_name, id")?;
        statement
            .query_map([], map_tag)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn rename_tag(&mut self, id: Uuid, name: String) -> Result<Tag> {
        self.ensure_tag(id)?;
        let name = required_text(name, "Tag name")?;
        let normalized_name = normalize_tag_name(&name);
        if self.tag_name_exists(&normalized_name, Some(id))? {
            return Err(Error::DuplicateTag(name));
        }
        let name_embedding = self.embed_text("Tag name", &name)?;
        self.connection.execute(
            "UPDATE tags
             SET name = ?2, normalized_name = ?3, name_embedding = ?4
             WHERE id = ?1",
            params![id.to_string(), name, normalized_name, name_embedding],
        )?;
        self.get_tag(id)
    }

    pub fn delete_tag(&mut self, id: Uuid) -> Result<()> {
        self.ensure_tag(id)?;
        self.connection
            .execute("DELETE FROM tags WHERE id = ?1", [id.to_string()])?;
        Ok(())
    }

    pub fn attach_tag_to_meme_pack(&mut self, meme_pack_id: Uuid, tag_id: Uuid) -> Result<()> {
        self.ensure_meme_pack(meme_pack_id)?;
        self.ensure_tag(tag_id)?;
        insert_tag_association(
            &self.connection,
            "meme_pack_tags",
            "meme_pack_id",
            "MemePack",
            meme_pack_id,
            tag_id,
        )
    }

    pub fn detach_tag_from_meme_pack(&mut self, meme_pack_id: Uuid, tag_id: Uuid) -> Result<()> {
        self.ensure_meme_pack(meme_pack_id)?;
        self.ensure_tag(tag_id)?;
        delete_tag_association(
            &self.connection,
            "meme_pack_tags",
            "meme_pack_id",
            "MemePack",
            meme_pack_id,
            tag_id,
        )
    }

    pub fn attach_tag_to_meme(&mut self, meme_id: Uuid, tag_id: Uuid) -> Result<()> {
        self.ensure_meme(meme_id)?;
        self.ensure_tag(tag_id)?;
        insert_tag_association(
            &self.connection,
            "meme_tags",
            "meme_id",
            "Meme",
            meme_id,
            tag_id,
        )
    }

    pub fn detach_tag_from_meme(&mut self, meme_id: Uuid, tag_id: Uuid) -> Result<()> {
        self.ensure_meme(meme_id)?;
        self.ensure_tag(tag_id)?;
        delete_tag_association(
            &self.connection,
            "meme_tags",
            "meme_id",
            "Meme",
            meme_id,
            tag_id,
        )
    }

    pub fn list_meme_pack_tags(&self, meme_pack_id: Uuid) -> Result<Vec<Tag>> {
        self.ensure_meme_pack(meme_pack_id)?;
        let mut statement = self.connection.prepare(
            "SELECT t.id, t.name
             FROM tags t
             JOIN meme_pack_tags pt ON pt.tag_id = t.id
             WHERE pt.meme_pack_id = ?1
             ORDER BY t.normalized_name, t.id",
        )?;
        statement
            .query_map([meme_pack_id.to_string()], map_tag)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn list_meme_direct_tags(&self, meme_id: Uuid) -> Result<Vec<Tag>> {
        self.ensure_meme(meme_id)?;
        let mut statement = self.connection.prepare(
            "SELECT t.id, t.name
             FROM tags t
             JOIN meme_tags mt ON mt.tag_id = t.id
             WHERE mt.meme_id = ?1
             ORDER BY t.normalized_name, t.id",
        )?;
        statement
            .query_map([meme_id.to_string()], map_tag)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn list_meme_effective_tags(&self, meme_id: Uuid) -> Result<Vec<EffectiveTag>> {
        self.ensure_meme(meme_id)?;
        let mut statement = self.connection.prepare(
            "SELECT
                 t.id,
                 t.name,
                 EXISTS(
                     SELECT 1 FROM meme_tags mt
                     WHERE mt.meme_id = ?1 AND mt.tag_id = t.id
                 ) AS direct,
                 EXISTS(
                     SELECT 1
                     FROM memes m
                     JOIN meme_pack_tags pt ON pt.meme_pack_id = m.meme_pack_id
                     WHERE m.id = ?1 AND pt.tag_id = t.id
                 ) AS inherited
             FROM tags t
             WHERE direct OR inherited
             ORDER BY t.normalized_name, t.id",
        )?;
        statement
            .query_map([meme_id.to_string()], |row| {
                Ok(EffectiveTag {
                    tag: Tag {
                        id: uuid_from_column(row, 0)?,
                        name: row.get(1)?,
                    },
                    direct: row.get(2)?,
                    inherited: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn list_all_meme_effective_tags(&self) -> Result<Vec<(Uuid, EffectiveTag)>> {
        let mut statement = self.connection.prepare(
            "SELECT meme_id, tag_id, name, MAX(is_direct), MAX(is_inherited)
             FROM (
                 SELECT
                     mt.meme_id AS meme_id,
                     t.id AS tag_id,
                     t.name AS name,
                     t.normalized_name AS normalized_name,
                     1 AS is_direct,
                     0 AS is_inherited
                 FROM meme_tags mt
                 JOIN tags t ON t.id = mt.tag_id
                 UNION ALL
                 SELECT
                     m.id AS meme_id,
                     t.id AS tag_id,
                     t.name AS name,
                     t.normalized_name AS normalized_name,
                     0 AS is_direct,
                     1 AS is_inherited
                 FROM memes m
                 JOIN meme_pack_tags pt ON pt.meme_pack_id = m.meme_pack_id
                 JOIN tags t ON t.id = pt.tag_id
             )
             GROUP BY meme_id, tag_id, name, normalized_name
             ORDER BY meme_id, normalized_name, tag_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    uuid_from_column(row, 0)?,
                    EffectiveTag {
                        tag: Tag {
                            id: uuid_from_column(row, 1)?,
                            name: row.get(2)?,
                        },
                        direct: row.get(3)?,
                        inherited: row.get(4)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    fn embed_text(&mut self, field: &'static str, text: &str) -> Result<Vec<u8>> {
        let vector = self
            .embedding_provider
            .embed_text(text)
            .map_err(Error::EmbeddingProvider)?;
        encode_embedding(field, vector, self.embedding_dimension)
    }

    fn embed_optional_text(
        &mut self,
        field: &'static str,
        text: Option<&str>,
    ) -> Result<Option<Vec<u8>>> {
        text.map(|text| self.embed_text(field, text)).transpose()
    }

    fn prepare_tags(&mut self, tags: Vec<String>) -> Result<Vec<PreparedTag>> {
        let mut normalized_names = HashSet::with_capacity(tags.len());
        let mut prepared = Vec::with_capacity(tags.len());
        for name in tags {
            let name = required_text(name, "Tag name")?;
            let normalized_name = normalize_tag_name(&name);
            if !normalized_names.insert(normalized_name.clone()) {
                continue;
            }
            let embedding = self.embed_text("Tag name", &name)?;
            prepared.push(PreparedTag {
                name,
                normalized_name,
                embedding,
            });
        }
        Ok(prepared)
    }

    fn prepare_contents(&mut self, contents: Vec<NewMemeContent>) -> Result<Vec<PreparedContent>> {
        contents
            .into_iter()
            .map(|content| self.prepare_content(content))
            .collect()
    }

    fn collect_prepared_content(&mut self, content: PreparedContent) -> Result<CollectorItem> {
        let id = content.id();
        let mut pending_files = PendingFiles::stage(
            &self.storage_root.join(STAGING_DIRECTORY),
            std::slice::from_ref(&content),
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate = Self::find_collector_duplicate(
            &transaction,
            self.embedding_dimension,
            content.kind(),
            content.content_hash(),
            content.embedding(),
            None,
            true,
        )?;
        insert_collector_content(&transaction, &content, duplicate.as_ref())?;
        pending_files.promote()?;
        transaction.commit()?;
        pending_files.disarm();
        self.get_collector_item(id)
    }

    fn recheck_collector_duplicate(
        connection: &Connection,
        embedding_dimension: usize,
        row: &RawCollectorItem,
    ) -> Result<Option<DetectedDuplicate>> {
        let duplicate = Self::find_collector_duplicate(
            connection,
            embedding_dimension,
            &row.kind,
            &row.content_hash,
            row.embedding.as_deref(),
            Some(row.id),
            !row.duplicate_dismissed,
        )?;
        let dismissed =
            row.duplicate_dismissed && !matches!(&duplicate, Some(DetectedDuplicate::Hash { .. }));
        update_collector_duplicate(connection, row.id, duplicate.as_ref(), dismissed)?;
        Ok(duplicate)
    }

    fn find_collector_duplicate(
        connection: &Connection,
        embedding_dimension: usize,
        kind: &str,
        content_hash: &[u8],
        embedding: Option<&[u8]>,
        collector_predecessor_of: Option<Uuid>,
        allow_similarity: bool,
    ) -> Result<Option<DetectedDuplicate>> {
        if let Some(target) =
            Self::find_hash_duplicate(connection, kind, content_hash, collector_predecessor_of)?
        {
            return Ok(Some(DetectedDuplicate::Hash { target }));
        }
        if !allow_similarity {
            return Ok(None);
        }
        let Some(embedding) = embedding else {
            return Ok(None);
        };

        let mut closest: Option<(DuplicateReference, f32)> = None;
        for candidate in Self::duplicate_candidates(connection, kind, collector_predecessor_of)? {
            let distance = cosine_distance_between_encoded_embeddings(
                embedding,
                &candidate.embedding,
                embedding_dimension,
            )?;
            if distance > COLLECTOR_DUPLICATE_MAX_COSINE_DISTANCE {
                continue;
            }
            if closest
                .as_ref()
                .is_none_or(|(_, closest_distance)| distance < *closest_distance)
            {
                closest = Some((candidate.target, distance));
            }
        }
        Ok(
            closest.map(|(target, cosine_distance)| DetectedDuplicate::Similarity {
                target,
                cosine_distance,
            }),
        )
    }

    fn find_hash_duplicate(
        connection: &Connection,
        kind: &str,
        content_hash: &[u8],
        collector_predecessor_of: Option<Uuid>,
    ) -> Result<Option<DuplicateReference>> {
        let map_target = |row: &rusqlite::Row<'_>| {
            Ok(DuplicateReference {
                source: CollectorDuplicateSource::Collector,
                content_id: uuid_from_column(row, 0)?,
            })
        };
        let collector = match collector_predecessor_of {
            Some(current) => connection
                .query_row(
                    "SELECT id FROM collector_items
                     WHERE kind = ?1 AND content_hash = ?2
                       AND rowid < (SELECT rowid FROM collector_items WHERE id = ?3)
                     ORDER BY rowid DESC LIMIT 1",
                    params![kind, content_hash, current.to_string()],
                    map_target,
                )
                .optional()?,
            None => connection
                .query_row(
                    "SELECT id FROM collector_items
                     WHERE kind = ?1 AND content_hash = ?2
                     ORDER BY rowid DESC LIMIT 1",
                    params![kind, content_hash],
                    map_target,
                )
                .optional()?,
        };
        if collector.is_some() {
            return Ok(collector);
        }
        connection
            .query_row(
                "SELECT id FROM meme_contents
                 WHERE kind = ?1 AND content_hash = ?2
                 ORDER BY rowid DESC LIMIT 1",
                params![kind, content_hash],
                |row| {
                    Ok(DuplicateReference {
                        source: CollectorDuplicateSource::Meme,
                        content_id: uuid_from_column(row, 0)?,
                    })
                },
            )
            .optional()
            .map_err(Error::from)
    }

    fn duplicate_candidates(
        connection: &Connection,
        kind: &str,
        collector_predecessor_of: Option<Uuid>,
    ) -> Result<Vec<DuplicateCandidate>> {
        let mut candidates = Vec::new();
        {
            let (sql, current) = match collector_predecessor_of {
                Some(id) => (
                    "SELECT id, embedding FROM collector_items
                     WHERE kind = ?1 AND embedding IS NOT NULL
                       AND rowid < (SELECT rowid FROM collector_items WHERE id = ?2)
                     ORDER BY rowid DESC",
                    Some(id.to_string()),
                ),
                None => (
                    "SELECT id, embedding FROM collector_items
                     WHERE kind = ?1 AND embedding IS NOT NULL ORDER BY rowid DESC",
                    None,
                ),
            };
            let mut statement = connection.prepare(sql)?;
            let map_candidate = |row: &rusqlite::Row<'_>| {
                Ok(DuplicateCandidate {
                    target: DuplicateReference {
                        source: CollectorDuplicateSource::Collector,
                        content_id: uuid_from_column(row, 0)?,
                    },
                    embedding: row.get(1)?,
                })
            };
            let rows = match current.as_deref() {
                Some(current) => statement.query_map(params![kind, current], map_candidate)?,
                None => statement.query_map([kind], map_candidate)?,
            };
            candidates.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }
        {
            let mut statement = connection.prepare(
                "SELECT id, embedding FROM meme_contents
                 WHERE kind = ?1 AND embedding IS NOT NULL ORDER BY rowid DESC",
            )?;
            let rows = statement.query_map([kind], |row| {
                Ok(DuplicateCandidate {
                    target: DuplicateReference {
                        source: CollectorDuplicateSource::Meme,
                        content_id: uuid_from_column(row, 0)?,
                    },
                    embedding: row.get(1)?,
                })
            })?;
            candidates.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }
        Ok(candidates)
    }

    fn prepare_content(&mut self, content: NewMemeContent) -> Result<PreparedContent> {
        let id = Uuid::new_v4();
        match content {
            NewMemeContent::Text { text } => {
                let text = required_text(text, "Meme text")?;
                let embedding = self.embed_text("Meme text", &text)?;
                let content_hash = sha256_bytes(text.as_bytes());
                Ok(PreparedContent::Text {
                    id,
                    text,
                    content_hash,
                    embedding,
                })
            }
            NewMemeContent::Image { source_path } => {
                let analyzed = self.analyze_image_source(source_path)?;
                let relative_path = PathBuf::from(format!(
                    "{MEDIA_DIRECTORY}/{id}.{}",
                    analyzed.format.extension()
                ));
                let final_path = resolve_media_path(&self.storage_root, &relative_path)?;
                Ok(PreparedContent::Image {
                    id,
                    source_path: analyzed.source_path,
                    relative_path,
                    final_path,
                    width: analyzed.width,
                    height: analyzed.height,
                    byte_size: analyzed.byte_size,
                    format: analyzed.format,
                    content_hash: analyzed.content_hash,
                    embedding: analyzed.embedding,
                })
            }
            NewMemeContent::Motion {
                source_path,
                preview_path,
                width,
                height,
                format,
            } => {
                if width == 0 || height == 0 || !fs::metadata(&source_path)?.is_file() {
                    return Err(Error::InvalidMotionMedia(source_path));
                }
                let content_hash = sha256_file(&source_path)?;
                let byte_size = fs::metadata(&source_path)?.len();
                if byte_size == 0 {
                    return Err(Error::InvalidMotionMedia(source_path));
                }
                let relative_path =
                    PathBuf::from(format!("{MEDIA_DIRECTORY}/{id}.{}", format.extension()));
                let final_path = resolve_media_path(&self.storage_root, &relative_path)?;
                let preview = preview_path
                    .map(|preview_path| self.analyze_image_source(preview_path))
                    .transpose()?;
                let (preview_source_path, preview_relative_path, preview_final_path, embedding) =
                    match preview {
                        Some(preview) => {
                            let relative_path = PathBuf::from(format!(
                                "{MEDIA_DIRECTORY}/{id}.preview.{}",
                                preview.format.extension()
                            ));
                            let final_path =
                                resolve_media_path(&self.storage_root, &relative_path)?;
                            (
                                Some(preview.source_path),
                                Some(relative_path),
                                Some(final_path),
                                Some(preview.embedding),
                            )
                        }
                        None => (None, None, None, None),
                    };
                if sha256_file(&source_path)? != content_hash {
                    return Err(Error::SourceMediaChanged(source_path));
                }
                Ok(PreparedContent::Motion {
                    id,
                    source_path,
                    relative_path,
                    final_path,
                    preview_source_path,
                    preview_relative_path,
                    preview_final_path,
                    width,
                    height,
                    byte_size,
                    format,
                    content_hash,
                    embedding,
                })
            }
        }
    }

    fn analyze_image_source(&mut self, source_path: PathBuf) -> Result<AnalyzedImageSource> {
        if !fs::metadata(&source_path)?.is_file() {
            return Err(Error::UnsupportedImageFormat(source_path));
        }
        let content_hash = sha256_file(&source_path)?;
        let (format, decoded) = decode_supported_image(&source_path)?;
        let width = decoded.width();
        let height = decoded.height();
        let vector = self
            .embedding_provider
            .embed_image(&decoded)
            .map_err(Error::EmbeddingProvider)?;
        let embedding = encode_embedding("Meme image", vector, self.embedding_dimension)?;
        if sha256_file(&source_path)? != content_hash {
            return Err(Error::SourceMediaChanged(source_path));
        }
        let byte_size = fs::metadata(&source_path)?.len();
        Ok(AnalyzedImageSource {
            source_path,
            width,
            height,
            byte_size,
            format,
            content_hash,
            embedding,
        })
    }

    fn contents_for_meme(&self, meme_id: Uuid) -> Result<Vec<MemeContent>> {
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT id, kind, text, relative_path, preview_relative_path, width, height,
                        byte_size, image_format, motion_format
                 FROM meme_contents
                 WHERE meme_id = ?1
                 ORDER BY position",
            )?;
            statement
                .query_map([meme_id.to_string()], |row| {
                    Ok(RawContent {
                        id: uuid_from_column(row, 0)?,
                        kind: row.get(1)?,
                        text: row.get(2)?,
                        relative_path: row.get(3)?,
                        preview_relative_path: row.get(4)?,
                        width: row.get(5)?,
                        height: row.get(6)?,
                        byte_size: row.get(7)?,
                        image_format: row.get(8)?,
                        motion_format: row.get(9)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|row| row.into_domain(&self.storage_root))
            .collect()
    }

    fn ensure_meme_pack(&self, id: Uuid) -> Result<()> {
        ensure_exists(&self.connection, "meme_packs", id)?
            .then_some(())
            .ok_or(Error::MemePackNotFound(id))
    }

    fn ensure_meme(&self, id: Uuid) -> Result<()> {
        ensure_exists(&self.connection, "memes", id)?
            .then_some(())
            .ok_or(Error::MemeNotFound(id))
    }

    fn ensure_tag(&self, id: Uuid) -> Result<()> {
        ensure_exists(&self.connection, "tags", id)?
            .then_some(())
            .ok_or(Error::TagNotFound(id))
    }

    fn tag_name_exists(&self, normalized_name: &str, except: Option<Uuid>) -> Result<bool> {
        let found = match except {
            Some(id) => self.connection.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM tags WHERE normalized_name = ?1 AND id <> ?2
                 )",
                params![normalized_name, id.to_string()],
                |row| row.get(0),
            )?,
            None => self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE normalized_name = ?1)",
                [normalized_name],
                |row| row.get(0),
            )?,
        };
        Ok(found)
    }

    fn media_paths_for_meme(&self, meme_id: Uuid) -> Result<Vec<PathBuf>> {
        self.query_media_paths(
            "SELECT relative_path FROM meme_contents WHERE meme_id = ?1 AND kind IN ('image', 'motion')
             UNION ALL
             SELECT preview_relative_path FROM meme_contents
             WHERE meme_id = ?1 AND kind = 'motion' AND preview_relative_path IS NOT NULL",
            meme_id,
        )
    }

    fn media_paths_for_meme_pack(&self, meme_pack_id: Uuid) -> Result<Vec<PathBuf>> {
        self.query_media_paths(
            "SELECT c.relative_path
             FROM meme_contents c
             JOIN memes m ON m.id = c.meme_id
             WHERE m.meme_pack_id = ?1 AND c.kind IN ('image', 'motion')
             UNION ALL
             SELECT c.preview_relative_path
             FROM meme_contents c
             JOIN memes m ON m.id = c.meme_id
             WHERE m.meme_pack_id = ?1 AND c.kind = 'motion'
               AND c.preview_relative_path IS NOT NULL",
            meme_pack_id,
        )
    }

    fn query_media_paths(&self, sql: &str, owner_id: Uuid) -> Result<Vec<PathBuf>> {
        let relative_paths = {
            let mut statement = self.connection.prepare(sql)?;
            statement
                .query_map([owner_id.to_string()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        relative_paths
            .into_iter()
            .map(|relative| {
                let absolute = resolve_media_path(&self.storage_root, Path::new(&relative))?;
                if !absolute.is_file() {
                    return Err(Error::MissingMedia(absolute));
                }
                Ok(absolute)
            })
            .collect()
    }

    fn clean_staging_directory(&self) -> Result<()> {
        let staging = self.storage_root.join(STAGING_DIRECTORY);
        for entry in fs::read_dir(staging)? {
            let path = entry?.path();
            if path.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn collect_orphaned_media(&self) -> Result<()> {
        let referenced = {
            let mut statement = self.connection.prepare(
                "SELECT relative_path FROM meme_contents WHERE kind IN ('image', 'motion')
                 UNION
                 SELECT preview_relative_path FROM meme_contents
                 WHERE kind = 'motion' AND preview_relative_path IS NOT NULL
                 UNION
                 SELECT relative_path FROM collector_items WHERE kind IN ('image', 'motion')
                 UNION
                 SELECT preview_relative_path FROM collector_items
                 WHERE kind = 'motion' AND preview_relative_path IS NOT NULL",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?
        };
        let media_directory = self.storage_root.join(MEDIA_DIRECTORY);
        for entry in fs::read_dir(media_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let filename = entry.file_name();
            let filename = filename.to_str().ok_or_else(|| {
                Error::InvalidDatabase("managed media filename is not valid UTF-8".to_owned())
            })?;
            let relative = format!("{MEDIA_DIRECTORY}/{filename}");
            if !referenced.contains(&relative) {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(Error::InvalidDatabase(format!(
            "SQLite refused WAL journal mode and returned `{journal_mode}`"
        )));
    }
    Ok(())
}

fn initialize_database(
    connection: &mut Connection,
    model_id: &str,
    embedding_dimension: usize,
) -> Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => create_schema(connection, model_id, embedding_dimension),
        SCHEMA_VERSION => validate_database_metadata(connection, model_id, embedding_dimension),
        actual => Err(Error::UnsupportedSchemaVersion {
            expected: SCHEMA_VERSION,
            actual,
        }),
    }
}

fn create_schema(
    connection: &mut Connection,
    model_id: &str,
    embedding_dimension: usize,
) -> Result<()> {
    let embedding_bytes = embedding_dimension
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| Error::InvalidDatabase("embedding byte length exceeds usize".to_owned()))?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(&format!(
        "CREATE TABLE metadata (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            schema_version INTEGER NOT NULL,
            embedding_model_id TEXT NOT NULL CHECK(trim(embedding_model_id) <> ''),
            embedding_dimension INTEGER NOT NULL CHECK(embedding_dimension > 0)
        );
        CREATE TABLE meme_packs (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL CHECK(trim(name) <> ''),
            name_embedding BLOB NOT NULL CHECK(length(name_embedding) = {embedding_bytes}),
            description TEXT,
            description_embedding BLOB,
            author TEXT,
            source TEXT,
            CHECK(
                (description IS NULL AND description_embedding IS NULL) OR
                (description IS NOT NULL AND trim(description) <> '' AND
                 description_embedding IS NOT NULL AND length(description_embedding) = {embedding_bytes})
            ),
            CHECK(author IS NULL OR trim(author) <> ''),
            CHECK(source IS NULL OR trim(source) <> '')
        );
        CREATE TABLE memes (
            id TEXT PRIMARY KEY,
            meme_pack_id TEXT NOT NULL REFERENCES meme_packs(id) ON DELETE CASCADE,
            name TEXT,
            name_embedding BLOB,
            description TEXT,
            description_embedding BLOB,
            CHECK(
                (name IS NULL AND name_embedding IS NULL) OR
                (name IS NOT NULL AND trim(name) <> '' AND
                 name_embedding IS NOT NULL AND length(name_embedding) = {embedding_bytes})
            ),
            CHECK(
                (description IS NULL AND description_embedding IS NULL) OR
                (description IS NOT NULL AND trim(description) <> '' AND
                 description_embedding IS NOT NULL AND length(description_embedding) = {embedding_bytes})
            )
        );
        CREATE INDEX memes_meme_pack ON memes(meme_pack_id);
        CREATE TABLE meme_contents (
            id TEXT PRIMARY KEY,
            meme_id TEXT NOT NULL REFERENCES memes(id) ON DELETE CASCADE,
            position INTEGER NOT NULL CHECK(position >= 0),
            kind TEXT NOT NULL CHECK(kind IN ('image', 'motion', 'text')),
            text TEXT,
            relative_path TEXT UNIQUE,
            preview_relative_path TEXT UNIQUE,
            width INTEGER,
            height INTEGER,
            byte_size INTEGER,
            image_format TEXT CHECK(image_format IN ('png', 'jpeg', 'webp', 'gif')),
            motion_format TEXT CHECK(motion_format IN ('mp4', 'webm', 'tgs')),
            content_hash BLOB NOT NULL CHECK(length(content_hash) = {CONTENT_HASH_BYTES}),
            embedding BLOB CHECK(embedding IS NULL OR length(embedding) = {embedding_bytes}),
            UNIQUE(meme_id, position),
            CHECK(
                (kind = 'text' AND text IS NOT NULL AND trim(text) <> '' AND
                 relative_path IS NULL AND width IS NULL AND height IS NULL AND
                 byte_size IS NULL AND image_format IS NULL AND motion_format IS NULL AND
                 preview_relative_path IS NULL AND embedding IS NOT NULL) OR
                (kind = 'image' AND text IS NULL AND relative_path IS NOT NULL AND
                 width > 0 AND height > 0 AND byte_size > 0 AND image_format IS NOT NULL AND
                 motion_format IS NULL AND preview_relative_path IS NULL AND embedding IS NOT NULL) OR
                (kind = 'motion' AND text IS NULL AND relative_path IS NOT NULL AND
                 width > 0 AND height > 0 AND byte_size > 0 AND image_format IS NULL AND
                 motion_format IS NOT NULL AND
                 ((preview_relative_path IS NULL AND embedding IS NULL) OR
                  (preview_relative_path IS NOT NULL AND embedding IS NOT NULL)))
            )
        );
        CREATE INDEX meme_contents_meme ON meme_contents(meme_id, position);
        CREATE INDEX meme_contents_kind_hash ON meme_contents(kind, content_hash);
        CREATE TABLE collector_items (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK(kind IN ('image', 'motion', 'text')),
            text TEXT,
            relative_path TEXT UNIQUE,
            preview_relative_path TEXT UNIQUE,
            width INTEGER,
            height INTEGER,
            byte_size INTEGER,
            image_format TEXT CHECK(image_format IN ('png', 'jpeg', 'webp', 'gif')),
            motion_format TEXT CHECK(motion_format IN ('mp4', 'webm', 'tgs')),
            content_hash BLOB NOT NULL CHECK(length(content_hash) = {CONTENT_HASH_BYTES}),
            embedding BLOB CHECK(embedding IS NULL OR length(embedding) = {embedding_bytes}),
            duplicate_kind TEXT CHECK(duplicate_kind IN ('hash', 'similarity')),
            duplicate_target_source TEXT CHECK(duplicate_target_source IN ('collector', 'meme')),
            duplicate_target_id TEXT,
            duplicate_distance REAL,
            duplicate_dismissed INTEGER NOT NULL DEFAULT 0 CHECK(duplicate_dismissed IN (0, 1)),
            CHECK(
                (kind = 'text' AND text IS NOT NULL AND trim(text) <> '' AND
                 relative_path IS NULL AND width IS NULL AND height IS NULL AND
                 byte_size IS NULL AND image_format IS NULL AND motion_format IS NULL AND
                 preview_relative_path IS NULL AND embedding IS NOT NULL) OR
                (kind = 'image' AND text IS NULL AND relative_path IS NOT NULL AND
                 width > 0 AND height > 0 AND byte_size > 0 AND image_format IS NOT NULL AND
                 motion_format IS NULL AND preview_relative_path IS NULL AND embedding IS NOT NULL) OR
                (kind = 'motion' AND text IS NULL AND relative_path IS NOT NULL AND
                 width > 0 AND height > 0 AND byte_size > 0 AND image_format IS NULL AND
                 motion_format IS NOT NULL AND
                 ((preview_relative_path IS NULL AND embedding IS NULL) OR
                  (preview_relative_path IS NOT NULL AND embedding IS NOT NULL)))
            ),
            CHECK(
                (duplicate_kind IS NULL AND duplicate_target_source IS NULL AND
                 duplicate_target_id IS NULL AND duplicate_distance IS NULL) OR
                (duplicate_kind = 'hash' AND duplicate_target_source IS NOT NULL AND
                 duplicate_target_id IS NOT NULL AND duplicate_distance IS NULL) OR
                (duplicate_kind = 'similarity' AND duplicate_target_source IS NOT NULL AND
                 duplicate_target_id IS NOT NULL AND duplicate_distance BETWEEN 0.0 AND 2.0)
            ),
            CHECK(duplicate_dismissed = 0 OR duplicate_kind IS NULL)
        );
        CREATE INDEX collector_items_kind_hash ON collector_items(kind, content_hash);
        CREATE TABLE tags (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL CHECK(trim(name) <> ''),
            normalized_name TEXT NOT NULL UNIQUE CHECK(trim(normalized_name) <> ''),
            name_embedding BLOB NOT NULL CHECK(length(name_embedding) = {embedding_bytes})
        );
        CREATE TABLE meme_pack_tags (
            meme_pack_id TEXT NOT NULL REFERENCES meme_packs(id) ON DELETE CASCADE,
            tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY(meme_pack_id, tag_id)
        );
        CREATE INDEX meme_pack_tags_tag ON meme_pack_tags(tag_id);
        CREATE TABLE meme_tags (
            meme_id TEXT NOT NULL REFERENCES memes(id) ON DELETE CASCADE,
            tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY(meme_id, tag_id)
        );
        CREATE INDEX meme_tags_tag ON meme_tags(tag_id);"
    ))?;
    transaction.execute(
        "INSERT INTO metadata(singleton, schema_version, embedding_model_id, embedding_dimension)
         VALUES (1, ?1, ?2, ?3)",
        params![
            SCHEMA_VERSION,
            model_id,
            usize_to_i64(embedding_dimension, "embedding dimension")?
        ],
    )?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn validate_database_metadata(
    connection: &Connection,
    model_id: &str,
    embedding_dimension: usize,
) -> Result<()> {
    let metadata = connection
        .query_row(
            "SELECT schema_version, embedding_model_id, embedding_dimension
             FROM metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::InvalidDatabase("metadata row is missing".to_owned()))?;
    if metadata.0 != SCHEMA_VERSION {
        return Err(Error::UnsupportedSchemaVersion {
            expected: SCHEMA_VERSION,
            actual: metadata.0,
        });
    }
    let stored_dimension = usize::try_from(metadata.2).map_err(|_| {
        Error::InvalidDatabase(format!(
            "stored embedding dimension {} is invalid",
            metadata.2
        ))
    })?;
    if metadata.1 != model_id || stored_dimension != embedding_dimension {
        return Err(Error::IncompatibleEmbeddingSpace {
            expected_model: metadata.1,
            expected_dimension: stored_dimension,
            actual_model: model_id.to_owned(),
            actual_dimension: embedding_dimension,
        });
    }
    Ok(())
}

#[derive(Debug)]
struct AnalyzedImageSource {
    source_path: PathBuf,
    width: u32,
    height: u32,
    byte_size: u64,
    format: ImageFormat,
    content_hash: Vec<u8>,
    embedding: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct DuplicateReference {
    source: CollectorDuplicateSource,
    content_id: Uuid,
}

#[derive(Debug)]
struct DuplicateCandidate {
    target: DuplicateReference,
    embedding: Vec<u8>,
}

#[derive(Debug)]
enum DetectedDuplicate {
    Hash {
        target: DuplicateReference,
    },
    Similarity {
        target: DuplicateReference,
        cosine_distance: f32,
    },
}

#[derive(Debug)]
struct PreparedTag {
    name: String,
    normalized_name: String,
    embedding: Vec<u8>,
}

#[derive(Debug)]
enum PreparedContent {
    Image {
        id: Uuid,
        source_path: PathBuf,
        relative_path: PathBuf,
        final_path: PathBuf,
        width: u32,
        height: u32,
        byte_size: u64,
        format: ImageFormat,
        content_hash: Vec<u8>,
        embedding: Vec<u8>,
    },
    Motion {
        id: Uuid,
        source_path: PathBuf,
        relative_path: PathBuf,
        final_path: PathBuf,
        preview_source_path: Option<PathBuf>,
        preview_relative_path: Option<PathBuf>,
        preview_final_path: Option<PathBuf>,
        width: u32,
        height: u32,
        byte_size: u64,
        format: MotionFormat,
        content_hash: Vec<u8>,
        embedding: Option<Vec<u8>>,
    },
    Text {
        id: Uuid,
        text: String,
        content_hash: Vec<u8>,
        embedding: Vec<u8>,
    },
}

impl PreparedContent {
    fn id(&self) -> Uuid {
        match self {
            Self::Image { id, .. } | Self::Motion { id, .. } | Self::Text { id, .. } => *id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Image { .. } => "image",
            Self::Motion { .. } => "motion",
            Self::Text { .. } => "text",
        }
    }

    fn content_hash(&self) -> &[u8] {
        match self {
            Self::Image { content_hash, .. }
            | Self::Motion { content_hash, .. }
            | Self::Text { content_hash, .. } => content_hash,
        }
    }

    fn embedding(&self) -> Option<&[u8]> {
        match self {
            Self::Image { embedding, .. } | Self::Text { embedding, .. } => Some(embedding),
            Self::Motion { embedding, .. } => embedding.as_deref(),
        }
    }
}

fn insert_collector_content(
    transaction: &Transaction<'_>,
    content: &PreparedContent,
    duplicate: Option<&DetectedDuplicate>,
) -> Result<()> {
    let duplicate = DuplicateDatabaseValues::from_detected(duplicate);
    match content {
        PreparedContent::Text {
            id,
            text,
            content_hash,
            embedding,
        } => {
            transaction.execute(
                "INSERT INTO collector_items(
                    id, kind, text, content_hash, embedding, duplicate_kind,
                    duplicate_target_source, duplicate_target_id, duplicate_distance
                 ) VALUES (?1, 'text', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id.to_string(),
                    text,
                    content_hash,
                    embedding,
                    duplicate.kind,
                    duplicate.target_source,
                    duplicate.target_id,
                    duplicate.distance,
                ],
            )?;
        }
        PreparedContent::Image {
            id,
            relative_path,
            width,
            height,
            byte_size,
            format,
            content_hash,
            embedding,
            ..
        } => {
            transaction.execute(
                "INSERT INTO collector_items(
                    id, kind, relative_path, width, height, byte_size, image_format,
                    content_hash, embedding, duplicate_kind, duplicate_target_source,
                    duplicate_target_id, duplicate_distance
                 ) VALUES (?1, 'image', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id.to_string(),
                    path_to_database_string(relative_path)?,
                    i64::from(*width),
                    i64::from(*height),
                    u64_to_i64(*byte_size, "image byte size")?,
                    format.as_database_str(),
                    content_hash,
                    embedding,
                    duplicate.kind,
                    duplicate.target_source,
                    duplicate.target_id,
                    duplicate.distance,
                ],
            )?;
        }
        PreparedContent::Motion {
            id,
            relative_path,
            preview_relative_path,
            width,
            height,
            byte_size,
            format,
            content_hash,
            embedding,
            ..
        } => {
            transaction.execute(
                "INSERT INTO collector_items(
                    id, kind, relative_path, preview_relative_path, width, height, byte_size,
                    motion_format, content_hash, embedding, duplicate_kind,
                    duplicate_target_source, duplicate_target_id, duplicate_distance
                 ) VALUES (?1, 'motion', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    id.to_string(),
                    path_to_database_string(relative_path)?,
                    preview_relative_path
                        .as_deref()
                        .map(path_to_database_string)
                        .transpose()?,
                    i64::from(*width),
                    i64::from(*height),
                    u64_to_i64(*byte_size, "motion byte size")?,
                    format.as_database_str(),
                    content_hash,
                    embedding,
                    duplicate.kind,
                    duplicate.target_source,
                    duplicate.target_id,
                    duplicate.distance,
                ],
            )?;
        }
    }
    Ok(())
}

struct DuplicateDatabaseValues {
    kind: Option<&'static str>,
    target_source: Option<&'static str>,
    target_id: Option<String>,
    distance: Option<f64>,
}

impl DuplicateDatabaseValues {
    fn from_detected(duplicate: Option<&DetectedDuplicate>) -> Self {
        match duplicate {
            None => Self {
                kind: None,
                target_source: None,
                target_id: None,
                distance: None,
            },
            Some(DetectedDuplicate::Hash { target }) => Self {
                kind: Some("hash"),
                target_source: Some(duplicate_source_to_database_str(target.source)),
                target_id: Some(target.content_id.to_string()),
                distance: None,
            },
            Some(DetectedDuplicate::Similarity {
                target,
                cosine_distance,
            }) => Self {
                kind: Some("similarity"),
                target_source: Some(duplicate_source_to_database_str(target.source)),
                target_id: Some(target.content_id.to_string()),
                distance: Some(f64::from(*cosine_distance)),
            },
        }
    }
}

fn update_collector_duplicate(
    connection: &Connection,
    item_id: Uuid,
    duplicate: Option<&DetectedDuplicate>,
    dismissed: bool,
) -> Result<()> {
    let duplicate = DuplicateDatabaseValues::from_detected(duplicate);
    let changed = connection.execute(
        "UPDATE collector_items
         SET duplicate_kind = ?2, duplicate_target_source = ?3,
             duplicate_target_id = ?4, duplicate_distance = ?5,
             duplicate_dismissed = ?6
         WHERE id = ?1",
        params![
            item_id.to_string(),
            duplicate.kind,
            duplicate.target_source,
            duplicate.target_id,
            duplicate.distance,
            if dismissed { 1_i64 } else { 0_i64 },
        ],
    )?;
    if changed != 1 {
        return Err(Error::CollectorItemNotFound(item_id));
    }
    Ok(())
}

fn duplicate_source_to_database_str(source: CollectorDuplicateSource) -> &'static str {
    match source {
        CollectorDuplicateSource::Collector => "collector",
        CollectorDuplicateSource::Meme => "meme",
    }
}

fn attach_prepared_tags(
    connection: &Connection,
    meme_id: Uuid,
    tags: &[PreparedTag],
) -> Result<()> {
    for tag in tags {
        let existing = connection
            .query_row(
                "SELECT id FROM tags WHERE normalized_name = ?1",
                [&tag.normalized_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let tag_id = match existing {
            Some(raw) => parse_database_uuid(&raw, "Tag ID")?,
            None => {
                let id = Uuid::new_v4();
                connection.execute(
                    "INSERT INTO tags(id, name, normalized_name, name_embedding)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id.to_string(), tag.name, tag.normalized_name, tag.embedding,],
                )?;
                id
            }
        };
        connection.execute(
            "INSERT INTO meme_tags(meme_id, tag_id) VALUES (?1, ?2)",
            params![meme_id.to_string(), tag_id.to_string()],
        )?;
    }
    Ok(())
}

fn insert_contents(
    transaction: &Transaction<'_>,
    meme_id: Uuid,
    contents: &[PreparedContent],
) -> Result<()> {
    for (position, content) in contents.iter().enumerate() {
        let position = usize_to_i64(position, "Meme content position")?;
        match content {
            PreparedContent::Text {
                id,
                text,
                content_hash,
                embedding,
            } => {
                transaction.execute(
                    "INSERT INTO meme_contents(
                        id, meme_id, position, kind, text, content_hash, embedding
                     ) VALUES (?1, ?2, ?3, 'text', ?4, ?5, ?6)",
                    params![
                        id.to_string(),
                        meme_id.to_string(),
                        position,
                        text,
                        content_hash,
                        embedding,
                    ],
                )?;
            }
            PreparedContent::Image {
                id,
                relative_path,
                width,
                height,
                byte_size,
                format,
                content_hash,
                embedding,
                ..
            } => {
                let relative_path = path_to_database_string(relative_path)?;
                transaction.execute(
                    "INSERT INTO meme_contents(
                        id, meme_id, position, kind, relative_path, width, height,
                        byte_size, image_format, content_hash, embedding
                     ) VALUES (?1, ?2, ?3, 'image', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id.to_string(),
                        meme_id.to_string(),
                        position,
                        relative_path,
                        i64::from(*width),
                        i64::from(*height),
                        u64_to_i64(*byte_size, "image byte size")?,
                        format.as_database_str(),
                        content_hash,
                        embedding,
                    ],
                )?;
            }
            PreparedContent::Motion {
                id,
                relative_path,
                preview_relative_path,
                width,
                height,
                byte_size,
                format,
                content_hash,
                embedding,
                ..
            } => {
                transaction.execute(
                    "INSERT INTO meme_contents(
                        id, meme_id, position, kind, relative_path, preview_relative_path,
                        width, height, byte_size, motion_format, content_hash, embedding
                     ) VALUES (?1, ?2, ?3, 'motion', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        id.to_string(),
                        meme_id.to_string(),
                        position,
                        path_to_database_string(relative_path)?,
                        preview_relative_path
                            .as_deref()
                            .map(path_to_database_string)
                            .transpose()?,
                        i64::from(*width),
                        i64::from(*height),
                        u64_to_i64(*byte_size, "motion byte size")?,
                        format.as_database_str(),
                        content_hash,
                        embedding,
                    ],
                )?;
            }
        }
    }
    Ok(())
}

struct PendingFile {
    staged: PathBuf,
    final_path: PathBuf,
    promoted: bool,
}

struct PendingFiles {
    files: Vec<PendingFile>,
    active: bool,
}

impl PendingFiles {
    fn stage(staging_directory: &Path, contents: &[PreparedContent]) -> Result<Self> {
        let mut pending = Self {
            files: Vec::new(),
            active: true,
        };
        for content in contents {
            match content {
                PreparedContent::Image {
                    id,
                    source_path,
                    final_path,
                    byte_size,
                    content_hash,
                    ..
                }
                | PreparedContent::Motion {
                    id,
                    source_path,
                    final_path,
                    byte_size,
                    content_hash,
                    ..
                } => {
                    pending.stage_file(
                        staging_directory,
                        *id,
                        "media",
                        source_path,
                        final_path,
                        Some((content_hash, *byte_size)),
                    )?;
                    if let PreparedContent::Motion {
                        preview_source_path: Some(preview_source_path),
                        preview_final_path: Some(preview_final_path),
                        ..
                    } = content
                    {
                        pending.stage_file(
                            staging_directory,
                            *id,
                            "preview",
                            preview_source_path,
                            preview_final_path,
                            None,
                        )?;
                    }
                }
                PreparedContent::Text { .. } => {}
            }
        }
        Ok(pending)
    }

    fn stage_file(
        &mut self,
        staging_directory: &Path,
        id: Uuid,
        suffix: &str,
        source_path: &Path,
        final_path: &Path,
        integrity: Option<(&[u8], u64)>,
    ) -> Result<()> {
        let staged = staging_directory.join(format!("{id}.{suffix}.tmp"));
        self.files.push(PendingFile {
            staged: staged.clone(),
            final_path: final_path.to_path_buf(),
            promoted: false,
        });
        let mut source = File::open(source_path)?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
        if let Some((content_hash, byte_size)) = integrity
            && (sha256_file(&staged)? != content_hash || fs::metadata(&staged)?.len() != byte_size)
        {
            return Err(Error::SourceMediaChanged(source_path.to_path_buf()));
        }
        Ok(())
    }

    fn promote(&mut self) -> Result<()> {
        for file in &mut self.files {
            if file.final_path.exists() {
                return Err(Error::InvalidDatabase(format!(
                    "managed media destination already exists: {}",
                    file.final_path.display()
                )));
            }
            fs::rename(&file.staged, &file.final_path)?;
            file.promoted = true;
        }
        Ok(())
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PendingFiles {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        for file in &self.files {
            let path = if file.promoted {
                &file.final_path
            } else {
                &file.staged
            };
            let _ = fs::remove_file(path);
        }
    }
}

struct RawCollectorItem {
    id: Uuid,
    kind: String,
    text: Option<String>,
    relative_path: Option<String>,
    preview_relative_path: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    byte_size: Option<i64>,
    image_format: Option<String>,
    motion_format: Option<String>,
    content_hash: Vec<u8>,
    embedding: Option<Vec<u8>>,
    duplicate_kind: Option<String>,
    duplicate_target_source: Option<String>,
    duplicate_target_id: Option<String>,
    duplicate_distance: Option<f64>,
    duplicate_dismissed: bool,
}

fn map_raw_collector_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawCollectorItem> {
    Ok(RawCollectorItem {
        id: uuid_from_column(row, 0)?,
        kind: row.get(1)?,
        text: row.get(2)?,
        relative_path: row.get(3)?,
        preview_relative_path: row.get(4)?,
        width: row.get(5)?,
        height: row.get(6)?,
        byte_size: row.get(7)?,
        image_format: row.get(8)?,
        motion_format: row.get(9)?,
        content_hash: row.get(10)?,
        embedding: row.get(11)?,
        duplicate_kind: row.get(12)?,
        duplicate_target_source: row.get(13)?,
        duplicate_target_id: row.get(14)?,
        duplicate_distance: row.get(15)?,
        duplicate_dismissed: row.get(16)?,
    })
}

impl RawCollectorItem {
    fn into_domain(self, connection: &Connection, storage_root: &Path) -> Result<CollectorItem> {
        let id = self.id;
        let content = RawContent {
            id,
            kind: self.kind,
            text: self.text,
            relative_path: self.relative_path,
            preview_relative_path: self.preview_relative_path,
            width: self.width,
            height: self.height,
            byte_size: self.byte_size,
            image_format: self.image_format,
            motion_format: self.motion_format,
        }
        .into_domain(storage_root)?;
        let content = match content {
            MemeContent::Image(image) => CollectorContent::Image {
                relative_path: image.relative_path,
                width: image.width,
                height: image.height,
                byte_size: image.byte_size,
                format: image.format,
            },
            MemeContent::Motion(motion) => CollectorContent::Motion {
                relative_path: motion.relative_path,
                preview_relative_path: motion.preview_relative_path,
                width: motion.width,
                height: motion.height,
                byte_size: motion.byte_size,
                format: motion.format,
            },
            MemeContent::Text(text) => CollectorContent::Text { text: text.text },
        };
        let duplicate = collector_duplicate_from_raw(
            connection,
            id,
            self.duplicate_kind,
            self.duplicate_target_source,
            self.duplicate_target_id,
            self.duplicate_distance,
        )?;
        Ok(CollectorItem {
            id,
            content,
            duplicate,
        })
    }
}

fn collector_item_into_meme_content(item: CollectorItem) -> MemeContent {
    match item.content {
        CollectorContent::Image {
            relative_path,
            width,
            height,
            byte_size,
            format,
        } => MemeContent::Image(MemeImage {
            id: item.id,
            relative_path,
            width,
            height,
            byte_size,
            format,
        }),
        CollectorContent::Motion {
            relative_path,
            preview_relative_path,
            width,
            height,
            byte_size,
            format,
        } => MemeContent::Motion(MemeMotion {
            id: item.id,
            relative_path,
            preview_relative_path,
            width,
            height,
            byte_size,
            format,
        }),
        CollectorContent::Text { text } => MemeContent::Text(MemeText { id: item.id, text }),
    }
}

fn collector_duplicate_from_raw(
    connection: &Connection,
    item_id: Uuid,
    kind: Option<String>,
    target_source: Option<String>,
    target_id: Option<String>,
    distance: Option<f64>,
) -> Result<Option<CollectorDuplicate>> {
    let Some(kind) = kind else {
        if target_source.is_some() || target_id.is_some() || distance.is_some() {
            return Err(Error::InvalidDatabase(format!(
                "Collector item {item_id} has duplicate details without a duplicate kind"
            )));
        }
        return Ok(None);
    };
    let source_raw = target_source.ok_or_else(|| {
        Error::InvalidDatabase(format!(
            "Collector item {item_id} has duplicate kind `{kind}` without a target source"
        ))
    })?;
    let source = match source_raw.as_str() {
        "collector" => CollectorDuplicateSource::Collector,
        "meme" => CollectorDuplicateSource::Meme,
        _ => {
            return Err(Error::InvalidDatabase(format!(
                "Collector item {item_id} has unknown duplicate target source `{source_raw}`"
            )));
        }
    };
    let target_id_raw = target_id.ok_or_else(|| {
        Error::InvalidDatabase(format!(
            "Collector item {item_id} has duplicate kind `{kind}` without a target ID"
        ))
    })?;
    let content_id = parse_database_uuid(&target_id_raw, "Collector duplicate target")?;
    let (meme_id, meme_name) = match source {
        CollectorDuplicateSource::Collector => (None, None),
        CollectorDuplicateSource::Meme => connection
            .query_row(
                "SELECT c.meme_id, m.name
                 FROM meme_contents c
                 JOIN memes m ON m.id = c.meme_id
                 WHERE c.id = ?1",
                [content_id.to_string()],
                |row| Ok((uuid_from_column(row, 0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
            .map_or((None, None), |(meme_id, meme_name)| {
                (Some(meme_id), meme_name)
            }),
    };
    let target = CollectorDuplicateTarget {
        source,
        content_id,
        meme_id,
        meme_name,
    };
    match kind.as_str() {
        "hash" => {
            if distance.is_some() {
                return Err(Error::InvalidDatabase(format!(
                    "Collector item {item_id} has a hash duplicate with a cosine distance"
                )));
            }
            Ok(Some(CollectorDuplicate::Hash { target }))
        }
        "similarity" => {
            let distance = distance.ok_or_else(|| {
                Error::InvalidDatabase(format!(
                    "Collector item {item_id} has a similarity duplicate without a distance"
                ))
            })?;
            if !distance.is_finite() || !(0.0..=2.0).contains(&distance) {
                return Err(Error::InvalidDatabase(format!(
                    "Collector item {item_id} has invalid cosine distance {distance}"
                )));
            }
            Ok(Some(CollectorDuplicate::Similarity {
                target,
                cosine_distance: distance as f32,
            }))
        }
        _ => Err(Error::InvalidDatabase(format!(
            "Collector item {item_id} has unknown duplicate kind `{kind}`"
        ))),
    }
}

struct RawContent {
    id: Uuid,
    kind: String,
    text: Option<String>,
    relative_path: Option<String>,
    preview_relative_path: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    byte_size: Option<i64>,
    image_format: Option<String>,
    motion_format: Option<String>,
}

impl RawContent {
    fn into_domain(self, storage_root: &Path) -> Result<MemeContent> {
        match self.kind.as_str() {
            "text" => {
                let text = self.text.ok_or_else(|| {
                    Error::InvalidDatabase(format!("text content {} has no text", self.id))
                })?;
                if self.relative_path.is_some()
                    || self.preview_relative_path.is_some()
                    || self.width.is_some()
                    || self.height.is_some()
                    || self.byte_size.is_some()
                    || self.image_format.is_some()
                    || self.motion_format.is_some()
                {
                    return Err(Error::InvalidDatabase(format!(
                        "text content {} contains image metadata",
                        self.id
                    )));
                }
                Ok(MemeContent::Text(MemeText { id: self.id, text }))
            }
            "image" => {
                if self.text.is_some() {
                    return Err(Error::InvalidDatabase(format!(
                        "image content {} contains text",
                        self.id
                    )));
                }
                let relative_path = self.relative_path.ok_or_else(|| {
                    Error::InvalidDatabase(format!("image content {} has no path", self.id))
                })?;
                let relative_path = PathBuf::from(relative_path);
                let absolute_path = resolve_media_path(storage_root, &relative_path)?;
                if !absolute_path.is_file() {
                    return Err(Error::MissingMedia(absolute_path));
                }
                let width = positive_u32(self.width, self.id, "width")?;
                let height = positive_u32(self.height, self.id, "height")?;
                let byte_size = positive_u64(self.byte_size, self.id, "byte size")?;
                let format_raw = self.image_format.ok_or_else(|| {
                    Error::InvalidDatabase(format!("image content {} has no format", self.id))
                })?;
                let format = ImageFormat::from_database_str(&format_raw).ok_or_else(|| {
                    Error::InvalidDatabase(format!(
                        "image content {} has unknown format `{format_raw}`",
                        self.id
                    ))
                })?;
                Ok(MemeContent::Image(MemeImage {
                    id: self.id,
                    relative_path,
                    width,
                    height,
                    byte_size,
                    format,
                }))
            }
            "motion" => {
                if self.text.is_some() || self.image_format.is_some() {
                    return Err(Error::InvalidDatabase(format!(
                        "motion content {} contains incompatible metadata",
                        self.id
                    )));
                }
                let relative_path = self.relative_path.ok_or_else(|| {
                    Error::InvalidDatabase(format!("motion content {} has no path", self.id))
                })?;
                let relative_path = PathBuf::from(relative_path);
                let absolute_path = resolve_media_path(storage_root, &relative_path)?;
                if !absolute_path.is_file() {
                    return Err(Error::MissingMedia(absolute_path));
                }
                let preview_relative_path = self.preview_relative_path.map(PathBuf::from);
                if let Some(preview_relative_path) = &preview_relative_path {
                    let preview_path = resolve_media_path(storage_root, preview_relative_path)?;
                    if !preview_path.is_file() {
                        return Err(Error::MissingMedia(preview_path));
                    }
                }
                let width = positive_u32(self.width, self.id, "width")?;
                let height = positive_u32(self.height, self.id, "height")?;
                let byte_size = positive_u64(self.byte_size, self.id, "byte size")?;
                let format_raw = self.motion_format.ok_or_else(|| {
                    Error::InvalidDatabase(format!("motion content {} has no format", self.id))
                })?;
                let format = MotionFormat::from_database_str(&format_raw).ok_or_else(|| {
                    Error::InvalidDatabase(format!(
                        "motion content {} has unknown format `{format_raw}`",
                        self.id
                    ))
                })?;
                Ok(MemeContent::Motion(MemeMotion {
                    id: self.id,
                    relative_path,
                    preview_relative_path,
                    width,
                    height,
                    byte_size,
                    format,
                }))
            }
            kind => Err(Error::InvalidDatabase(format!(
                "content {} has unknown kind `{kind}`",
                self.id
            ))),
        }
    }
}

fn validate_meme_pack_input(input: NewMemePack) -> Result<NewMemePack> {
    Ok(NewMemePack {
        name: required_text(input.name, "MemePack name")?,
        description: optional_text(input.description, "MemePack description")?,
        author: optional_text(input.author, "MemePack author")?,
        source: optional_text(input.source, "MemePack source")?,
    })
}

fn validate_update_meme_pack_input(input: UpdateMemePack) -> Result<UpdateMemePack> {
    Ok(UpdateMemePack {
        name: required_text(input.name, "MemePack name")?,
        description: optional_text(input.description, "MemePack description")?,
        author: optional_text(input.author, "MemePack author")?,
        source: optional_text(input.source, "MemePack source")?,
    })
}

fn validate_meme_metadata(
    name: Option<String>,
    description: Option<String>,
) -> Result<(Option<String>, Option<String>)> {
    Ok((
        optional_text(name, "Meme name")?,
        optional_text(description, "Meme description")?,
    ))
}

fn required_text(value: String, field: &'static str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::EmptyField { field });
    }
    Ok(trimmed.to_owned())
}

fn optional_text(value: Option<String>, field: &'static str) -> Result<Option<String>> {
    value.map(|value| required_text(value, field)).transpose()
}

fn normalize_tag_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn sha256_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(hasher.finalize().to_vec())
}

fn encode_embedding(
    field: &'static str,
    values: Vec<f32>,
    expected_dimension: usize,
) -> Result<Vec<u8>> {
    if values.len() != expected_dimension {
        return Err(Error::InvalidEmbeddingDimension {
            field,
            expected: expected_dimension,
            actual: values.len(),
        });
    }
    if let Some((index, _)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(Error::NonFiniteEmbedding { field, index });
    }
    let squared_norm = values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    if !squared_norm.is_finite() || squared_norm <= f64::EPSILON {
        return Err(Error::ZeroEmbedding { field });
    }
    let mut encoded = Vec::with_capacity(expected_dimension * size_of::<f32>());
    for value in values {
        encoded.extend_from_slice(&value.to_le_bytes());
    }
    Ok(encoded)
}

fn cosine_distance_between_encoded_embeddings(
    left: &[u8],
    right: &[u8],
    expected_dimension: usize,
) -> Result<f32> {
    let left = decode_embedding("query image", left, expected_dimension)?;
    let right = decode_embedding("stored image", right, expected_dimension)?;
    let mut dot = 0.0_f64;
    let mut left_squared_norm = 0.0_f64;
    let mut right_squared_norm = 0.0_f64;
    for (left, right) in left.iter().zip(&right) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_squared_norm += left * left;
        right_squared_norm += right * right;
    }
    if left_squared_norm <= f64::EPSILON || right_squared_norm <= f64::EPSILON {
        return Err(Error::InvalidDatabase(
            "image embedding has zero L2 norm".to_owned(),
        ));
    }
    let similarity =
        (dot / (left_squared_norm.sqrt() * right_squared_norm.sqrt())).clamp(-1.0, 1.0);
    Ok((1.0 - similarity) as f32)
}

fn cosine_similarity_between_encoded_embeddings(
    left: &[u8],
    right: &[u8],
    expected_dimension: usize,
) -> Result<f32> {
    Ok(1.0 - cosine_distance_between_encoded_embeddings(left, right, expected_dimension)?)
}

fn decode_embedding(
    field: &'static str,
    encoded: &[u8],
    expected_dimension: usize,
) -> Result<Vec<f32>> {
    let expected_bytes = expected_dimension
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| Error::InvalidDatabase("embedding byte length exceeds usize".to_owned()))?;
    if encoded.len() != expected_bytes {
        return Err(Error::InvalidDatabase(format!(
            "{field} embedding has {} bytes; expected {expected_bytes}",
            encoded.len()
        )));
    }
    encoded
        .chunks_exact(size_of::<f32>())
        .enumerate()
        .map(|(index, bytes)| {
            let bytes: [u8; size_of::<f32>()] = bytes.try_into().map_err(|_| {
                Error::InvalidDatabase(format!("{field} embedding value {index} is truncated"))
            })?;
            let value = f32::from_le_bytes(bytes);
            if !value.is_finite() {
                return Err(Error::InvalidDatabase(format!(
                    "{field} embedding contains a non-finite value at index {index}"
                )));
            }
            Ok(value)
        })
        .collect()
}

fn decode_supported_image(path: &Path) -> Result<(ImageFormat, DynamicImage)> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let detected = reader
        .format()
        .ok_or_else(|| Error::UnsupportedImageFormat(path.to_path_buf()))?;
    let format = match detected {
        DecodedImageFormat::Png => ImageFormat::Png,
        DecodedImageFormat::Jpeg => ImageFormat::Jpeg,
        DecodedImageFormat::WebP => ImageFormat::WebP,
        DecodedImageFormat::Gif => ImageFormat::Gif,
        _ => return Err(Error::UnsupportedImageFormat(path.to_path_buf())),
    };
    Ok((format, reader.decode()?))
}

fn resolve_media_path(storage_root: &Path, relative_path: &Path) -> Result<PathBuf> {
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::UnsafeMediaPath(relative_path.to_path_buf()));
    }
    let parent = relative_path
        .parent()
        .ok_or_else(|| Error::UnsafeMediaPath(relative_path.to_path_buf()))?;
    if parent != Path::new(MEDIA_DIRECTORY) || relative_path.file_name().is_none() {
        return Err(Error::UnsafeMediaPath(relative_path.to_path_buf()));
    }
    Ok(storage_root.join(relative_path))
}

fn path_to_database_string(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| Error::UnsafeMediaPath(path.to_path_buf()))?,
            ),
            _ => return Err(Error::UnsafeMediaPath(path.to_path_buf())),
        }
    }
    Ok(parts.join("/"))
}

fn ensure_exists(connection: &Connection, table: &str, id: Uuid) -> Result<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?1)");
    connection
        .query_row(&sql, [id.to_string()], |row| row.get(0))
        .map_err(Error::from)
}

fn insert_tag_association(
    connection: &Connection,
    table: &'static str,
    owner_column: &'static str,
    target: &'static str,
    target_id: Uuid,
    tag_id: Uuid,
) -> Result<()> {
    let exists_sql = format!(
        "SELECT EXISTS(
            SELECT 1 FROM {table} WHERE {owner_column} = ?1 AND tag_id = ?2
         )"
    );
    let exists: bool = connection.query_row(
        &exists_sql,
        params![target_id.to_string(), tag_id.to_string()],
        |row| row.get(0),
    )?;
    if exists {
        return Err(Error::TagAssociationExists {
            target,
            target_id,
            tag_id,
        });
    }
    let insert_sql = format!("INSERT INTO {table}({owner_column}, tag_id) VALUES (?1, ?2)");
    connection.execute(
        &insert_sql,
        params![target_id.to_string(), tag_id.to_string()],
    )?;
    Ok(())
}

fn delete_tag_association(
    connection: &Connection,
    table: &'static str,
    owner_column: &'static str,
    target: &'static str,
    target_id: Uuid,
    tag_id: Uuid,
) -> Result<()> {
    let sql = format!("DELETE FROM {table} WHERE {owner_column} = ?1 AND tag_id = ?2");
    let changed = connection.execute(&sql, params![target_id.to_string(), tag_id.to_string()])?;
    if changed == 0 {
        return Err(Error::TagAssociationNotFound {
            target,
            target_id,
            tag_id,
        });
    }
    Ok(())
}

fn map_meme_pack(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemePack> {
    Ok(MemePack {
        id: uuid_from_column(row, 0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        author: row.get(3)?,
        source: row.get(4)?,
    })
}

fn map_tag(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tag> {
    Ok(Tag {
        id: uuid_from_column(row, 0)?,
        name: row.get(1)?,
    })
}

fn uuid_from_column(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Uuid> {
    let raw: String = row.get(column)?;
    Uuid::parse_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_database_uuid(value: &str, field: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        Error::InvalidDatabase(format!("{field} contains invalid UUID `{value}`: {error}"))
    })
}

fn positive_u32(value: Option<i64>, id: Uuid, field: &str) -> Result<u32> {
    let raw = value
        .ok_or_else(|| Error::InvalidDatabase(format!("image content {id} has no {field}")))?;
    let converted = u32::try_from(raw).map_err(|_| {
        Error::InvalidDatabase(format!("image content {id} has invalid {field} {raw}"))
    })?;
    if converted == 0 {
        return Err(Error::InvalidDatabase(format!(
            "image content {id} has zero {field}"
        )));
    }
    Ok(converted)
}

fn positive_u64(value: Option<i64>, id: Uuid, field: &str) -> Result<u64> {
    let raw = value
        .ok_or_else(|| Error::InvalidDatabase(format!("image content {id} has no {field}")))?;
    let converted = u64::try_from(raw).map_err(|_| {
        Error::InvalidDatabase(format!("image content {id} has invalid {field} {raw}"))
    })?;
    if converted == 0 {
        return Err(Error::InvalidDatabase(format!(
            "image content {id} has zero {field}"
        )));
    }
    Ok(converted)
}

fn usize_to_i64(value: usize, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        Error::InvalidDatabase(format!("{field} exceeds SQLite integer range: {value}"))
    })
}

fn u64_to_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        Error::InvalidDatabase(format!("{field} exceeds SQLite integer range: {value}"))
    })
}

fn remove_managed_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        fs::remove_file(path)?;
    }
    Ok(())
}
