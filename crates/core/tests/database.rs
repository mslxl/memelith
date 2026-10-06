use std::{
    fs::{self, File},
    io,
    path::Path,
};

use image::{
    Delay, DynamicImage, Frame, GenericImageView, ImageBuffer, ImageFormat as EncodedImageFormat,
    Rgba, RgbaImage, codecs::gif::GifEncoder,
};
use memelith_core::SEMANTIC_MIN_SIMILARITY;
use memelith_core::{
    EffectiveTag, EmbeddingProvider, EmbeddingProviderError, Error, ImageFormat, ImageType,
    MemeContent, MemeDatabase, NewMeme, NewMemeContent, NewMemePack, NewTag, UpdateImageSemantics,
    UpdateMemeMetadata, UpdateMemePack,
};
use rusqlite::Connection;

#[derive(Clone)]
struct FakeEmbeddingProvider {
    model_id: String,
    dimension: usize,
    wrong_output_dimension: Option<usize>,
    fail: bool,
}

#[test]
fn migrates_actual_v1_schema_without_changing_legacy_data() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("media/images")).unwrap();
    let media = root.join("media/images/legacy.png");
    ImageBuffer::from_pixel(2, 2, Rgba([17u8, 31, 47, 255]))
        .save(&media)
        .unwrap();
    let bytes = fs::read(&media).unwrap();
    let connection = Connection::open(root.join("memelith.sqlite3")).unwrap();
    connection
        .execute_batch(include_str!("fixtures/schema_v1.sql"))
        .unwrap();
    let pack = uuid::Uuid::from_u128(1);
    let meme = uuid::Uuid::from_u128(2);
    let image = uuid::Uuid::from_u128(3);
    let text = uuid::Uuid::from_u128(4);
    let tag = uuid::Uuid::from_u128(5);
    connection
        .execute(
            "INSERT INTO meme_packs(id,name,name_embedding) VALUES (?1,'old pack',zeroblob(16))",
            [pack.to_string()],
        )
        .unwrap();
    connection.execute("INSERT INTO memes(id,meme_pack_id,name,name_embedding,description,description_embedding) VALUES (?1,?2,'old name',zeroblob(16),'old description',zeroblob(16))", rusqlite::params![meme.to_string(), pack.to_string()]).unwrap();
    connection.execute("INSERT INTO meme_contents(id,meme_id,position,kind,relative_path,width,height,byte_size,image_format,content_hash,embedding) VALUES (?1,?2,0,'image','media/images/legacy.png',2,2,?3,'png',zeroblob(32),zeroblob(16))", rusqlite::params![image.to_string(),meme.to_string(),bytes.len() as i64]).unwrap();
    connection.execute("INSERT INTO meme_contents(id,meme_id,position,kind,text,content_hash,embedding) VALUES (?1,?2,1,'text','legacy text',zeroblob(32),zeroblob(16))", rusqlite::params![text.to_string(),meme.to_string()]).unwrap();
    connection
        .execute(
            "INSERT INTO tags VALUES (?1,'kind:legacy','kind:legacy',zeroblob(16))",
            [tag.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO meme_tags VALUES (?1,?2)",
            rusqlite::params![meme.to_string(), tag.to_string()],
        )
        .unwrap();
    drop(connection);
    for _ in 0..2 {
        let database = MemeDatabase::open(root, FakeEmbeddingProvider::valid()).unwrap();
        let saved = database.get_meme(meme).unwrap();
        assert_eq!(saved.name.as_deref(), Some("old name"));
        assert_eq!(saved.description.as_deref(), Some("old description"));
        assert_eq!(saved.meme_pack_id, pack);
        assert!(
            matches!(&saved.contents[1], MemeContent::Text(t) if t.id == text && t.text == "legacy text")
        );
        let semantics = database.get_image_semantics(image).unwrap();
        assert_eq!(semantics.image_type, ImageType::Unknown);
        assert_eq!(semantics.image_type_source, "unknown");
        assert_eq!(semantics.image_review_status, "unchecked");
        assert!(semantics.caption.is_none());
        assert_eq!(
            fs::read(
                database
                    .resolve_media_path(&semantics.relative_path)
                    .unwrap()
            )
            .unwrap(),
            bytes
        );
        assert_eq!(database.list_meme_direct_tags(meme).unwrap()[0].id, tag);
        let connection = Connection::open(database.database_path()).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            6
        );
        let original_embedding: Vec<u8> = connection
            .query_row(
                "SELECT embedding FROM meme_contents WHERE id=?1",
                [image.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(original_embedding, vec![0u8; 16]);
        assert_eq!(
            connection
                .query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
            "ok"
        );
    }
}

#[test]
fn migration_rolls_back_schema_and_version_on_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("memelith.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("fixtures/schema_v1.sql"))
        .unwrap();
    connection.execute_batch("CREATE TRIGGER reject_migration BEFORE UPDATE ON metadata BEGIN SELECT RAISE(ABORT, 'migration interrupted'); END;").unwrap();
    drop(connection);
    assert!(MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).is_err());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        1
    );
    let columns = connection
        .prepare("PRAGMA table_info(meme_contents)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!columns.iter().any(|column| column == "image_type"));
    connection
        .execute_batch("DROP TRIGGER reject_migration;")
        .unwrap();
    drop(connection);
    MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
}

#[test]
fn caption_ocr_and_review_survive_embedding_failure_and_rebuild_without_vlm() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("ocr.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [11, 22, 33, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "OCR");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    drop(database);
    let mut failing_provider = FakeEmbeddingProvider::valid();
    failing_provider.fail = true;
    let mut database = MemeDatabase::open(&storage, failing_provider).unwrap();
    database
        .save_vlm_semantics(
            id,
            ImageType::Unknown,
            "automatic",
            "needs_review",
            "caption",
            &["tag".to_owned()],
            "可搜索原文",
            Some(("uncertain", "类型不明", "")),
        )
        .unwrap();
    assert!(database.rebuild_image_semantics(id).is_err());
    let item = database.get_image_semantics(id).unwrap();
    assert_eq!(item.status, "done");
    assert_eq!(item.embedding_status, "failed");
    assert!(item.embedding_error.is_some());
    assert_eq!(item.visible_text.as_deref(), Some("可搜索原文"));
    assert_eq!(item.category_fit.as_deref(), Some("uncertain"));
    assert_eq!(item.category_review_reason.as_deref(), Some("类型不明"));
    assert_eq!(database.list_images_needing_review().unwrap().len(), 1);
    assert!(
        matches!(&database.get_meme(meme.id).unwrap().contents[0], MemeContent::Image(image) if image.visible_text.as_deref() == Some("可搜索原文"))
    );
    drop(database);
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 1);
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 0);
    let item = database.get_image_semantics(id).unwrap();
    assert_eq!(item.embedding_status, "done");
    assert!(item.built_at.is_some());
    assert!(item.embedding_error.is_none());
    assert_eq!(
        database.search_vlm_semantics("caption", 0).unwrap()[0].meme_id,
        meme.id
    );
    database
        .edit_image_semantics(
            id,
            ImageType::Illustration,
            "人工含义",
            &["人工标签".to_owned()],
            "人工文字",
        )
        .unwrap();
    database
        .save_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "自动覆盖",
            &["自动标签".to_owned()],
            "错误 OCR",
            Some(("match", "", "")),
        )
        .unwrap();
    let item = database.get_image_semantics(id).unwrap();
    assert_eq!(item.provenance, "manual");
    assert_eq!(item.image_type, ImageType::Illustration);
    assert_eq!(item.caption.as_deref(), Some("人工含义"));
    assert_eq!(item.visible_text.as_deref(), Some("人工文字"));
    assert_eq!(item.embedding_status, "pending");
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 1);
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 0);
    let raw = Connection::open(database.database_path()).unwrap();
    raw.execute(
        "UPDATE meme_contents SET semantic_embedding_model='obsolete' WHERE id=?1",
        [id.to_string()],
    )
    .unwrap();
    assert!(
        database
            .search_vlm_semantics("人工含义", 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 1);
}

