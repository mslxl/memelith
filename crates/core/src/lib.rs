#![forbid(unsafe_code)]

mod database;
mod embedding;
mod error;
mod model;

pub use database::{COLLECTOR_DUPLICATE_MAX_COSINE_DISTANCE, MemeDatabase};
pub use embedding::{EmbeddingProvider, EmbeddingProviderError};
pub use error::{Error, Result};
pub use model::{
    CollectorContent, CollectorDuplicate, CollectorDuplicateSource, CollectorDuplicateTarget,
    CollectorItem, EffectiveTag, ImageDuplicate, ImageFormat, Meme, MemeContent, MemeImage,
    MemeMotion, MemePack, MemeText, MotionFormat, NewMeme, NewMemeContent, NewMemeFromCollector,
    NewMemePack, NewTag, SemanticMemeMatch, SimilarMemeImage, Tag, UpdateMemeMetadata,
    UpdateMemePack,
};

pub const APPLICATION_NAME: &str = "Memelith";

/// Application state owned by the Memelith frontend.
///
/// waifu-sensor is an independent library and CLI; applications opt into it by
/// constructing the library with their own SQLite connection and asset paths.
#[derive(Debug, Default)]
pub struct Memelith;

impl Memelith {
    pub fn new() -> Self {
        Self
    }

    pub fn status_summary(&self) -> &'static str {
        "Ready"
    }
}

#[cfg(test)]
mod tests {
    use super::Memelith;

    #[test]
    fn memelith_core_does_not_construct_waifu_sensor_implicitly() {
        assert_eq!(Memelith::new().status_summary(), "Ready");
    }
}
