-- Schema from 96e348f, with a four-dimensional test embedding provider.
CREATE TABLE metadata (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            schema_version INTEGER NOT NULL,
            embedding_model_id TEXT NOT NULL CHECK(trim(embedding_model_id) <> ''),
            embedding_dimension INTEGER NOT NULL CHECK(embedding_dimension > 0)
        );
        CREATE TABLE meme_packs (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL CHECK(trim(name) <> ''),
            name_embedding BLOB NOT NULL CHECK(length(name_embedding) = 16),
            description TEXT,
            description_embedding BLOB,
            author TEXT,
            source TEXT,
            CHECK(
                (description IS NULL AND description_embedding IS NULL) OR
                (description IS NOT NULL AND trim(description) <> '' AND
                 description_embedding IS NOT NULL AND length(description_embedding) = 16)
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
                 name_embedding IS NOT NULL AND length(name_embedding) = 16)
            ),
            CHECK(
                (description IS NULL AND description_embedding IS NULL) OR
                (description IS NOT NULL AND trim(description) <> '' AND
                 description_embedding IS NOT NULL AND length(description_embedding) = 16)
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
            content_hash BLOB NOT NULL CHECK(length(content_hash) = 32),
            embedding BLOB CHECK(embedding IS NULL OR length(embedding) = 16),
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
            content_hash BLOB NOT NULL CHECK(length(content_hash) = 32),
            embedding BLOB CHECK(embedding IS NULL OR length(embedding) = 16),
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
            name_embedding BLOB NOT NULL CHECK(length(name_embedding) = 16)
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
        CREATE INDEX meme_tags_tag ON meme_tags(tag_id);
INSERT INTO metadata VALUES (1, 1, 'test-embedding-v1', 4);
PRAGMA user_version = 1;
