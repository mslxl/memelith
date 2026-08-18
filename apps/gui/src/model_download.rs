use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use reqwest::{StatusCode, blocking::Client, header::RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const APPLICATION_MODELS_DIRECTORY: &str = "memelith/models";
const CLIP_DIRECTORY: &str = "chinese-clip-vit-base-patch16";
const WAIFU_DIRECTORY: &str = "ml-danbooru";
const DOWNLOAD_BUFFER_BYTES: usize = 1024 * 1024;
const INSTALLATION_MARKER_FILENAME: &str = ".models-installed.json";
const INSTALLATION_MARKER_SCHEMA: u32 = 1;
const ALL_MODELS_LABEL: &str = "本地模型";

const CLIP_MODEL_MANIFEST: &[u8] =
    include_bytes!("../../../crates/clip/assets/models/chinese-clip-vit-base-patch16/model.json");
const CLIP_TOKENIZER: &[u8] = include_bytes!(
    "../../../crates/clip/assets/models/chinese-clip-vit-base-patch16/tokenizer.json"
);
const CLIP_NOTICE: &[u8] =
    include_bytes!("../../../crates/clip/assets/models/chinese-clip-vit-base-patch16/NOTICE.md");

const REMOTE_ASSETS: [RemoteAsset; 3] = [
    RemoteAsset {
        label: "CLIP text model",
        directory: CLIP_DIRECTORY,
        filename: "text_encoder.onnx",
        url: "https://huggingface.co/zihuv/chinese-clip-vit-base-patch16-onnx/resolve/8001064569c647b9d9ae7756872ba8c334753d91/text.onnx",
        size: 408_491_787,
        sha256: "6d8de6ba498ae2c944ec2b578371c1b3e9dd5a9a991dd4f7dabc16b8111c6f4d",
    },
    RemoteAsset {
        label: "CLIP image model",
        directory: CLIP_DIRECTORY,
        filename: "image_encoder.onnx",
        url: "https://huggingface.co/zihuv/chinese-clip-vit-base-patch16-onnx/resolve/8001064569c647b9d9ae7756872ba8c334753d91/visual.onnx",
        size: 344_963_070,
        sha256: "303f72307943b7ef5e478ac0f8a5686bdfc62847b8b331e32e89fe223ffcfe35",
    },
    RemoteAsset {
        label: "Waifu Sensor model",
        directory: WAIFU_DIRECTORY,
        filename: "ml_caformer_m36_dec-5-97527.onnx",
        url: "https://huggingface.co/deepghs/ml-danbooru-onnx/resolve/eb9058324a741f1b90d4db168f6e1d6b6cb7e63d/ml_caformer_m36_dec-5-97527.onnx",
        size: 286_584_124,
        sha256: "4ea7aa66df59632c71e036cd9eda9c89e0eaf58bcd39bf9718e63583378cba75",
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledModels {
    pub clip_directory: PathBuf,
    pub waifu_model_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelInstallPhase {
    Checking,
    Downloading,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelInstallProgress {
    pub phase: ModelInstallPhase,
    pub asset_label: &'static str,
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

impl ModelInstallProgress {
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.completed_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
    }
}

#[derive(Debug, Error)]
pub enum ModelInstallError {
    #[error("the operating system did not provide an application data directory")]
    DataDirectoryUnavailable,

    #[error("model request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("model file operation failed: {0}")]
    Io(#[from] io::Error),

    #[error("downloaded model `{path}` has {actual} bytes; expected {expected}")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    #[error("downloaded model `{path}` has SHA-256 {actual}; expected {expected}")]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error(
        "model installation failed and temporary file `{path}` could not be removed: {cleanup}"
    )]
    Cleanup {
        path: PathBuf,
        #[source]
        source: Box<ModelInstallError>,
        cleanup: io::Error,
    },
}

type Result<T> = std::result::Result<T, ModelInstallError>;

#[derive(Clone, Copy)]
struct RemoteAsset {
    label: &'static str,
    directory: &'static str,
    filename: &'static str,
    url: &'static str,
    size: u64,
    sha256: &'static str,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallationMarker {
    schema: u32,
    assets: Vec<InstallationMarkerAsset>,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallationMarkerAsset {
    directory: String,
    filename: String,
    size: u64,
    sha256: String,
    modified_seconds: u64,
    modified_nanos: u32,
}

pub fn ensure_default_models(
    mut on_progress: impl FnMut(ModelInstallProgress),
) -> Result<InstalledModels> {
    let data_directory =
        dirs::data_local_dir().ok_or(ModelInstallError::DataDirectoryUnavailable)?;
    ensure_models_in(
        &data_directory.join(APPLICATION_MODELS_DIRECTORY),
        &mut on_progress,
    )
}

fn ensure_models_in(
    root: &Path,
    on_progress: &mut impl FnMut(ModelInstallProgress),
) -> Result<InstalledModels> {
    fs::create_dir_all(root)?;
    let clip_directory = root.join(CLIP_DIRECTORY);
    fs::create_dir_all(&clip_directory)?;
    write_embedded_file(&clip_directory.join("model.json"), CLIP_MODEL_MANIFEST)?;
    write_embedded_file(&clip_directory.join("tokenizer.json"), CLIP_TOKENIZER)?;
    write_embedded_file(&clip_directory.join("NOTICE.md"), CLIP_NOTICE)?;

    let total_bytes = REMOTE_ASSETS.iter().map(|asset| asset.size).sum();
    if installation_marker_matches(root) {
        on_progress(ModelInstallProgress {
            phase: ModelInstallPhase::Checking,
            asset_label: ALL_MODELS_LABEL,
            completed_bytes: total_bytes,
            total_bytes,
        });
        return Ok(installed_models(root, clip_directory));
    }

    let client = Client::builder()
        .user_agent(concat!("Memelith/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30 * 60))
        .build()?;
    let mut completed_bytes = 0;
    for asset in REMOTE_ASSETS {
        let directory = root.join(asset.directory);
        fs::create_dir_all(&directory)?;
        let path = directory.join(asset.filename);
        on_progress(ModelInstallProgress {
            phase: ModelInstallPhase::Checking,
            asset_label: asset.label,
            completed_bytes,
            total_bytes,
        });
        if verify_file(&path, asset.size, asset.sha256)? {
            remove_file_if_exists(&temporary_path(&directory, asset.filename))?;
            completed_bytes += asset.size;
            on_progress(ModelInstallProgress {
                phase: ModelInstallPhase::Checking,
                asset_label: asset.label,
                completed_bytes,
                total_bytes,
            });
            continue;
        }

        let temporary = temporary_path(&directory, asset.filename);
        let mut partial_size = match fs::metadata(&temporary) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            Ok(_) => {
                remove_file_if_exists(&temporary)?;
                0
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        if partial_size > asset.size {
            remove_file_if_exists(&temporary)?;
            partial_size = 0;
        }
        if partial_size == asset.size {
            if verify_file(&temporary, asset.size, asset.sha256)? {
                fs::rename(&temporary, &path)?;
                completed_bytes += asset.size;
                on_progress(ModelInstallProgress {
                    phase: ModelInstallPhase::Checking,
                    asset_label: asset.label,
                    completed_bytes,
                    total_bytes,
                });
                continue;
            }
            remove_file_if_exists(&temporary)?;
            partial_size = 0;
        }

        let mut request = client.get(asset.url);
        if partial_size > 0 {
            request = request.header(RANGE, format!("bytes={partial_size}-"));
        }
        let response = request.send()?;
        if partial_size > 0
            && response.status() != StatusCode::PARTIAL_CONTENT
            && response.status().is_success()
        {
            remove_file_if_exists(&temporary)?;
        }
        let response = response.error_for_status()?;
        install_asset_from_reader(
            &directory,
            asset.filename,
            asset.size,
            asset.sha256,
            response,
            |asset_bytes| {
                on_progress(ModelInstallProgress {
                    phase: ModelInstallPhase::Downloading,
                    asset_label: asset.label,
                    completed_bytes: completed_bytes + asset_bytes,
                    total_bytes,
                });
            },
        )?;
        completed_bytes += asset.size;
    }

    write_installation_marker(root)?;
    Ok(installed_models(root, clip_directory))
}

fn installed_models(root: &Path, clip_directory: PathBuf) -> InstalledModels {
    InstalledModels {
        clip_directory,
        waifu_model_path: root
            .join(WAIFU_DIRECTORY)
            .join("ml_caformer_m36_dec-5-97527.onnx"),
    }
}

fn install_asset_from_reader(
    directory: &Path,
    filename: &str,
    expected_size: u64,
    expected_sha256: &str,
    mut reader: impl Read,
    mut on_progress: impl FnMut(u64),
) -> Result<()> {
    fs::create_dir_all(directory)?;
    let destination = directory.join(filename);
    let temporary = temporary_path(directory, filename);
    let existing_size = match fs::metadata(&temporary) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => {
            remove_file_if_exists(&temporary)?;
            0
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };

    let installation = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&temporary)?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
        let mut actual_size = existing_size;
        if existing_size > 0 {
            let mut existing = File::open(&temporary)?;
            loop {
                let read = existing.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
            on_progress(existing_size);
        }
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            digest.update(&buffer[..read]);
            actual_size += read as u64;
            on_progress(actual_size);
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);

        if actual_size != expected_size {
            return Err(ModelInstallError::SizeMismatch {
                path: destination.clone(),
                expected: expected_size,
                actual: actual_size,
            });
        }
        let actual_sha256 = hex::encode(digest.finalize());
        if actual_sha256 != expected_sha256 {
            return Err(ModelInstallError::ChecksumMismatch {
                path: destination.clone(),
                expected: expected_sha256.to_owned(),
                actual: actual_sha256,
            });
        }
        fs::rename(&temporary, &destination)?;
        Ok(())
    })();

    if let Err(source) = installation {
        let keep_partial = matches!(
            &source,
            ModelInstallError::Io(_) | ModelInstallError::SizeMismatch { .. }
        );
        if !keep_partial && let Err(cleanup) = remove_file_if_exists(&temporary) {
            return Err(ModelInstallError::Cleanup {
                path: temporary,
                source: Box::new(source),
                cleanup,
            });
        }
        return Err(source);
    }
    Ok(())
}

fn installation_marker_matches(root: &Path) -> bool {
    let marker_path = root.join(INSTALLATION_MARKER_FILENAME);
    let Ok(bytes) = fs::read(marker_path) else {
        return false;
    };
    let Ok(marker) = serde_json::from_slice::<InstallationMarker>(&bytes) else {
        return false;
    };
    if marker.schema != INSTALLATION_MARKER_SCHEMA || marker.assets.len() != REMOTE_ASSETS.len() {
        return false;
    }

    REMOTE_ASSETS
        .iter()
        .zip(marker.assets.iter())
        .all(|(asset, record)| {
            if record.directory != asset.directory
                || record.filename != asset.filename
                || record.size != asset.size
                || record.sha256 != asset.sha256
            {
                return false;
            }
            let path = root.join(asset.directory).join(asset.filename);
            let Ok(metadata) = fs::metadata(path) else {
                return false;
            };
            if !metadata.is_file() || metadata.len() != asset.size {
                return false;
            }
            let Some((modified_seconds, modified_nanos)) = modification_time(&metadata) else {
                return false;
            };
            modified_seconds == record.modified_seconds && modified_nanos == record.modified_nanos
        })
}

fn write_installation_marker(root: &Path) -> Result<()> {
    let assets = REMOTE_ASSETS
        .iter()
        .map(|asset| {
            let path = root.join(asset.directory).join(asset.filename);
            let metadata = fs::metadata(path)?;
            let (modified_seconds, modified_nanos) =
                modification_time(&metadata).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "model file has no usable modification time",
                    )
                })?;
            Ok(InstallationMarkerAsset {
                directory: asset.directory.to_owned(),
                filename: asset.filename.to_owned(),
                size: asset.size,
                sha256: asset.sha256.to_owned(),
                modified_seconds,
                modified_nanos,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let marker = InstallationMarker {
        schema: INSTALLATION_MARKER_SCHEMA,
        assets,
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_embedded_file(&root.join(INSTALLATION_MARKER_FILENAME), &bytes)
}

fn modification_time(metadata: &fs::Metadata) -> Option<(u64, u32)> {
    let elapsed = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((elapsed.as_secs(), elapsed.subsec_nanos()))
}

fn verify_file(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != expected_size {
        return Ok(false);
    }

    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()) == expected_sha256)
}

fn write_embedded_file(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded model file has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "embedded model filename is not UTF-8",
            )
        })?;
    let temporary = temporary_path(parent, filename);
    remove_file_if_exists(&temporary)?;
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    Ok(())
}

fn temporary_path(directory: &Path, filename: &str) -> PathBuf {
    directory.join(format!(".{filename}.part"))
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor};

    use sha2::{Digest, Sha256};

    use super::{
        ModelInstallError, install_asset_from_reader, temporary_path, verify_file,
        write_embedded_file,
    };

    #[test]
    fn installs_a_verified_asset_and_removes_the_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"verified model bytes";
        let sha256 = hex::encode(Sha256::digest(bytes));
        let mut progress = Vec::new();

        install_asset_from_reader(
            directory.path(),
            "model.onnx",
            bytes.len() as u64,
            &sha256,
            Cursor::new(bytes),
            |completed| progress.push(completed),
        )
        .unwrap();

        assert_eq!(
            fs::read(directory.path().join("model.onnx")).unwrap(),
            bytes
        );
        assert_eq!(progress, vec![bytes.len() as u64]);
        assert!(!temporary_path(directory.path(), "model.onnx").exists());
    }

    #[test]
    fn rejects_a_checksum_mismatch_without_exposing_the_asset() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"corrupt model bytes";

        let error = install_asset_from_reader(
            directory.path(),
            "model.onnx",
            bytes.len() as u64,
            &"0".repeat(64),
            Cursor::new(bytes),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, ModelInstallError::ChecksumMismatch { .. }));
        assert!(!directory.path().join("model.onnx").exists());
        assert!(!temporary_path(directory.path(), "model.onnx").exists());
    }

    #[test]
    fn verifies_both_size_and_digest_for_cached_assets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.onnx");
        let bytes = b"cached model";
        fs::write(&path, bytes).unwrap();
        let sha256 = hex::encode(Sha256::digest(bytes));

        assert!(verify_file(&path, bytes.len() as u64, &sha256).unwrap());
        assert!(!verify_file(&path, bytes.len() as u64 + 1, &sha256).unwrap());
        assert!(!verify_file(&path, bytes.len() as u64, &"f".repeat(64)).unwrap());
    }

    #[test]
    fn restores_embedded_support_files_to_the_exact_shipped_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.json");
        fs::write(&path, b"stale").unwrap();

        write_embedded_file(&path, b"trusted manifest").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"trusted manifest");
    }
}