#[test]
fn v2_migration_preserves_captions_and_recovers_interrupted_embedding_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy-caption.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [12, 34, 56, 255]);
    let mut database =
        MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Legacy semantics");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .apply_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "旧含义",
            &["旧标签".to_owned()],
            "旧 OCR",
        )
        .unwrap();
    let path = database.database_path().to_owned();
    drop(database);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("DROP TABLE image_category_history; DROP TABLE image_semantic_state; UPDATE metadata SET schema_version=2; PRAGMA user_version=2;").unwrap();
    drop(raw);
    let database = MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
    let item = database.get_image_semantics(id).unwrap();
    assert_eq!(item.caption.as_deref(), Some("旧含义"));
    assert_eq!(item.visible_text.as_deref(), Some("旧 OCR"));
    assert_eq!(item.embedding_status, "done");
    let raw = Connection::open(database.database_path()).unwrap();
    assert_eq!(
        raw.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        6
    );
    raw.execute(
        "UPDATE image_semantic_state SET embedding_status='running' WHERE content_id=?1",
        [id.to_string()],
    )
    .unwrap();
    drop(raw);
    drop(database);
    let database = MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
    assert_eq!(
        database.get_image_semantics(id).unwrap().embedding_status,
        "pending"
    );
}

#[test]
fn v3_migration_rolls_back_and_preserves_semantics_on_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("image.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [11, 22, 33, 255]);
    let root = directory.path().join("library");
    let mut database = MemeDatabase::open(&root, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "v3");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .edit_image_semantics(
            id,
            ImageType::Sticker,
            "caption",
            &["tag".to_owned()],
            "OCR",
        )
        .unwrap();
    database.rebuild_image_semantics(id).unwrap();
    let expected = database.get_image_semantics(id).unwrap();
    let path = database.database_path().to_owned();
    drop(database);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("DROP TABLE image_category_history;
        UPDATE metadata SET schema_version=3; PRAGMA user_version=3;
        ALTER TABLE image_semantic_state ADD COLUMN requested INTEGER NOT NULL DEFAULT 0 CHECK(requested IN (0,1));
        CREATE TRIGGER reject_v4 BEFORE UPDATE ON metadata BEGIN SELECT RAISE(ABORT,'migration interrupted'); END;").unwrap();
    assert!(MemeDatabase::open(&root, FakeEmbeddingProvider::valid()).is_err());
    assert_eq!(
        raw.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        3
    );
    assert_eq!(
        raw.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='image_category_history'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    raw.execute_batch("DROP TRIGGER reject_v4;").unwrap();
    drop(raw);
    for _ in 0..2 {
        let database = MemeDatabase::open(&root, FakeEmbeddingProvider::valid()).unwrap();
        assert_eq!(database.get_image_semantics(id).unwrap(), expected);
    }
    let raw = Connection::open(&path).unwrap();
    let columns = raw
        .prepare("PRAGMA table_info(image_semantic_state)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!columns.iter().any(|column| column == "requested"));
}

#[test]
fn automatic_reclassification_keeps_bounded_history_without_moving_media() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("image.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [31, 42, 53, 255]);
    let root = directory.path().join("library");
    let mut database = MemeDatabase::open(&root, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Classification");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    let relative_path = database.get_image_semantics(id).unwrap().relative_path;
    let bytes = fs::read(database.resolve_media_path(&relative_path).unwrap()).unwrap();
    let before = database.get_image_semantics(id).unwrap();
    let raw = Connection::open(database.database_path()).unwrap();
    raw.execute_batch("CREATE TRIGGER reject_history BEFORE INSERT ON image_category_history BEGIN SELECT RAISE(ABORT,'history interrupted'); END;").unwrap();
    assert!(
        database
            .save_vlm_semantics(
                id,
                ImageType::Sticker,
                "automatic",
                "needs_review",
                "caption",
                &["tag".to_owned()],
                "OCR",
                Some(("conflict", "reason", "sticker"))
            )
            .is_err()
    );
    assert_eq!(database.get_image_semantics(id).unwrap(), before);
    raw.execute_batch("DROP TRIGGER reject_history;").unwrap();
    drop(raw);
    for index in 0..23 {
        let suggested = if index % 2 == 0 {
            "sticker"
        } else {
            "illustration"
        };
        database
            .save_vlm_semantics(
                id,
                ImageType::Unknown,
                "automatic",
                "needs_review",
                "caption",
                &["tag".to_owned()],
                "OCR",
                Some(("conflict", "reason", suggested)),
            )
            .unwrap();
        let current = database.get_image_semantics(id).unwrap();
        assert_eq!(current.category_fit.as_deref(), Some("uncertain"));
        assert_eq!(current.image_review_status, "needs_review");
        assert_eq!(current.embedding_status, "pending");
        assert_eq!(current.reclassification_history.len(), (index + 1).min(20));
        let last = current.reclassification_history.last().unwrap();
        assert_eq!(last.to_category, suggested);
        assert_eq!(last.status, "auto_reclassified");
        assert_eq!(last.reason, "reason");
        assert!(!last.at.is_empty());
    }
    database
        .save_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "caption",
            &["tag".to_owned()],
            "OCR",
            Some(("match", "", "")),
        )
        .unwrap();
    assert_eq!(
        database
            .get_image_semantics(id)
            .unwrap()
            .image_review_status,
        "needs_review"
    );
    database
        .save_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "needs_review",
            "caption",
            &["tag".to_owned()],
            "OCR",
            Some(("conflict", "no category", "invented")),
        )
        .unwrap();
    let current = database.get_image_semantics(id).unwrap();
    assert_eq!(current.image_type, ImageType::Unknown);
    assert_eq!(current.suggested_category.as_deref(), Some(""));
    assert_eq!(
        current.reclassification_history.last().unwrap().status,
        "moved_to_review"
    );
    database
        .resolve_image_review(id, ImageType::Illustration)
        .unwrap();
    database
        .save_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "needs_review",
            "caption",
            &["tag".to_owned()],
            "OCR",
            Some(("conflict", "auto", "sticker")),
        )
        .unwrap();
    let confirmed = database.get_image_semantics(id).unwrap();
    assert_eq!(confirmed.image_type, ImageType::Illustration);
    assert_eq!(confirmed.image_type_source, "manual");
    assert_eq!(confirmed.image_review_status, "confirmed");
    assert_eq!(
        confirmed.reclassification_history,
        current.reclassification_history
    );
    assert_eq!(confirmed.relative_path, relative_path);
    assert_eq!(
        fs::read(database.resolve_media_path(&relative_path).unwrap()).unwrap(),
        bytes
    );
    drop(database);
    let database = MemeDatabase::open(&root, FakeEmbeddingProvider::valid()).unwrap();
    assert_eq!(database.get_image_semantics(id).unwrap(), confirmed);
}

impl FakeEmbeddingProvider {
    fn valid() -> Self {
        Self {
            model_id: "test-embedding-v1".to_owned(),
            dimension: 4,
            wrong_output_dimension: None,
            fail: false,
        }
    }

    fn text_values(text: &str, dimension: usize) -> Vec<f32> {
        let mut values = (1..=dimension)
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        if let Some(first) = values.first_mut() {
            *first = text.len() as f32 + 1.0;
        }
        values
    }

    fn image_values(image: &DynamicImage, dimension: usize) -> Vec<f32> {
        let pixel = image.get_pixel(0, 0).0;
        let base = [
            image.width() as f32,
            image.height() as f32,
            f32::from(pixel[0]) + 1.0,
            f32::from(pixel[2]) + 1.0,
        ];
        (0..dimension)
            .map(|index| base[index % base.len()])
            .collect()
    }

    fn output_dimension(&self) -> usize {
        self.wrong_output_dimension.unwrap_or(self.dimension)
    }
}

impl EmbeddingProvider for FakeEmbeddingProvider {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed_text(&mut self, text: &str) -> std::result::Result<Vec<f32>, EmbeddingProviderError> {
        if self.fail {
            return Err(Box::new(io::Error::other("intentional provider failure")));
        }
        Ok(Self::text_values(text, self.output_dimension()))
    }

    fn embed_image(
        &mut self,
        image: &DynamicImage,
    ) -> std::result::Result<Vec<f32>, EmbeddingProviderError> {
        if self.fail {
            return Err(Box::new(io::Error::other("intentional provider failure")));
        }
        Ok(Self::image_values(image, self.output_dimension()))
    }
}

struct FixedVectorProvider {
    values: Vec<f32>,
}

#[test]
fn semantic_threshold_matches_reference_contract() {
    assert_eq!(SEMANTIC_MIN_SIMILARITY, 0.25);
}

impl EmbeddingProvider for FixedVectorProvider {
    fn model_id(&self) -> &str {
        "fixed-vector-test"
    }

    fn dimension(&self) -> usize {
        self.values.len()
    }

    fn embed_text(&mut self, _text: &str) -> std::result::Result<Vec<f32>, EmbeddingProviderError> {
        Ok(self.values.clone())
    }

    fn embed_image(
        &mut self,
        _image: &DynamicImage,
    ) -> std::result::Result<Vec<f32>, EmbeddingProviderError> {
        Ok(self.values.clone())
    }
}

#[test]
fn persists_mixed_contents_embeddings_and_full_meme_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.png");
    write_static_image(&source, EncodedImageFormat::Png, 3, 2, [220, 10, 30, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();

    let pack = database
        .create_meme_pack(NewMemePack {
            name: "  Reactions  ".to_owned(),
            description: Some("  Everyday reactions  ".to_owned()),
            author: Some("  Alice  ".to_owned()),
            source: Some("  local import  ".to_owned()),
        })
        .unwrap();
    assert_eq!(pack.name, "Reactions");
    assert_eq!(pack.description.as_deref(), Some("Everyday reactions"));
    assert_eq!(pack.author.as_deref(), Some("Alice"));
    assert_eq!(pack.source.as_deref(), Some("local import"));

    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: Some("  Greeting  ".to_owned()),
                description: Some("  A mixed Meme  ".to_owned()),
                contents: vec![
                    NewMemeContent::Text {
                        text: "  hello  ".to_owned(),
                    },
                    NewMemeContent::Image {
                        source_path: source.clone(),
                    },
                ],
            },
        )
        .unwrap();
    assert_eq!(meme.name.as_deref(), Some("Greeting"));
    assert_eq!(meme.description.as_deref(), Some("A mixed Meme"));
    assert_eq!(meme.contents.len(), 2);
    assert!(matches!(
        &meme.contents[0],
        MemeContent::Text(text) if text.text == "hello"
    ));
    let image = match &meme.contents[1] {
        MemeContent::Image(image) => image,
        other => panic!("expected image content, got {other:?}"),
    };
    assert!(!image.relative_path.is_absolute());
    assert_eq!(
        image.relative_path.parent(),
        Some(Path::new("media/images"))
    );
    assert_eq!((image.width, image.height), (3, 2));
    assert_eq!(image.format, ImageFormat::Png);
    assert_eq!(image.image_type, ImageType::Unknown);
    assert_eq!(image.visible_text, None);
    assert_eq!(image.byte_size, fs::metadata(&source).unwrap().len());
    let managed_path = database.resolve_media_path(&image.relative_path).unwrap();
    assert_eq!(fs::read(&managed_path).unwrap(), fs::read(&source).unwrap());

    let raw = Connection::open(database.database_path()).unwrap();
    let stored_pack_name: Vec<u8> = raw
        .query_row(
            "SELECT name_embedding FROM meme_packs WHERE id = ?1",
            [pack.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_embedding(&stored_pack_name),
        FakeEmbeddingProvider::text_values("Reactions", 4)
    );
    let stored_pack_description: Vec<u8> = raw
        .query_row(
            "SELECT description_embedding FROM meme_packs WHERE id = ?1",
            [pack.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_embedding(&stored_pack_description),
        FakeEmbeddingProvider::text_values("Everyday reactions", 4)
    );
    let (stored_meme_name, stored_meme_description): (Vec<u8>, Vec<u8>) = raw
        .query_row(
            "SELECT name_embedding, description_embedding FROM memes WHERE id = ?1",
            [meme.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        decode_embedding(&stored_meme_name),
        FakeEmbeddingProvider::text_values("Greeting", 4)
    );
    assert_eq!(
        decode_embedding(&stored_meme_description),
        FakeEmbeddingProvider::text_values("A mixed Meme", 4)
    );
    let stored_text: Vec<u8> = raw
        .query_row(
            "SELECT embedding FROM meme_contents WHERE kind = 'text' AND meme_id = ?1",
            [meme.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_embedding(&stored_text),
        FakeEmbeddingProvider::text_values("hello", 4)
    );
    drop(raw);

    let updated = database
        .update_meme_metadata(
            meme.id,
            UpdateMemeMetadata {
                name: Some("Updated".to_owned()),
                description: None,
            },
        )
        .unwrap();
    let unchanged_image = match &updated.contents[1] {
        MemeContent::Image(image) => image,
        other => panic!("expected image content, got {other:?}"),
    };
    assert_eq!(unchanged_image.relative_path, image.relative_path);

    let destination = database
        .create_meme_pack(NewMemePack {
            name: "Archive".to_owned(),
            description: None,
            author: None,
            source: None,
        })
        .unwrap();
    let moved = database.move_meme(meme.id, destination.id).unwrap();
    assert_eq!(moved.meme_pack_id, destination.id);
    assert!(database.list_memes(pack.id).unwrap().is_empty());
    assert_eq!(
        database.list_memes(destination.id).unwrap(),
        vec![moved.clone()]
    );
    assert_eq!(database.list_all_memes().unwrap(), vec![moved.clone()]);

    let old_managed_path = managed_path;
    let replaced = database
        .replace_meme_contents(
            meme.id,
            vec![NewMemeContent::Text {
                text: "replacement".to_owned(),
            }],
        )
        .unwrap();
    assert_eq!(replaced.contents.len(), 1);
    assert!(matches!(
        &replaced.contents[0],
        MemeContent::Text(text) if text.text == "replacement"
    ));
    assert!(!old_managed_path.exists());

    database
        .update_meme_pack(
            destination.id,
            UpdateMemePack {
                name: "Archived".to_owned(),
                description: Some("Stored".to_owned()),
                author: None,
                source: None,
            },
        )
        .unwrap();
    assert_eq!(
        database.get_meme_pack(destination.id).unwrap().name,
        "Archived"
    );
    database.delete_meme(meme.id).unwrap();
    assert!(matches!(database.get_meme(meme.id), Err(Error::MemeNotFound(id)) if id == meme.id));
    database.delete_meme_pack(destination.id).unwrap();
    assert!(matches!(
        database.get_meme_pack(destination.id),
        Err(Error::MemePackNotFound(id)) if id == destination.id
    ));
}

#[test]
fn persists_image_semantics_and_ocr_without_changing_media() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("semantic.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [8, 9, 10, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Semantics");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let image_id = meme.contents[0].id();
    database
        .update_image_semantics(
            image_id,
            UpdateImageSemantics {
                image_type: ImageType::Sticker,
                image_type_source: "manual".to_owned(),
                image_review_status: "confirmed".to_owned(),
                caption: Some("聊天中表达惊讶".to_owned()),
                semantic_tags: vec!["惊讶".to_owned()],
                visible_text: Some("啊？".to_owned()),
                status: "done".to_owned(),
                error: None,
                prompt_version: Some("test".to_owned()),
                text_hash: Some("hash".to_owned()),
                embedding_provider: None,
                embedding_model: None,
                embedding_dimension: None,
                embedding: None,
            },
        )
        .unwrap();
    let loaded = database.get_meme(meme.id).unwrap();
    let image = match &loaded.contents[0] {
        MemeContent::Image(image) => image,
        other => panic!("expected image content, got {other:?}"),
    };
    assert_eq!(image.image_type, ImageType::Sticker);
    assert_eq!(image.visible_text.as_deref(), Some("啊？"));
    let semantics = database.get_image_semantics(image_id).unwrap();
    assert_eq!(semantics.semantic_tags, vec!["惊讶"]);
    assert_eq!(semantics.image_review_status, "confirmed");
}

#[test]
fn semantic_embedding_is_independent_from_clip_embedding() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("semantic-vector.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [1, 2, 3, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Vectors");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let image_id = meme.contents[0].id();
    let raw = Connection::open(database.database_path()).unwrap();
    let clip_before: Vec<u8> = raw
        .query_row(
            "SELECT embedding FROM meme_contents WHERE id = ?1",
            [image_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    database
        .save_vlm_semantics(
            image_id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "表达惊讶",
            &["惊讶".to_owned()],
            "啊",
            None,
        )
        .unwrap();
    database
        .embed_image_semantics(
            image_id,
            "表达惊讶",
            &["惊讶".to_owned()],
            "啊",
            ImageType::Sticker,
        )
        .unwrap();
    let (clip_after, semantic, model, dimension, hash, status): (Vec<u8>, Vec<u8>, String, i64, String, String) = raw.query_row(
        "SELECT embedding, semantic_embedding, semantic_embedding_model, semantic_embedding_dimension, semantic_text_hash, semantic_status FROM meme_contents WHERE id = ?1",
        [image_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))).unwrap();
    assert_eq!(clip_before, clip_after);
    assert_eq!(semantic.len(), 16);
    assert_eq!(model, "test-embedding-v1");
    assert_eq!(dimension, 4);
    assert!(!hash.is_empty());
    assert_eq!(status, "done");
}

#[test]
fn semantic_vector_commit_is_atomic_and_batch_continues_after_one_failure() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = MemeDatabase::open(
        directory.path().join("library"),
        FakeEmbeddingProvider::valid(),
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Atomic vectors");
    let mut ids = Vec::new();
    for index in 0..2 {
        let source = directory.path().join(format!("image-{index}.png"));
        write_static_image(&source, EncodedImageFormat::Png, 2, 2, [index, 12, 34, 255]);
        let meme = database
            .create_meme(
                pack.id,
                NewMeme {
                    name: None,
                    description: None,
                    contents: vec![NewMemeContent::Image {
                        source_path: source,
                    }],
                },
            )
            .unwrap();
        let id = meme.contents[0].id();
        database
            .save_vlm_semantics(
                id,
                ImageType::Sticker,
                "automatic",
                "confirmed",
                "caption",
                &["tag".to_owned()],
                "OCR",
                None,
            )
            .unwrap();
        ids.push(id);
    }
    let bad = ids[1];
    let raw = Connection::open(database.database_path()).unwrap();
    raw.execute_batch(&format!("CREATE TRIGGER fail_vector_commit BEFORE UPDATE ON image_semantic_state WHEN NEW.content_id = '{bad}' AND NEW.embedding_status = 'done' BEGIN SELECT RAISE(ABORT, 'injected index failure'); END;")).unwrap();
    assert!(database.rebuild_pending_semantics().is_err());
    let failed = database.get_image_semantics(bad).unwrap();
    assert_eq!(failed.status, "done");
    assert_eq!(failed.embedding_status, "failed");
    assert_eq!(failed.visible_text.as_deref(), Some("OCR"));
    assert!(failed.text_hash.is_none());
    assert!(failed.built_at.is_none());
    let vector: Option<Vec<u8>> = raw
        .query_row(
            "SELECT semantic_embedding FROM meme_contents WHERE id=?1",
            [bad.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(vector.is_none());
    assert_eq!(
        database
            .get_image_semantics(ids[0])
            .unwrap()
            .embedding_status,
        "done"
    );
    raw.execute_batch("DROP TRIGGER fail_vector_commit;")
        .unwrap();
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 1);
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 0);
    assert!(
        database
            .embed_image_semantics(
                bad,
                "stale caption",
                &["tag".to_owned()],
                "OCR",
                ImageType::Sticker
            )
            .is_err()
    );
    assert_eq!(
        database
            .get_image_semantics(bad)
            .unwrap()
            .caption
            .as_deref(),
        Some("caption")
    );
}

#[test]
fn metadata_update_invalidates_incompatible_vectors_and_protects_manual_text() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("image.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [21, 43, 65, 255]);
    let mut database = MemeDatabase::open(
        directory.path().join("library"),
        FakeEmbeddingProvider::valid(),
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Index validation");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    let text = memelith_core::semantic::build_semantic_text(
        "caption",
        &["tag".to_owned()],
        "OCR",
        "sticker",
    );
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let valid = UpdateImageSemantics {
        image_type: ImageType::Sticker,
        image_type_source: "automatic".to_owned(),
        image_review_status: "confirmed".to_owned(),
        caption: Some("caption".to_owned()),
        semantic_tags: vec!["tag".to_owned()],
        visible_text: Some("OCR".to_owned()),
        status: "done".to_owned(),
        error: None,
        prompt_version: Some(memelith_core::semantic::CAPTION_PROMPT_VERSION.to_owned()),
        text_hash: Some(hash),
        embedding_provider: Some("clip".to_owned()),
        embedding_model: Some("test-embedding-v1".to_owned()),
        embedding_dimension: Some(4),
        embedding: Some(
            [1f32, 2., 3., 4.]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
        ),
    };
    database.update_image_semantics(id, valid.clone()).unwrap();
    assert_eq!(
        database.get_image_semantics(id).unwrap().embedding_status,
        "done"
    );
    for variant in 0..6 {
        let mut update = valid.clone();
        match variant {
            0 => update.caption = Some("changed".to_owned()),
            1 => update.embedding_provider = Some("other".to_owned()),
            2 => update.embedding_model = Some("other".to_owned()),
            3 => update.embedding_dimension = Some(8),
            4 => update.embedding = Some(vec![0; 16]),
            _ => {
                update.embedding = Some(
                    [f32::NAN; 4]
                        .into_iter()
                        .flat_map(f32::to_le_bytes)
                        .collect(),
                )
            }
        }
        database.update_image_semantics(id, update).unwrap();
        assert_eq!(
            database.get_image_semantics(id).unwrap().embedding_status,
            "pending"
        );
        assert!(
            database
                .search_vlm_semantics("caption", 0)
                .unwrap()
                .is_empty()
        );
    }
    database
        .set_manual_image_types(&[(id, ImageType::Sticker)])
        .unwrap();
    database.update_image_semantics(id, valid.clone()).unwrap();
    let automatic = database.get_image_semantics(id).unwrap();
    assert_eq!(automatic.image_type_source, "manual");
    assert_eq!(automatic.provenance, "automatic");
    let mut manual = valid.clone();
    manual.image_type_source = "manual".to_owned();
    manual.caption = Some("manual caption".to_owned());
    database.update_image_semantics(id, manual).unwrap();
    database.update_image_semantics(id, valid).unwrap();
    database
        .fail_image_semantics(id, "late network error")
        .unwrap();
    assert!(database.begin_image_semantics(id).is_err());
    assert_eq!(database.get_image_semantics(id).unwrap().status, "done");
    assert!(database.get_image_semantics(id).unwrap().error.is_none());
    assert_eq!(
        database.get_image_semantics(id).unwrap().caption.as_deref(),
        Some("manual caption")
    );
}

#[test]
fn editing_from_another_connection_during_embedding_discards_the_old_vector() {
    struct EditingProvider {
        database_path: std::path::PathBuf,
        edited: bool,
    }
    impl EmbeddingProvider for EditingProvider {
        fn model_id(&self) -> &str {
            "test-embedding-v1"
        }
        fn dimension(&self) -> usize {
            4
        }
        fn embed_image(
            &mut self,
            image: &DynamicImage,
        ) -> Result<Vec<f32>, EmbeddingProviderError> {
            Ok(FakeEmbeddingProvider::image_values(image, 4))
        }
        fn embed_text(&mut self, text: &str) -> Result<Vec<f32>, EmbeddingProviderError> {
            if !self.edited && text.starts_with("图片含义：") {
                self.edited = true;
                let connection = Connection::open(&self.database_path)?;
                connection.execute_batch("BEGIN IMMEDIATE;
                    UPDATE meme_contents SET visible_text='new OCR',semantic_embedding=NULL,semantic_text_hash=NULL WHERE kind='image';
                    UPDATE image_semantic_state SET provenance='manual',embedding_status='pending';
                    COMMIT;")?;
            }
            Ok(FakeEmbeddingProvider::text_values(text, 4))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("image.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [65, 43, 21, 255]);
    let root = directory.path().join("library");
    let mut database = MemeDatabase::open(
        &root,
        EditingProvider {
            database_path: root.join("memelith.sqlite3"),
            edited: false,
        },
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Concurrent edit");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .save_vlm_semantics(
            id,
            ImageType::Unknown,
            "automatic",
            "needs_review",
            "caption",
            &["tag".to_owned()],
            "old OCR",
            None,
        )
        .unwrap();
    assert!(database.rebuild_image_semantics(id).is_err());
    let item = database.get_image_semantics(id).unwrap();
    assert_eq!(item.visible_text.as_deref(), Some("new OCR"));
    assert_eq!(item.provenance, "manual");
    assert_eq!(item.embedding_status, "pending");
    assert!(item.embedding_error.is_none());
    assert!(item.text_hash.is_none());
    assert!(
        database
            .search_vlm_semantics("caption", 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 1);
    assert_eq!(database.rebuild_pending_semantics().unwrap(), 0);
}

#[test]
fn reopening_database_recovers_interrupted_semantic_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("running.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [4, 5, 6, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Recovery");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .set_imported_image_types(&[(id, ImageType::Sticker)])
        .unwrap();
    assert_eq!(
        database.get_image_semantics(id).unwrap().image_type_source,
        "imported"
    );
    database
        .set_manual_image_types(&[(id, ImageType::Illustration)])
        .unwrap();
    database
        .set_imported_image_types(&[(id, ImageType::Sticker)])
        .unwrap();
    assert_eq!(
        database.get_image_semantics(id).unwrap().image_type,
        ImageType::Illustration
    );
    let raw = Connection::open(database.database_path()).unwrap();
    raw.execute("UPDATE meme_contents SET semantic_status = 'running', semantic_error = 'interrupted' WHERE id = ?1", [id.to_string()]).unwrap();
    drop(raw);
    drop(database);
    let database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let semantics = database.get_image_semantics(id).unwrap();
    assert_eq!(semantics.status, "pending");
    assert_eq!(semantics.error, None);
}

#[test]
fn semantic_job_state_transitions_are_explicit() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("state.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [7, 8, 9, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "States");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database.begin_image_semantics(id).unwrap();
    assert_eq!(database.get_image_semantics(id).unwrap().status, "running");
    assert!(database.begin_image_semantics(id).is_err());
    database.fail_image_semantics(id, "timeout").unwrap();
    let semantics = database.get_image_semantics(id).unwrap();
    assert_eq!(semantics.status, "failed");
    assert_eq!(semantics.error.as_deref(), Some("timeout"));
    assert_eq!(database.list_images_pending_semantics().unwrap().len(), 1);
}

#[test]
fn resolving_review_marks_manual_and_removes_queue_entry() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("review.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [2, 4, 6, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Review");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .update_image_semantics(
            id,
            UpdateImageSemantics {
                image_type: ImageType::Sticker,
                image_type_source: "automatic".to_owned(),
                image_review_status: "needs_review".to_owned(),
                caption: Some("caption".to_owned()),
                semantic_tags: vec!["a".to_owned()],
                visible_text: None,
                status: "done".to_owned(),
                error: None,
                prompt_version: None,
                text_hash: Some("hash".to_owned()),
                embedding_provider: None,
                embedding_model: None,
                embedding_dimension: None,
                embedding: None,
            },
        )
        .unwrap();
    assert_eq!(database.list_images_needing_review().unwrap().len(), 1);
    database
        .resolve_image_review(id, ImageType::Illustration)
        .unwrap();
    let result = database.get_image_semantics(id).unwrap();
    assert_eq!(result.image_type, ImageType::Illustration);
    assert_eq!(result.image_type_source, "manual");
    assert_eq!(result.image_review_status, "confirmed");
    assert!(database.list_images_needing_review().unwrap().is_empty());
}

#[test]
fn manual_image_type_is_persistent_atomic_and_invalidates_only_semantic_vectors() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("manual.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [3, 9, 12, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Manual");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let image_id = meme.contents[0].id();
    let update = UpdateImageSemantics {
        image_type: ImageType::Sticker,
        image_type_source: "automatic".to_owned(),
        image_review_status: "confirmed".to_owned(),
        caption: Some("caption".to_owned()),
        semantic_tags: vec!["tag".to_owned()],
        visible_text: Some("OCR".to_owned()),
        status: "done".to_owned(),
        error: None,
        prompt_version: Some("test".to_owned()),
        text_hash: Some("hash".to_owned()),
        embedding_provider: Some("test".to_owned()),
        embedding_model: Some("test-embedding-v1".to_owned()),
        embedding_dimension: Some(4),
        embedding: Some(
            [1.0f32, 2.0, 3.0, 4.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
        ),
    };
    database
        .update_image_semantics(image_id, update.clone())
        .unwrap();
    let raw = Connection::open(database.database_path()).unwrap();
    let clip_before: Vec<u8> = raw
        .query_row(
            "SELECT embedding FROM meme_contents WHERE id = ?1",
            [image_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        database
            .set_manual_image_types(&[
                (image_id, ImageType::Illustration),
                (uuid::Uuid::new_v4(), ImageType::Unknown)
            ])
            .is_err()
    );
    let unchanged: String = raw
        .query_row(
            "SELECT image_type_source FROM meme_contents WHERE id = ?1",
            [image_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, "automatic");
    database
        .set_manual_image_types(&[(image_id, ImageType::Unknown)])
        .unwrap();
    assert!(
        database
            .update_image_semantics(image_id, update.clone())
            .is_err()
    );
    let (clip_after, semantic, hash, source): (Vec<u8>, Option<Vec<u8>>, Option<String>, String) = raw.query_row(
        "SELECT embedding, semantic_embedding, semantic_text_hash, image_type_source FROM meme_contents WHERE id = ?1",
        [image_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap();
    assert_eq!(clip_before, clip_after);
    assert_eq!(semantic, None);
    assert_eq!(hash, None);
    assert_eq!(source, "manual");
    let mut same_type = update;
    same_type.image_type = ImageType::Unknown;
    database
        .update_image_semantics(image_id, same_type)
        .unwrap();
    let source: String = raw
        .query_row(
            "SELECT image_type_source FROM meme_contents WHERE id = ?1",
            [image_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, "manual");
    drop(raw);
    drop(database);
    let database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let loaded = database.get_meme(meme.id).unwrap();
    let MemeContent::Image(image) = &loaded.contents[0] else {
        panic!("expected image")
    };
    assert_eq!(image.image_type, ImageType::Unknown);
    assert_eq!(image.visible_text.as_deref(), Some("OCR"));
}

#[test]
fn detects_all_supported_formats_and_embeds_the_first_gif_frame() {
    let directory = tempfile::tempdir().unwrap();
    let sources = directory.path().join("sources");
    fs::create_dir(&sources).unwrap();
    let png = sources.join("image.bin");
    let jpeg = sources.join("photo.bin");
    let webp = sources.join("web.bin");
    let gif = sources.join("animated.bin");
    write_static_image(&png, EncodedImageFormat::Png, 2, 3, [200, 0, 0, 255]);
    write_static_image(&jpeg, EncodedImageFormat::Jpeg, 4, 2, [0, 200, 0, 255]);
    write_static_image(&webp, EncodedImageFormat::WebP, 3, 4, [0, 0, 200, 255]);
    write_animated_gif(&gif);

    let mut database = MemeDatabase::open(
        directory.path().join("library"),
        FakeEmbeddingProvider::valid(),
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Formats");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: [&png, &jpeg, &webp, &gif]
                    .into_iter()
                    .map(|path| NewMemeContent::Image {
                        source_path: path.clone(),
                    })
                    .collect(),
            },
        )
        .unwrap();

    let formats = meme
        .contents
        .iter()
        .map(|content| match content {
            MemeContent::Image(image) => image.format,
            other => panic!("expected image content, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        formats,
        vec![
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            ImageFormat::Gif
        ]
    );
    let managed_paths = meme
        .contents
        .iter()
        .map(|content| match content {
            MemeContent::Image(image) => database.resolve_media_path(&image.relative_path).unwrap(),
            other => panic!("expected image content, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let gif_content = match &meme.contents[3] {
        MemeContent::Image(image) => image,
        other => panic!("expected GIF content, got {other:?}"),
    };
    assert_eq!((gif_content.width, gif_content.height), (2, 2));

    let raw = Connection::open(database.database_path()).unwrap();
    let gif_embedding: Vec<u8> = raw
        .query_row(
            "SELECT embedding FROM meme_contents WHERE id = ?1",
            [gif_content.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let first_frame = image::ImageReader::open(&gif)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(
        decode_embedding(&gif_embedding),
        FakeEmbeddingProvider::image_values(&first_frame, 4)
    );
    drop(raw);
    database.delete_meme_pack(pack.id).unwrap();
    assert!(managed_paths.iter().all(|path| !path.exists()));
    assert!(matches!(
        database.get_meme(meme.id),
        Err(Error::MemeNotFound(id)) if id == meme.id
    ));
}

#[test]
fn combines_direct_and_inherited_tags_without_materializing_inheritance() {
    let directory = tempfile::tempdir().unwrap();
    let mut database =
        MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Tagged");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Text {
                    text: "tag me".to_owned(),
                }],
            },
        )
        .unwrap();
    let cat = database
        .create_tag(NewTag {
            name: "Cat".to_owned(),
        })
        .unwrap();
    let reaction = database
        .create_tag(NewTag {
            name: "reaction".to_owned(),
        })
        .unwrap();
    let reaction = database
        .rename_tag(reaction.id, "Emotion".to_owned())
        .unwrap();
    assert_eq!(reaction.name, "Emotion");
    assert!(matches!(
        database.rename_tag(reaction.id, "CAT".to_owned()),
        Err(Error::DuplicateTag(name)) if name == "CAT"
    ));
    assert!(matches!(
        database.create_tag(NewTag { name: " cAt ".to_owned() }),
        Err(Error::DuplicateTag(name)) if name == "cAt"
    ));

    database.attach_tag_to_meme_pack(pack.id, cat.id).unwrap();
    database.attach_tag_to_meme(meme.id, cat.id).unwrap();
    database
        .attach_tag_to_meme_pack(pack.id, reaction.id)
        .unwrap();
    assert_eq!(
        database.list_meme_effective_tags(meme.id).unwrap(),
        vec![
            EffectiveTag {
                tag: cat.clone(),
                direct: true,
                inherited: true,
            },
            EffectiveTag {
                tag: reaction.clone(),
                direct: false,
                inherited: true,
            },
        ]
    );
    assert!(matches!(
        database.attach_tag_to_meme(meme.id, cat.id),
        Err(Error::TagAssociationExists { target: "Meme", target_id, tag_id })
            if target_id == meme.id && tag_id == cat.id
    ));

    database.detach_tag_from_meme_pack(pack.id, cat.id).unwrap();
    assert_eq!(
        database.list_meme_effective_tags(meme.id).unwrap(),
        vec![
            EffectiveTag {
                tag: cat.clone(),
                direct: true,
                inherited: false,
            },
            EffectiveTag {
                tag: reaction,
                direct: false,
                inherited: true,
            },
        ]
    );
    assert!(matches!(
        database.detach_tag_from_meme_pack(pack.id, cat.id),
        Err(Error::TagAssociationNotFound { target: "MemePack", target_id, tag_id })
            if target_id == pack.id && tag_id == cat.id
    ));
    database.delete_tag(cat.id).unwrap();
    assert!(database.list_meme_direct_tags(meme.id).unwrap().is_empty());
}

#[test]
fn rejects_invalid_inputs_and_rolls_back_provider_failures() {
    let directory = tempfile::tempdir().unwrap();
    let mut database =
        MemeDatabase::open(directory.path(), FakeEmbeddingProvider::valid()).unwrap();
    assert!(matches!(
        database.create_meme_pack(NewMemePack {
            name: "   ".to_owned(),
            description: None,
            author: None,
            source: None,
        }),
        Err(Error::EmptyField {
            field: "MemePack name"
        })
    ));
    let pack = simple_pack(&mut database, "Valid");
    assert!(matches!(
        database.create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![]
            }
        ),
        Err(Error::EmptyMemeContents)
    ));
    let unsupported = directory.path().join("not-an-image.txt");
    fs::write(&unsupported, b"not an image").unwrap();
    assert!(matches!(
        database.create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image { source_path: unsupported.clone() }],
            }
        ),
        Err(Error::UnsupportedImageFormat(path)) if path == unsupported
    ));
    assert!(database.list_memes(pack.id).unwrap().is_empty());

    let wrong_directory = tempfile::tempdir().unwrap();
    let mut wrong = FakeEmbeddingProvider::valid();
    wrong.wrong_output_dimension = Some(3);
    let mut wrong_database = MemeDatabase::open(wrong_directory.path(), wrong).unwrap();
    assert!(matches!(
        wrong_database.create_meme_pack(NewMemePack {
            name: "Wrong vector".to_owned(),
            description: None,
            author: None,
            source: None,
        }),
        Err(Error::InvalidEmbeddingDimension {
            field: "MemePack name",
            expected: 4,
            actual: 3,
        })
    ));
    assert!(wrong_database.list_meme_packs().unwrap().is_empty());

    let failed_directory = tempfile::tempdir().unwrap();
    let mut failing = FakeEmbeddingProvider::valid();
    failing.fail = true;
    let mut failed_database = MemeDatabase::open(failed_directory.path(), failing).unwrap();
    assert!(matches!(
        failed_database.create_meme_pack(NewMemePack {
            name: "Failure".to_owned(),
            description: None,
            author: None,
            source: None,
        }),
        Err(Error::EmbeddingProvider(_))
    ));
    assert!(failed_database.list_meme_packs().unwrap().is_empty());

    let zero_directory = tempfile::tempdir().unwrap();
    let mut zero_database = MemeDatabase::open(
        zero_directory.path(),
        FixedVectorProvider {
            values: vec![0.0; 4],
        },
    )
    .unwrap();
    assert!(matches!(
        zero_database.create_tag(NewTag {
            name: "zero".to_owned()
        }),
        Err(Error::ZeroEmbedding { field: "Tag name" })
    ));

    let non_finite_directory = tempfile::tempdir().unwrap();
    let mut non_finite_database = MemeDatabase::open(
        non_finite_directory.path(),
        FixedVectorProvider {
            values: vec![1.0, f32::NAN, 2.0, 3.0],
        },
    )
    .unwrap();
    assert!(matches!(
        non_finite_database.create_tag(NewTag {
            name: "non-finite".to_owned()
        }),
        Err(Error::NonFiniteEmbedding {
            field: "Tag name",
            index: 1,
        })
    ));
}

#[test]
fn enforces_embedding_compatibility_schema_version_and_media_integrity() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("library");
    {
        let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
        let pack = simple_pack(&mut database, "Integrity");
        let source = directory.path().join("source.png");
        write_static_image(&source, EncodedImageFormat::Png, 2, 2, [1, 2, 3, 255]);
        let meme = database
            .create_meme(
                pack.id,
                NewMeme {
                    name: None,
                    description: None,
                    contents: vec![NewMemeContent::Image {
                        source_path: source,
                    }],
                },
            )
            .unwrap();
        let image = match &meme.contents[0] {
            MemeContent::Image(image) => image,
            other => panic!("expected image content, got {other:?}"),
        };
        fs::remove_file(database.resolve_media_path(&image.relative_path).unwrap()).unwrap();
        assert!(matches!(
            database.get_meme(meme.id),
            Err(Error::MissingMedia(_))
        ));
    }

    let mut different_model = FakeEmbeddingProvider::valid();
    different_model.model_id = "test-embedding-v2".to_owned();
    assert!(matches!(
        MemeDatabase::open(&storage, different_model),
        Err(Error::IncompatibleEmbeddingSpace {
            expected_model,
            actual_model,
            expected_dimension: 4,
            actual_dimension: 4,
        }) if expected_model == "test-embedding-v1" && actual_model == "test-embedding-v2"
    ));

    let raw = Connection::open(storage.join("memelith.sqlite3")).unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    drop(raw);
    assert!(matches!(
        MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()),
        Err(Error::UnsupportedSchemaVersion {
            expected: 6,
            actual: 99
        })
    ));
}

#[test]
fn motion_semantic_media_uses_preview_when_available() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("motion.mp4");
    let preview = directory.path().join("motion.png");
    fs::write(&source, b"video bytes").unwrap();
    write_static_image(&preview, EncodedImageFormat::Png, 2, 2, [1, 2, 3, 255]);
    let mut database = MemeDatabase::open(
        directory.path().join("library"),
        FakeEmbeddingProvider::valid(),
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Motion");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Motion {
                    source_path: source,
                    preview_path: Some(preview),
                    width: 2,
                    height: 2,
                    format: memelith_core::MotionFormat::Mp4,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    let selected = database.semantic_media_path(id).unwrap();
    assert_eq!(
        selected.extension().and_then(|ext| ext.to_str()),
        Some("png")
    );
    assert!(selected.is_file());
    database
        .apply_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "动图语义",
            &["动作".to_owned()],
            "动图文字",
        )
        .unwrap();
    assert!(matches!(
        &database.get_meme(meme.id).unwrap().contents[0],
        MemeContent::Motion(motion)
            if motion.image_type == ImageType::Sticker
                && motion.visible_text.as_deref() == Some("动图文字")
    ));
}

#[test]
fn imported_image_type_requests_vlm_processing() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("imported.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [4, 5, 6, 255]);
    let mut database = MemeDatabase::open(
        directory.path().join("library"),
        FakeEmbeddingProvider::valid(),
    )
    .unwrap();
    let pack = simple_pack(&mut database, "Imported");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    database
        .set_imported_image_types(&[(id, ImageType::Sticker)])
        .unwrap();
    let semantics = database.get_image_semantics(id).unwrap();
    assert_eq!(semantics.image_type, ImageType::Sticker);
    assert_eq!(semantics.image_type_source, "imported");
}

#[test]
fn semantic_index_signature_detects_every_rebuild_trigger() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("signature.png");
    write_static_image(&source, EncodedImageFormat::Png, 2, 2, [2, 5, 8, 255]);
    let storage = directory.path().join("library");
    let mut database = MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap();
    let pack = simple_pack(&mut database, "Signature");
    let meme = database
        .create_meme(
            pack.id,
            NewMeme {
                name: None,
                description: None,
                contents: vec![NewMemeContent::Image {
                    source_path: source,
                }],
            },
        )
        .unwrap();
    let id = meme.contents[0].id();
    let legacy_results = database.search_memes_semantic("caption", 0).unwrap();
    assert!(
        database
            .search_vlm_semantics("caption", 0)
            .unwrap()
            .is_empty()
    );
    database
        .apply_vlm_semantics(
            id,
            ImageType::Sticker,
            "automatic",
            "confirmed",
            "caption",
            &(0..6).map(|i| format!("tag{i}")).collect::<Vec<_>>(),
            "ocr",
        )
        .unwrap();
    let semantics = database.get_image_semantics(id).unwrap();
    assert_eq!(
        database.search_memes_semantic("caption", 0).unwrap(),
        legacy_results
    );
    assert_eq!(
        database.search_vlm_semantics("caption", 0).unwrap()[0].meme_id,
        meme.id
    );
    let connection = Connection::open(database.database_path()).unwrap();
    connection
        .execute(
            "UPDATE meme_contents SET semantic_embedding_model = 'other-model' WHERE id = ?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(
        database
            .search_vlm_semantics("caption", 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        database.search_memes_semantic("caption", 0).unwrap(),
        legacy_results
    );
    connection
        .execute(
            "UPDATE meme_contents SET semantic_embedding_model = 'test-embedding-v1' WHERE id = ?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(!database.semantic_index_needs_rebuild(
        &semantics,
        "clip",
        "test-embedding-v1",
        4,
        memelith_core::semantic::CAPTION_PROMPT_VERSION,
        semantics.text_hash.as_deref().unwrap()
    ));
    assert!(database.semantic_index_needs_rebuild(
        &semantics,
        "other",
        "test-embedding-v1",
        4,
        memelith_core::semantic::CAPTION_PROMPT_VERSION,
        semantics.text_hash.as_deref().unwrap()
    ));
    assert!(database.semantic_index_needs_rebuild(
        &semantics,
        "clip",
        "other",
        4,
        memelith_core::semantic::CAPTION_PROMPT_VERSION,
        semantics.text_hash.as_deref().unwrap()
    ));
    assert!(database.semantic_index_needs_rebuild(
        &semantics,
        "clip",
        "test-embedding-v1",
        8,
        memelith_core::semantic::CAPTION_PROMPT_VERSION,
        semantics.text_hash.as_deref().unwrap()
    ));
    assert!(database.semantic_index_needs_rebuild(
        &semantics,
        "clip",
        "test-embedding-v1",
        4,
        "other",
        semantics.text_hash.as_deref().unwrap()
    ));
    assert!(database.semantic_index_needs_rebuild(
        &semantics,
        "clip",
        "test-embedding-v1",
        4,
        memelith_core::semantic::CAPTION_PROMPT_VERSION,
        "other-hash"
    ));
}

#[test]
fn semantic_search_returns_at_most_requested_limit_and_ignores_empty_query() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = MemeDatabase::open(
        directory.path(),
        FixedVectorProvider {
            values: vec![1.0, 0.0, 0.0, 0.0],
        },
    )
    .unwrap();
    assert!(
        database
            .search_memes_semantic("   ", 10)
            .unwrap()
            .is_empty()
    );
    let first = simple_pack(&mut database, "First");
    let second = simple_pack(&mut database, "Second");
    database
        .create_meme(
            first.id,
            NewMeme {
                name: Some("alpha".to_owned()),
                description: None,
                contents: vec![NewMemeContent::Text {
                    text: "a".to_owned(),
                }],
            },
        )
        .unwrap();
    database
        .create_meme(
            second.id,
            NewMeme {
                name: Some("beta".to_owned()),
                description: None,
                contents: vec![NewMemeContent::Text {
                    text: "b".to_owned(),
                }],
            },
        )
        .unwrap();
    assert!(database.search_memes_semantic("query", 1).unwrap().len() <= 1);
}

#[test]
fn removes_staging_and_orphaned_media_when_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("library");
    drop(MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap());
    let orphan = storage.join("media/images/orphan.png");
    let staged = storage.join(".staging/unfinished.tmp");
    fs::write(&orphan, b"orphan").unwrap();
    fs::write(&staged, b"unfinished").unwrap();

    drop(MemeDatabase::open(&storage, FakeEmbeddingProvider::valid()).unwrap());

    assert!(!orphan.exists());
    assert!(!staged.exists());
}

fn simple_pack(database: &mut MemeDatabase, name: &str) -> memelith_core::MemePack {
    database
        .create_meme_pack(NewMemePack {
            name: name.to_owned(),
            description: None,
            author: None,
            source: None,
        })
        .unwrap()
}

fn write_static_image(
    path: &Path,
    format: EncodedImageFormat,
    width: u32,
    height: u32,
    color: [u8; 4],
) {
    let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(width, height, Rgba(color)));
    image.save_with_format(path, format).unwrap();
}

fn write_animated_gif(path: &Path) {
    let first: RgbaImage = ImageBuffer::from_pixel(2, 2, Rgba([240, 0, 0, 255]));
    let second: RgbaImage = ImageBuffer::from_pixel(2, 2, Rgba([0, 0, 240, 255]));
    let file = File::create(path).unwrap();
    let mut encoder = GifEncoder::new(file);
    encoder
        .encode_frames([
            Frame::from_parts(first, 0, 0, Delay::from_numer_denom_ms(100, 1)),
            Frame::from_parts(second, 0, 0, Delay::from_numer_denom_ms(100, 1)),
        ])
        .unwrap();
}

fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}
