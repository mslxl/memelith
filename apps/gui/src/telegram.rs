use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use memelith_clip::{ClipModel, ExecutionPolicy};
use memelith_core::{
    COLLECTOR_DUPLICATE_MAX_COSINE_DISTANCE, MemeDatabase, MotionFormat, NewMeme, NewMemeContent,
    NewMemePack,
};
use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

const TELEGRAM_API: &str = "https://api.telegram.org";
const UPDATE_TIMEOUT_SECONDS: i64 = 2;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_MAX_IDLE_CONNECTIONS_PER_HOST: usize = 4;
const MAX_TELEGRAM_WORKERS: usize = 4;
const MAX_SIMILAR_PER_PAGE: usize = 9;
const STICKER_AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const TELEGRAM_STICKER_LINK_PREFIX: &str = "https://t.me/addstickers/";

#[derive(Debug, Error)]
enum TelegramError {
    #[error("Telegram request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Telegram returned an error: {0}")]
    Api(String),

    #[error("Telegram response did not contain a result")]
    MissingResult,

    #[error("Telegram response contained invalid data: {0}")]
    Json(#[from] serde_json::Error),

    #[error("image operation failed: {0}")]
    Image(#[from] image::ImageError),

    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("Memelith database operation failed: {0}")]
    Database(#[from] memelith_core::Error),

    #[error("Telegram file response did not include a file path")]
    MissingFilePath,

    #[error("invalid callback data")]
    InvalidCallback,

    #[error("callback is not authorized for this operation")]
    UnauthorizedCallback,

    #[error("Telegram message did not include a sender")]
    MissingSender,

    #[error("Telegram sticker pack reference is invalid")]
    InvalidStickerPackReference,

    #[error("Telegram sticker pack `{0}` is already being synchronized")]
    StickerPackAlreadySyncing(String),

    #[error("Telegram sticker file identifier is invalid")]
    InvalidStickerFileIdentifier,

    #[error("Telegram Bot is stopping")]
    Stopping,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelegramBotStatus {
    Starting,
    LoadingModel,
    Running,
    CollectorUpdated,
    StickerPackSyncStarted {
        pack_id: Uuid,
    },
    StickerPackSynced {
        pack_id: Uuid,
        title: String,
        added: usize,
        skipped: usize,
        announce: bool,
    },
    StickerPackSyncFailed {
        pack_id: Uuid,
        message: String,
        announce: bool,
        terminal: bool,
    },
    Failed(String),
    Stopped,
}

pub struct TelegramBotHandle {
    stop: Option<mpsc::Sender<()>>,
    commands: mpsc::Sender<TelegramBotCommand>,
    thread: Option<JoinHandle<()>>,
}

enum TelegramBotCommand {
    SyncStickerPack { pack_id: Uuid, source: String },
}

impl TelegramBotHandle {
    pub fn start(
        storage_root: PathBuf,
        token: String,
        model_directory: PathBuf,
    ) -> std::io::Result<(Self, mpsc::Receiver<TelegramBotStatus>)> {
        let (stop, stop_receiver) = mpsc::channel();
        let (commands, command_receiver) = mpsc::channel();
        let (status, status_receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("memelith-telegram".to_owned())
            .spawn(move || {
                let _ = status.send(TelegramBotStatus::Starting);
                match run_bot(
                    storage_root,
                    token,
                    model_directory,
                    stop_receiver,
                    command_receiver,
                    &status,
                ) {
                    Ok(()) => {
                        let _ = status.send(TelegramBotStatus::Stopped);
                    }
                    Err(error) => {
                        let message = runtime_error_message(&error);
                        eprintln!("Memelith Telegram bot stopped: {message}");
                        let _ = status.send(TelegramBotStatus::Failed(message));
                    }
                }
            })?;
        Ok((
            Self {
                stop: Some(stop),
                commands,
                thread: Some(thread),
            },
            status_receiver,
        ))
    }

    pub fn stop(mut self) {
        self.request_stop();
        self.join();
    }

    pub fn request_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    pub fn sync_sticker_pack(&self, pack_id: Uuid, source: String) -> Result<(), String> {
        self.commands
            .send(TelegramBotCommand::SyncStickerPack { pack_id, source })
            .map_err(|_| "Telegram Bot 线程未运行".to_owned())
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
            && thread.join().is_err()
        {
            eprintln!("Memelith Telegram bot thread panicked");
        }
    }
}

impl Drop for TelegramBotHandle {
    fn drop(&mut self) {
        self.request_stop();
        self.join();
    }
}

struct BotState {
    api: TelegramApi,
    database: Mutex<MemeDatabase>,
    storage_root: PathBuf,
    status: mpsc::Sender<TelegramBotStatus>,
    stopping: AtomicBool,
    syncing_sticker_sets: Mutex<HashSet<String>>,
    pending: Mutex<HashMap<Uuid, PendingOperation>>,
    pending_deletions: Mutex<HashSet<(i64, i64)>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone)]
struct PendingOperation {
    id: Uuid,
    chat_id: i64,
    confirmation_message_id: i64,
    media_message_ids: Vec<i64>,
    user_id: i64,
    source_path: PathBuf,
    preview_path: Option<PathBuf>,
    media: IncomingMedia,
    collector_item_id: Uuid,
    similar_paths: Vec<PathBuf>,
    duplicate_count: usize,
    page: usize,
}

impl PendingOperation {
    fn preview_path(&self) -> Option<&Path> {
        match self.media {
            IncomingMedia::Image => Some(&self.source_path),
            IncomingMedia::Motion { .. } => self.preview_path.as_deref(),
        }
    }
}

#[derive(Clone)]
enum IncomingMedia {
    Image,
    Motion {
        width: u32,
        height: u32,
        format: MotionFormat,
    },
}

#[derive(Clone)]
struct TelegramApi {
    client: Client,
    base_url: String,
}

impl TelegramApi {
    fn new(token: &str) -> Result<Self, TelegramError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(TelegramError::Api("Telegram Bot token is empty".to_owned()));
        }
        let client = Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .pool_max_idle_per_host(HTTP_MAX_IDLE_CONNECTIONS_PER_HOST)
            .pool_idle_timeout(HTTP_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            base_url: format!("{TELEGRAM_API}/bot{token}"),
        })
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        parameters: impl Serialize,
    ) -> Result<T, TelegramError> {
        let response = self
            .client
            .post(format!("{}/{method}", self.base_url))
            .json(&parameters)
            .send()?;
        let response = response.error_for_status()?;
        let body = response.json::<ApiResponse<T>>()?;
        if !body.ok {
            return Err(TelegramError::Api(
                body.description
                    .unwrap_or_else(|| "unknown Telegram error".to_owned()),
            ));
        }
        body.result.ok_or(TelegramError::MissingResult)
    }

    fn get_updates(&self, offset: i64) -> Result<Vec<Update>, TelegramError> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": UPDATE_TIMEOUT_SECONDS,
                "allowed_updates": ["message", "callback_query"],
            }),
        )
    }

    fn verify_bot(&self) -> Result<(), TelegramError> {
        self.call::<serde_json::Value>("getMe", json!({}))?;
        Ok(())
    }

    fn get_file(&self, file_id: &str) -> Result<TelegramFile, TelegramError> {
        self.call("getFile", json!({ "file_id": file_id }))
    }

    fn get_sticker_set(&self, name: &str) -> Result<StickerSet, TelegramError> {
        self.call("getStickerSet", json!({ "name": name }))
    }

    fn download_file(&self, file_path: &str) -> Result<Vec<u8>, TelegramError> {
        let token_url = self
            .base_url
            .strip_prefix(TELEGRAM_API)
            .ok_or_else(|| TelegramError::Api("invalid Telegram API URL".to_owned()))?;
        let url = format!("{TELEGRAM_API}/file{token_url}/{file_path}");
        Ok(self
            .client
            .get(url)
            .send()?
            .error_for_status()?
            .bytes()?
            .to_vec())
    }

    fn send_message(&self, chat_id: i64, text: &str) -> Result<Message, TelegramError> {
        self.call("sendMessage", json!({"chat_id": chat_id, "text": text}))
    }

    fn send_confirmation(
        &self,
        chat_id: i64,
        text: &str,
        markup: &InlineKeyboardMarkup,
        reply_to_message_id: Option<i64>,
    ) -> Result<Message, TelegramError> {
        let parameters = match reply_to_message_id {
            Some(message_id) => json!({
                "chat_id": chat_id,
                "text": text,
                "reply_markup": markup,
                "reply_parameters": {"message_id": message_id},
            }),
            None => json!({"chat_id": chat_id, "text": text, "reply_markup": markup}),
        };
        self.call("sendMessage", parameters)
    }

    fn send_preview_media(
        &self,
        chat_id: i64,
        paths: &[PathBuf],
        caption: &str,
    ) -> Result<Vec<Message>, TelegramError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        if paths.len() == 1 {
            let part = photo_part(&paths[0], 0)?;
            let form = multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .text("caption", caption.to_owned())
                .part("photo", part);
            return self
                .multipart_call("sendPhoto", form)
                .map(|message| vec![message]);
        }

        let mut media = Vec::with_capacity(paths.len());
        let mut form = multipart::Form::new().text("chat_id", chat_id.to_string());
        for (index, path) in paths.iter().enumerate() {
            let attachment = format!("memelith-preview-{index}");
            let mut item = json!({
                "type": "photo",
                "media": format!("attach://{attachment}"),
            });
            if index == 0 {
                item["caption"] = caption.into();
            }
            media.push(item);
            form = form.part(attachment, photo_part(path, index)?);
        }
        form = form.text("media", serde_json::to_string(&media)?);
        self.multipart_call("sendMediaGroup", form)
    }

    fn answer_callback(
        &self,
        callback_id: &str,
        text: Option<&str>,
    ) -> Result<bool, TelegramError> {
        self.call(
            "answerCallbackQuery",
            json!({"callback_query_id": callback_id, "text": text}),
        )
    }

    fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<bool, TelegramError> {
        self.call(
            "deleteMessage",
            json!({"chat_id": chat_id, "message_id": message_id}),
        )
    }

    fn multipart_call<T: DeserializeOwned>(
        &self,
        method: &str,
        form: multipart::Form,
    ) -> Result<T, TelegramError> {
        let response = self
            .client
            .post(format!("{}/{method}", self.base_url))
            .multipart(form)
            .send()?;
        let response = response.error_for_status()?;
        let body = response.json::<ApiResponse<T>>()?;
        if !body.ok {
            return Err(TelegramError::Api(
                body.description
                    .unwrap_or_else(|| "unknown Telegram error".to_owned()),
            ));
        }
        body.result.ok_or(TelegramError::MissingResult)
    }
}

fn run_bot(
    storage_root: PathBuf,
    token: String,
    model_directory: PathBuf,
    stop_receiver: mpsc::Receiver<()>,
    command_receiver: mpsc::Receiver<TelegramBotCommand>,
    status: &mpsc::Sender<TelegramBotStatus>,
) -> Result<(), TelegramError> {
    let api = TelegramApi::new(&token)?;
    api.verify_bot()?;
    if stop_receiver.try_recv().is_ok() {
        return Ok(());
    }
    let _ = status.send(TelegramBotStatus::LoadingModel);
    let model = ClipModel::load(model_directory, ExecutionPolicy::Auto)
        .map_err(|error| TelegramError::Api(format!("failed to load CLIP model: {error}")))?;
    if stop_receiver.try_recv().is_ok() {
        return Ok(());
    }
    let database = MemeDatabase::open(&storage_root, model)?;
    let state = Arc::new(BotState {
        api,
        database: Mutex::new(database),
        storage_root,
        status: status.clone(),
        stopping: AtomicBool::new(false),
        syncing_sticker_sets: Mutex::new(HashSet::new()),
        pending: Mutex::new(HashMap::new()),
        pending_deletions: Mutex::new(HashSet::new()),
        workers: Mutex::new(Vec::new()),
    });
    let _ = status.send(TelegramBotStatus::Running);

    let mut offset = 0_i64;
    let mut next_auto_sync = Instant::now();
    'poll: loop {
        if stop_receiver.try_recv().is_ok() {
            break;
        }
        reap_finished_workers(&state);
        retry_pending_deletions(&state);
        drain_commands(&state, &command_receiver)?;
        if Instant::now() >= next_auto_sync {
            schedule_automatic_sticker_syncs(&state)?;
            next_auto_sync = Instant::now() + STICKER_AUTO_SYNC_INTERVAL;
        }
        match state.api.get_updates(offset) {
            Ok(updates) => {
                for update in updates {
                    if !wait_for_worker_capacity(&state, &stop_receiver) {
                        break 'poll;
                    }
                    offset = update.update_id.saturating_add(1);
                    let worker_state = Arc::clone(&state);
                    let worker = thread::Builder::new()
                        .name("memelith-telegram-update".to_owned())
                        .spawn(move || {
                            if let Err(error) = handle_update(worker_state, update) {
                                eprintln!("Memelith Telegram update failed: {error}");
                            }
                        })
                        .map_err(|error| TelegramError::Io(std::io::Error::other(error)));
                    let worker = match worker {
                        Ok(worker) => worker,
                        Err(error) => {
                            shutdown_state(&state);
                            return Err(error);
                        }
                    };
                    let mut workers = match state.workers.lock() {
                        Ok(workers) => workers,
                        Err(_) => {
                            if worker.join().is_err() {
                                eprintln!("Memelith Telegram update worker panicked");
                            }
                            shutdown_state(&state);
                            return Err(TelegramError::Api("worker lock was poisoned".to_owned()));
                        }
                    };
                    workers.push(worker);
                }
            }
            Err(error) => {
                eprintln!("Memelith Telegram polling failed: {error}");
                if stop_receiver.recv_timeout(Duration::from_secs(1)).is_ok() {
                    break;
                }
            }
        }
    }
    shutdown_state(&state);
    Ok(())
}

fn wait_for_worker_capacity(state: &Arc<BotState>, stop_receiver: &mpsc::Receiver<()>) -> bool {
    loop {
        reap_finished_workers(state);
        let at_capacity = match state.workers.lock() {
            Ok(workers) => workers.len() >= MAX_TELEGRAM_WORKERS,
            Err(_) => {
                eprintln!("Memelith Telegram failed to acquire worker lock while throttling");
                return false;
            }
        };
        if !at_capacity {
            return true;
        }
        match stop_receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn runtime_error_message(error: &TelegramError) -> String {
    match error {
        TelegramError::Request(error) if error.is_timeout() => "Telegram 请求超时".to_owned(),
        TelegramError::Request(error) if error.is_connect() => {
            "无法连接 Telegram，请检查网络".to_owned()
        }
        TelegramError::Request(error) => match error.status() {
            Some(status) => format!("Telegram 请求失败（HTTP {status}）"),
            None => format!("Telegram 网络请求失败：{error}"),
        },
        error => error.to_string(),
    }
}

fn shutdown_state(state: &Arc<BotState>) {
    state.stopping.store(true, Ordering::Release);
    let workers = match state.workers.lock() {
        Ok(mut workers) => std::mem::take(&mut *workers),
        Err(_) => {
            eprintln!("Memelith Telegram failed to acquire worker lock during shutdown");
            Vec::new()
        }
    };
    for worker in workers {
        if worker.join().is_err() {
            eprintln!("Memelith Telegram update worker panicked during shutdown");
        }
    }

    let pending_operations = match state.pending.lock() {
        Ok(mut pending) => pending.drain().map(|(_, operation)| operation).collect(),
        Err(_) => {
            eprintln!("Memelith Telegram failed to acquire pending lock during shutdown");
            Vec::new()
        }
    };
    for operation in pending_operations {
        if operation.collector_item_id != Uuid::nil() {
            match state.database.lock() {
                Ok(mut database) => {
                    if let Err(error) = database.delete_collector_item(operation.collector_item_id)
                    {
                        eprintln!(
                            "Memelith Telegram failed to clean up reserved Collector item during shutdown: {error}"
                        );
                    }
                }
                Err(_) => {
                    eprintln!("Memelith Telegram failed to acquire database lock during shutdown")
                }
            }
        }
        remove_temp_file(&operation.source_path);
        if let Some(preview_path) = operation.preview_path {
            remove_temp_file(&preview_path);
        }
    }
}

fn reap_finished_workers(state: &Arc<BotState>) {
    let mut finished = Vec::new();
    match state.workers.lock() {
        Ok(mut workers) => {
            let mut active = Vec::with_capacity(workers.len());
            for worker in workers.drain(..) {
                if worker.is_finished() {
                    finished.push(worker);
                } else {
                    active.push(worker);
                }
            }
            *workers = active;
        }
        Err(_) => {
            eprintln!("Memelith Telegram failed to acquire worker lock while reaping");
            return;
        }
    }
    for worker in finished {
        if worker.join().is_err() {
            eprintln!("Memelith Telegram update worker panicked");
        }
    }
}

fn drain_commands(
    state: &Arc<BotState>,
    commands: &mpsc::Receiver<TelegramBotCommand>,
) -> Result<(), TelegramError> {
    while let Ok(command) = commands.try_recv() {
        match command {
            TelegramBotCommand::SyncStickerPack { pack_id, source } => {
                if let Err(error) = spawn_sticker_sync(Arc::clone(state), pack_id, source, true) {
                    eprintln!("Memelith Telegram failed to schedule sticker sync: {error}");
                }
            }
        }
    }
    Ok(())
}

fn schedule_automatic_sticker_syncs(state: &Arc<BotState>) -> Result<(), TelegramError> {
    let sources = {
        let database = state
            .database
            .lock()
            .map_err(|_| TelegramError::Api("database lock was poisoned".to_owned()))?;
        database
            .list_meme_packs()?
            .into_iter()
            .filter_map(|pack| {
                let source = pack.source?;
                parse_sticker_pack_reference(&source)
                    .ok()
                    .map(|_| (pack.id, source))
            })
            .collect::<Vec<_>>()
    };
    for (pack_id, source) in sources {
        if let Err(error) = spawn_sticker_sync(Arc::clone(state), pack_id, source, false) {
            eprintln!("Memelith Telegram failed to schedule automatic sticker sync: {error}");
        }
    }
    Ok(())
}

fn spawn_sticker_sync(
    state: Arc<BotState>,
    pack_id: Uuid,
    source: String,
    announce: bool,
) -> Result<(), TelegramError> {
    let pack_name = parse_sticker_pack_reference(&source)?;
    let sync_key = sticker_pack_sync_key(&pack_name);
    {
        let mut syncing = state
            .syncing_sticker_sets
            .lock()
            .map_err(|_| TelegramError::Api("sticker sync lock was poisoned".to_owned()))?;
        if !syncing.insert(sync_key.clone()) {
            if announce {
                let _ = state.status.send(TelegramBotStatus::StickerPackSyncFailed {
                    pack_id,
                    message: "该贴纸包正在检查更新".to_owned(),
                    announce: true,
                    terminal: false,
                });
            }
            return Ok(());
        }
    }
    let _ = state
        .status
        .send(TelegramBotStatus::StickerPackSyncStarted { pack_id });
    let worker_state = Arc::clone(&state);
    let worker = match thread::Builder::new()
        .name("memelith-telegram-sticker-sync".to_owned())
        .spawn(move || {
            let result = sync_sticker_pack(&worker_state, pack_id, &pack_name);
            if let Ok(mut syncing) = worker_state.syncing_sticker_sets.lock() {
                syncing.remove(&sync_key);
            }
            match result {
                Ok((title, added, skipped)) => {
                    let _ = worker_state
                        .status
                        .send(TelegramBotStatus::StickerPackSynced {
                            pack_id,
                            title,
                            added,
                            skipped,
                            announce,
                        });
                }
                Err(error) => {
                    eprintln!("Memelith Telegram sticker sync failed: {error}");
                    let _ = worker_state
                        .status
                        .send(TelegramBotStatus::StickerPackSyncFailed {
                            pack_id,
                            message: error.to_string(),
                            announce,
                            terminal: true,
                        });
                }
            }
        }) {
        Ok(worker) => worker,
        Err(error) => {
            state
                .syncing_sticker_sets
                .lock()
                .map_err(|_| TelegramError::Api("sticker sync lock was poisoned".to_owned()))?
                .remove(&sticker_pack_sync_key(&parse_sticker_pack_reference(
                    &source,
                )?));
            let message = error.to_string();
            let _ = state.status.send(TelegramBotStatus::StickerPackSyncFailed {
                pack_id,
                message: message.clone(),
                announce,
                terminal: true,
            });
            return Err(TelegramError::Io(std::io::Error::other(message)));
        }
    };
    state
        .workers
        .lock()
        .map_err(|_| TelegramError::Api("worker lock was poisoned".to_owned()))?
        .push(worker);
    Ok(())
}

fn sync_sticker_pack(
    state: &Arc<BotState>,
    pack_id: Uuid,
    pack_name: &str,
) -> Result<(String, usize, usize), TelegramError> {
    let sticker_set = state.api.get_sticker_set(pack_name)?;
    sync_sticker_set(state, pack_id, sticker_set)
}

fn sync_sticker_set(
    state: &Arc<BotState>,
    pack_id: Uuid,
    sticker_set: StickerSet,
) -> Result<(String, usize, usize), TelegramError> {
    let mut added = 0;
    let mut skipped = 0;
    let import_directory = state.storage_root.join(".telegram/stickers");
    fs::create_dir_all(&import_directory)?;
    for sticker in sticker_set.stickers {
        if state.stopping.load(Ordering::Acquire) {
            return Err(TelegramError::Stopping);
        }
        let file_id = &sticker.file_id;
        let file = state.api.get_file(file_id)?;
        let file_path = file.file_path.ok_or(TelegramError::MissingFilePath)?;
        let extension = Path::new(&file_path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("webp");
        if !is_safe_sticker_file_identifier(&sticker.file_unique_id) {
            return Err(TelegramError::InvalidStickerFileIdentifier);
        }
        let source_path = import_directory.join(format!("{}.{extension}", sticker.file_unique_id));
        if !source_path.is_file() {
            let bytes = state.api.download_file(&file_path)?;
            fs::write(&source_path, bytes)?;
        }
        let mut database = state
            .database
            .lock()
            .map_err(|_| TelegramError::Api("database lock was poisoned".to_owned()))?;
        if database
            .find_meme_with_media_in_pack(pack_id, &source_path)?
            .is_some()
        {
            skipped += 1;
            continue;
        }
        let new_content = if sticker.is_animated || sticker.is_video {
            let preview = sticker
                .thumbnail
                .as_ref()
                .map(|thumbnail| -> Result<_, TelegramError> {
                    let preview_file = state.api.get_file(&thumbnail.file_id)?;
                    let preview_path = preview_file
                        .file_path
                        .ok_or(TelegramError::MissingFilePath)?;
                    let preview_extension = Path::new(&preview_path)
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or("jpg");
                    let preview_path_on_disk = import_directory.join(format!(
                        "{}.preview.{preview_extension}",
                        sticker.file_unique_id
                    ));
                    if !preview_path_on_disk.is_file() {
                        fs::write(
                            &preview_path_on_disk,
                            state.api.download_file(&preview_path)?,
                        )?;
                    }
                    Ok((preview_path_on_disk, thumbnail.width, thumbnail.height))
                })
                .transpose()?;
            let preview_path = preview.map(|(path, _, _)| path);
            NewMemeContent::Motion {
                source_path: source_path.clone(),
                preview_path,
                width: sticker.width,
                height: sticker.height,
                format: if sticker.is_animated {
                    MotionFormat::Tgs
                } else {
                    MotionFormat::WebM
                },
            }
        } else {
            NewMemeContent::Image {
                source_path: source_path.clone(),
            }
        };
        database.create_meme(
            pack_id,
            NewMeme {
                name: sticker.emoji.filter(|emoji| !emoji.trim().is_empty()),
                description: None,
                contents: vec![new_content],
            },
        )?;
        added += 1;
    }
    Ok((sticker_set.title, added, skipped))
}

fn parse_sticker_pack_reference(source: &str) -> Result<String, TelegramError> {
    let source = source.trim();
    let name = source
        .strip_prefix(TELEGRAM_STICKER_LINK_PREFIX)
        .or_else(|| source.strip_prefix("http://t.me/addstickers/"))
        .or_else(|| source.strip_prefix("https://telegram.me/addstickers/"))
        .or_else(|| source.strip_prefix("http://telegram.me/addstickers/"))
        .ok_or(TelegramError::InvalidStickerPackReference)?
        .trim_matches('/');
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(TelegramError::InvalidStickerPackReference);
    }
    Ok(name.to_owned())
}

fn sticker_pack_sync_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn is_safe_sticker_file_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

pub fn is_sticker_pack_source(source: &str) -> bool {
    parse_sticker_pack_reference(source).is_ok()
}

fn handle_update(state: Arc<BotState>, update: Update) -> Result<(), TelegramError> {
    if let Some(callback) = update.callback_query {
        return handle_callback(state, callback);
    }
    if let Some(message) = update.message {
        let sticker_reference = message
            .sticker
            .as_ref()
            .and_then(|sticker| sticker.set_name.as_deref())
            .map(|name| StickerPackReference {
                name: name.to_owned(),
                source: format!("{TELEGRAM_STICKER_LINK_PREFIX}{name}"),
            });
        let text_reference = message
            .text
            .as_deref()
            .or(message.caption.as_deref())
            .and_then(find_sticker_pack_reference);
        if let Some(reference) = sticker_reference.or(text_reference) {
            let chat_id = message.chat.id;
            let message_id = message.message_id;
            match import_sticker_pack(&state, &reference) {
                Ok((pack_id, title, added, skipped)) => {
                    let _ = state.status.send(TelegramBotStatus::StickerPackSynced {
                        pack_id,
                        title: title.clone(),
                        added,
                        skipped,
                        announce: false,
                    });
                    delete_message_or_queue(&state, chat_id, message_id)?;
                    state.api.send_message(
                        chat_id,
                        &format!("已同步贴纸包「{title}」：新增 {added} 张，跳过 {skipped} 张。"),
                    )?;
                    return Ok(());
                }
                Err(error) => {
                    log_api_result(
                        "send sticker pack failure message",
                        state
                            .api
                            .send_message(chat_id, &format!("贴纸包处理失败：{error}")),
                    );
                    return Err(error);
                }
            }
        }
        let chat_id = message.chat.id;
        let incoming = if let Some(photo) = largest_photo(&message.photo) {
            Some(IncomingTelegramMedia::Image {
                file_id: photo.file_id,
            })
        } else if let Some(animation) = &message.animation {
            Some(IncomingTelegramMedia::Animation {
                file_id: animation.file_id.clone(),
                thumbnail_file_id: animation
                    .thumbnail
                    .as_ref()
                    .map(|thumbnail| thumbnail.file_id.clone()),
                width: animation.width,
                height: animation.height,
            })
        } else {
            message
                .video
                .as_ref()
                .map(|video| IncomingTelegramMedia::Video {
                    file_id: video.file_id.clone(),
                    thumbnail_file_id: video
                        .thumbnail
                        .as_ref()
                        .map(|thumbnail| thumbnail.file_id.clone()),
                    width: video.width,
                    height: video.height,
                })
        };
        let Some(incoming) = incoming else {
            return Ok(());
        };
        if let Err(error) = handle_media(Arc::clone(&state), message, incoming) {
            log_api_result(
                "send media failure message",
                state
                    .api
                    .send_message(chat_id, &format!("图片处理失败：{error}")),
            );
            return Err(error);
        }
    }
    Ok(())
}

fn import_sticker_pack(
    state: &Arc<BotState>,
    reference: &StickerPackReference,
) -> Result<(Uuid, String, usize, usize), TelegramError> {
    let sync_key = sticker_pack_sync_key(&reference.name);
    {
        let mut syncing = state
            .syncing_sticker_sets
            .lock()
            .map_err(|_| TelegramError::Api("sticker sync lock was poisoned".to_owned()))?;
        if !syncing.insert(sync_key.clone()) {
            return Err(TelegramError::StickerPackAlreadySyncing(
                reference.name.clone(),
            ));
        }
    }
    let mut active_pack_id = None;
    let result: Result<(Uuid, String, usize, usize), TelegramError> = (|| {
        let sticker_set = state.api.get_sticker_set(&reference.name)?;
        let pack_id = {
            let mut database = state
                .database
                .lock()
                .map_err(|_| TelegramError::Api("database lock was poisoned".to_owned()))?;
            let existing = database.list_meme_packs()?.into_iter().find(|pack| {
                pack.source
                    .as_deref()
                    .and_then(|source| parse_sticker_pack_reference(source).ok())
                    .is_some_and(|name| name.eq_ignore_ascii_case(&sticker_set.name))
            });
            match existing {
                Some(pack) => pack.id,
                None => {
                    database
                        .create_meme_pack(NewMemePack {
                            name: sticker_set.title.clone(),
                            description: Some(
                                "从 Telegram 贴纸包自动同步；动画与视频贴纸保存为预览图".to_owned(),
                            ),
                            author: None,
                            source: Some(reference.source.clone()),
                        })?
                        .id
                }
            }
        };
        active_pack_id = Some(pack_id);
        let _ = state
            .status
            .send(TelegramBotStatus::StickerPackSyncStarted { pack_id });
        let (title, added, skipped) = sync_sticker_set(state, pack_id, sticker_set)?;
        Ok((pack_id, title, added, skipped))
    })();
    state
        .syncing_sticker_sets
        .lock()
        .map_err(|_| TelegramError::Api("sticker sync lock was poisoned".to_owned()))?
        .remove(&sync_key);
    if let (Some(pack_id), Err(error)) = (active_pack_id, &result) {
        let _ = state.status.send(TelegramBotStatus::StickerPackSyncFailed {
            pack_id,
            message: error.to_string(),
            announce: false,
            terminal: true,
        });
    }
    result
}

struct StickerPackReference {
    name: String,
    source: String,
}

fn find_sticker_pack_reference(text: &str) -> Option<StickerPackReference> {
    text.split_whitespace().find_map(|part| {
        let source = part.trim_matches(|character: char| {
            matches!(
                character,
                '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '。'
            )
        });
        parse_sticker_pack_reference(source)
            .ok()
            .map(|name| StickerPackReference {
                name,
                source: source.to_owned(),
            })
    })
}

enum IncomingTelegramMedia {
    Image {
        file_id: String,
    },
    Animation {
        file_id: String,
        thumbnail_file_id: Option<String>,
        width: u32,
        height: u32,
    },
    Video {
        file_id: String,
        thumbnail_file_id: Option<String>,
        width: u32,
        height: u32,
    },
}

fn handle_media(
    state: Arc<BotState>,
    message: Message,
    incoming: IncomingTelegramMedia,
) -> Result<(), TelegramError> {
    let sender = message.from.as_ref().ok_or(TelegramError::MissingSender)?;
    let (file_id, thumbnail_file_id, dimensions, animation) = match incoming {
        IncomingTelegramMedia::Image { file_id } => (file_id, None, None, false),
        IncomingTelegramMedia::Animation {
            file_id,
            thumbnail_file_id,
            width,
            height,
        } => (file_id, thumbnail_file_id, Some((width, height)), true),
        IncomingTelegramMedia::Video {
            file_id,
            thumbnail_file_id,
            width,
            height,
        } => (file_id, thumbnail_file_id, Some((width, height)), false),
    };
    let file = state.api.get_file(&file_id)?;
    let file_path = file.file_path.ok_or(TelegramError::MissingFilePath)?;
    let bytes = state.api.download_file(&file_path)?;
    let incoming_directory = state.storage_root.join(".telegram/incoming");
    fs::create_dir_all(&incoming_directory)?;
    let extension = Path::new(&file_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or(if dimensions.is_some() { "mp4" } else { "image" });
    let source_path = incoming_directory.join(format!("{}.{extension}", Uuid::new_v4()));
    let mut source_file = TempFileGuard::new(source_path);
    fs::write(source_file.path(), bytes)?;

    let media = match dimensions {
        None => IncomingMedia::Image,
        Some(_) if extension.eq_ignore_ascii_case("gif") => IncomingMedia::Image,
        Some((width, height)) => IncomingMedia::Motion {
            width,
            height,
            format: if extension.eq_ignore_ascii_case("webm") {
                MotionFormat::WebM
            } else if animation || extension.eq_ignore_ascii_case("mp4") {
                MotionFormat::Mp4
            } else {
                return Err(TelegramError::Api(format!(
                    "不支持 Telegram 媒体格式：{extension}"
                )));
            },
        },
    };
    let mut preview_file = thumbnail_file_id
        .map(|thumbnail_file_id| -> Result<_, TelegramError> {
            let thumbnail = state.api.get_file(&thumbnail_file_id)?;
            let thumbnail_path = thumbnail.file_path.ok_or(TelegramError::MissingFilePath)?;
            let preview_path = incoming_directory.join(format!("{}.preview", Uuid::new_v4()));
            let preview_file = TempFileGuard::new(preview_path);
            fs::write(
                preview_file.path(),
                state.api.download_file(&thumbnail_path)?,
            )?;
            Ok(preview_file)
        })
        .transpose()?;
    let preview_path = match &media {
        IncomingMedia::Image => Some(source_file.path()),
        IncomingMedia::Motion { .. } => preview_file.as_ref().map(TempFileGuard::path),
    };

    let duplicate_result = {
        let mut database = state
            .database
            .lock()
            .map_err(|_| TelegramError::Api("database lock was poisoned".to_owned()))?;
        database.find_media_duplicates(
            source_file.path(),
            preview_path,
            COLLECTOR_DUPLICATE_MAX_COSINE_DISTANCE,
        )
    };
    let duplicates = duplicate_result?;

    if duplicates.is_empty() {
        let mut database = state
            .database
            .lock()
            .map_err(|_| TelegramError::Api("database lock was poisoned".to_owned()))?;
        collect_incoming_media(&mut database, &media, source_file.path(), preview_path)?;
        drop(database);
        let _ = state.status.send(TelegramBotStatus::CollectorUpdated);
        if !delete_message_or_queue(&state, message.chat.id, message.message_id)? {
            log_api_result(
                "send source deletion warning",
                state.api.send_message(
                    message.chat.id,
                    "图片已添加，但原消息暂时无法删除，机器人会自动重试。",
                ),
            );
        }
        return Ok(());
    }

    let similar_paths = match duplicates
        .iter()
        .filter_map(|duplicate| duplicate.preview_relative_path.as_ref())
        .map(|relative_path| {
            MemeDatabase::resolve_media_path_from_root(&state.storage_root, relative_path)
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(paths) => paths,
        Err(error) => {
            return Err(error.into());
        }
    };
    let operation_id = Uuid::new_v4();
    let pending = PendingOperation {
        id: operation_id,
        chat_id: message.chat.id,
        confirmation_message_id: 0,
        media_message_ids: Vec::new(),
        user_id: sender.id,
        source_path: source_file.path().to_owned(),
        preview_path: preview_file
            .as_ref()
            .map(|preview_file| preview_file.path().to_path_buf()),
        media,
        collector_item_id: Uuid::nil(),
        similar_paths,
        duplicate_count: duplicates.len(),
        page: 0,
    };
    let mut pending = pending;
    // Reserve the Collector row before sending Telegram's confirmation so later
    // uploads cannot acquire a newer rowid and overtake this operation.
    let reservation_result = match state.database.lock() {
        Ok(mut database) => collect_incoming_media(
            &mut database,
            &pending.media,
            &pending.source_path,
            pending.preview_path.as_deref(),
        ),
        Err(_) => Err(memelith_core::Error::InvalidDatabase(
            "database lock was poisoned".to_owned(),
        )),
    };
    let collector_item_id = match reservation_result {
        Ok(item) => item.id,
        Err(error) => {
            delete_confirmation_messages(&state, &pending)?;
            remove_temp_file(&pending.source_path);
            if let Some(preview_path) = &pending.preview_path {
                remove_temp_file(preview_path);
            }
            return Err(error.into());
        }
    };
    pending.collector_item_id = collector_item_id;
    let sent = match send_pending_confirmation(&state, &pending) {
        Ok(sent) => sent,
        Err(error) => {
            if let Ok(mut database) = state.database.lock()
                && let Err(delete_error) = database.delete_collector_item(collector_item_id)
            {
                eprintln!(
                    "Memelith Telegram failed to clean up reserved Collector item: {delete_error}"
                );
            }
            remove_temp_file(&pending.source_path);
            if let Some(preview_path) = &pending.preview_path {
                remove_temp_file(preview_path);
            }
            return Err(error);
        }
    };
    pending.confirmation_message_id = sent.confirmation_message_id;
    pending.media_message_ids = sent.media_message_ids;
    let pending_result = state
        .pending
        .lock()
        .map_err(|_| TelegramError::Api("pending operation lock was poisoned".to_owned()));
    let mut operations = match pending_result {
        Ok(operations) => operations,
        Err(error) => {
            if let Err(delete_error) = delete_confirmation_messages(&state, &pending) {
                eprintln!(
                    "Memelith Telegram failed to clean up orphaned confirmation messages: {delete_error}"
                );
            }
            if let Ok(mut database) = state.database.lock()
                && let Err(delete_error) = database.delete_collector_item(collector_item_id)
            {
                eprintln!(
                    "Memelith Telegram failed to clean up reserved Collector item: {delete_error}"
                );
            }
            return Err(error);
        }
    };
    operations.insert(operation_id, pending);
    drop(operations);
    source_file.disarm();
    if let Some(preview_file) = preview_file.as_mut() {
        preview_file.disarm();
    }
    delete_message_or_queue(&state, message.chat.id, message.message_id)?;
    Ok(())
}

fn handle_callback(state: Arc<BotState>, callback: CallbackQuery) -> Result<(), TelegramError> {
    let data = callback
        .data
        .as_deref()
        .ok_or(TelegramError::InvalidCallback)?;
    let (operation_id, action, page) = parse_callback(data)?;

    if action != "page" && action != "keep" && action != "discard" {
        return Err(TelegramError::InvalidCallback);
    }
    if action != "page" && page.is_some() {
        return Err(TelegramError::InvalidCallback);
    }

    let callback_message = match callback.message.as_ref() {
        Some(message) => message,
        None => {
            log_api_result(
                "answer callback query without message",
                state
                    .api
                    .answer_callback(&callback.id, Some("此按钮已失效。")),
            );
            return Err(TelegramError::UnauthorizedCallback);
        }
    };
    let pending_snapshot = {
        let operations = state
            .pending
            .lock()
            .map_err(|_| TelegramError::Api("pending operation lock was poisoned".to_owned()))?;
        operations
            .get(&operation_id)
            .cloned()
            .ok_or(TelegramError::InvalidCallback)?
    };
    if callback.from.id != pending_snapshot.user_id
        || callback_message.chat.id != pending_snapshot.chat_id
        || callback_message.message_id != pending_snapshot.confirmation_message_id
    {
        log_api_result(
            "answer unauthorized callback query",
            state
                .api
                .answer_callback(&callback.id, Some("此按钮不属于你的操作。")),
        );
        return Err(TelegramError::UnauthorizedCallback);
    }
    log_api_result(
        "answer callback query",
        state.api.answer_callback(&callback.id, None),
    );

    if action == "page" {
        let mut next = pending_snapshot.clone();
        let page = page.ok_or(TelegramError::InvalidCallback)?;
        next.page = page.min(page_count(next.similar_paths.len()).saturating_sub(1));
        next.confirmation_message_id = 0;
        next.media_message_ids.clear();
        let sent = send_pending_confirmation(&state, &next)?;
        next.confirmation_message_id = sent.confirmation_message_id;
        next.media_message_ids = sent.media_message_ids;

        let replaced = match (|| {
            let mut operations = state.pending.lock().map_err(|_| {
                TelegramError::Api("pending operation lock was poisoned".to_owned())
            })?;
            let Some(operation) = operations.get_mut(&operation_id) else {
                return Ok(false);
            };
            if operation.confirmation_message_id != pending_snapshot.confirmation_message_id {
                Ok(false)
            } else {
                *operation = next.clone();
                Ok(true)
            }
        })() {
            Ok(replaced) => replaced,
            Err(error) => {
                if let Err(delete_error) = delete_confirmation_messages(&state, &next) {
                    eprintln!(
                        "Memelith Telegram failed to clean up replacement confirmation: {delete_error}"
                    );
                }
                return Err(error);
            }
        };
        if !replaced {
            delete_confirmation_messages(&state, &next)?;
            return Err(TelegramError::InvalidCallback);
        }
        delete_confirmation_messages(&state, &pending_snapshot)?;
        return Ok(());
    }

    let pending = state
        .pending
        .lock()
        .map_err(|_| TelegramError::Api("pending operation lock was poisoned".to_owned()))?
        .remove(&operation_id)
        .ok_or(TelegramError::InvalidCallback)?;
    if action == "keep" {
        let _ = state.status.send(TelegramBotStatus::CollectorUpdated);
    } else if action != "discard" {
        return Err(TelegramError::InvalidCallback);
    } else {
        let delete_result = match state.database.lock() {
            Ok(mut database) => database.delete_collector_item(pending.collector_item_id),
            Err(_) => Err(memelith_core::Error::InvalidDatabase(
                "database lock was poisoned".to_owned(),
            )),
        };
        if let Err(error) = delete_result {
            let restore_result = state
                .pending
                .lock()
                .map(|mut operations| operations.insert(operation_id, pending.clone()))
                .map_err(|_| TelegramError::Api("pending operation lock was poisoned".to_owned()));
            if let Err(restore_error) = restore_result {
                eprintln!(
                    "Memelith Telegram failed to restore pending operation after discard failure: {restore_error}"
                );
            }
            log_api_result(
                "send Collector discard failure message",
                state
                    .api
                    .send_message(pending.chat_id, &format!("放弃添加失败：{error}")),
            );
            return Err(error.into());
        }
        let _ = state.status.send(TelegramBotStatus::CollectorUpdated);
    }
    remove_temp_file(&pending.source_path);
    if let Some(preview_path) = &pending.preview_path {
        remove_temp_file(preview_path);
    }
    delete_confirmation_messages(&state, &pending)?;
    Ok(())
}

fn parse_callback(data: &str) -> Result<(Uuid, &str, Option<usize>), TelegramError> {
    let mut fields = data.split(':');
    if fields.next() != Some("memelith") {
        return Err(TelegramError::InvalidCallback);
    }
    let operation_id = fields
        .next()
        .ok_or(TelegramError::InvalidCallback)?
        .parse()
        .map_err(|_| TelegramError::InvalidCallback)?;
    let action = fields.next().ok_or(TelegramError::InvalidCallback)?;
    let page = fields
        .next()
        .map(|value| value.parse().map_err(|_| TelegramError::InvalidCallback))
        .transpose()?;
    if fields.next().is_some() {
        return Err(TelegramError::InvalidCallback);
    }
    Ok((operation_id, action, page))
}

fn collect_incoming_media(
    database: &mut MemeDatabase,
    media: &IncomingMedia,
    source_path: &Path,
    preview_path: Option<&Path>,
) -> memelith_core::Result<memelith_core::CollectorItem> {
    match media {
        IncomingMedia::Image => database.collect_image(source_path),
        IncomingMedia::Motion {
            width,
            height,
            format,
        } => database.collect_motion(
            source_path,
            preview_path.map(Path::to_path_buf),
            *width,
            *height,
            *format,
        ),
    }
}

struct SentPendingConfirmation {
    confirmation_message_id: i64,
    media_message_ids: Vec<i64>,
}

fn send_pending_confirmation(
    state: &BotState,
    operation: &PendingOperation,
) -> Result<SentPendingConfirmation, TelegramError> {
    let media_messages = state.api.send_preview_media(
        operation.chat_id,
        &preview_paths(operation),
        &preview_caption(operation),
    )?;
    let media_message_ids = media_messages
        .iter()
        .map(|message| message.message_id)
        .collect::<Vec<_>>();
    let reply_to_message_id = media_message_ids.first().copied();
    let confirmation = match state.api.send_confirmation(
        operation.chat_id,
        &confirmation_text(operation),
        &preview_markup(operation),
        reply_to_message_id,
    ) {
        Ok(message) => message,
        Err(error) => {
            if let Err(delete_error) =
                delete_messages_or_queue(state, operation.chat_id, &media_message_ids)
            {
                eprintln!(
                    "Memelith Telegram failed to clean up confirmation media: {delete_error}"
                );
            }
            return Err(error);
        }
    };
    Ok(SentPendingConfirmation {
        confirmation_message_id: confirmation.message_id,
        media_message_ids,
    })
}

fn preview_paths(operation: &PendingOperation) -> Vec<PathBuf> {
    let start = operation.page.saturating_mul(MAX_SIMILAR_PER_PAGE);
    let end = (start + MAX_SIMILAR_PER_PAGE).min(operation.similar_paths.len());
    let mut paths = Vec::with_capacity(end.saturating_sub(start) + 1);
    if let Some(preview_path) = operation.preview_path() {
        paths.push(preview_path.to_path_buf());
    }
    paths.extend(operation.similar_paths[start..end].iter().cloned());
    paths
}

fn page_count(similar_count: usize) -> usize {
    similar_count.div_ceil(MAX_SIMILAR_PER_PAGE).max(1)
}

fn preview_caption(operation: &PendingOperation) -> String {
    let page = operation.page + 1;
    let pages = page_count(operation.similar_paths.len());
    if operation.preview_path().is_some() {
        format!("第 1 张为待添加内容，其余为相似内容（第 {page} / {pages} 页）。")
    } else {
        format!("当前内容无法预览，以下为检测到的相似内容（第 {page} / {pages} 页）。")
    }
}

fn confirmation_text(operation: &PendingOperation) -> String {
    format!(
        "检测到 {} 张相似图片。第 {} / {} 页，请选择是否仍然添加到 Collector。",
        operation.duplicate_count,
        operation.page + 1,
        page_count(operation.similar_paths.len())
    )
}

fn preview_markup(operation: &PendingOperation) -> InlineKeyboardMarkup {
    let pages = page_count(operation.similar_paths.len());
    let mut rows = Vec::new();
    if pages > 1 {
        let mut navigation = Vec::new();
        if operation.page > 0 {
            navigation.push(InlineKeyboardButton {
                text: "上一页".to_owned(),
                callback_data: format!("memelith:{}:page:{}", operation.id, operation.page - 1),
            });
        }
        if operation.page + 1 < pages {
            navigation.push(InlineKeyboardButton {
                text: "下一页".to_owned(),
                callback_data: format!("memelith:{}:page:{}", operation.id, operation.page + 1),
            });
        }
        rows.push(navigation);
    }
    rows.push(vec![
        InlineKeyboardButton {
            text: "仍然添加".to_owned(),
            callback_data: format!("memelith:{}:keep", operation.id),
        },
        InlineKeyboardButton {
            text: "放弃".to_owned(),
            callback_data: format!("memelith:{}:discard", operation.id),
        },
    ]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

fn largest_photo(photos: &[PhotoSize]) -> Option<PhotoSize> {
    photos.iter().cloned().max_by_key(|photo| {
        (
            u64::from(photo.width).saturating_mul(u64::from(photo.height)),
            photo.file_size.unwrap_or_default(),
        )
    })
}

fn log_api_result<T>(operation: &str, result: Result<T, TelegramError>) {
    if let Err(error) = result {
        eprintln!("Memelith Telegram failed to {operation}: {error}");
    }
}

fn photo_part(path: &Path, index: usize) -> Result<multipart::Part, TelegramError> {
    let bytes = fs::read(path)?;
    let format = image::guess_format(&bytes)?;
    let extension = format.extensions_str().first().copied().unwrap_or("image");
    Ok(multipart::Part::bytes(bytes)
        .file_name(format!("memelith-preview-{index}.{extension}"))
        .mime_str(format.to_mime_type())?)
}

fn delete_confirmation_messages(
    state: &BotState,
    operation: &PendingOperation,
) -> Result<(), TelegramError> {
    if operation.confirmation_message_id != 0 {
        delete_message_or_queue(state, operation.chat_id, operation.confirmation_message_id)?;
    }
    delete_messages_or_queue(state, operation.chat_id, &operation.media_message_ids)
}

fn delete_messages_or_queue(
    state: &BotState,
    chat_id: i64,
    message_ids: &[i64],
) -> Result<(), TelegramError> {
    for &message_id in message_ids {
        delete_message_or_queue(state, chat_id, message_id)?;
    }
    Ok(())
}

fn delete_message_or_queue(
    state: &BotState,
    chat_id: i64,
    message_id: i64,
) -> Result<bool, TelegramError> {
    match state.api.delete_message(chat_id, message_id) {
        Ok(true) => Ok(true),
        Ok(false) => {
            queue_message_deletion(state, chat_id, message_id)?;
            eprintln!("Memelith Telegram deleteMessage returned false for {chat_id}:{message_id}");
            Ok(false)
        }
        Err(error) => {
            queue_message_deletion(state, chat_id, message_id)?;
            eprintln!("Memelith Telegram failed to delete {chat_id}:{message_id}: {error}");
            Ok(false)
        }
    }
}

fn queue_message_deletion(
    state: &BotState,
    chat_id: i64,
    message_id: i64,
) -> Result<(), TelegramError> {
    state
        .pending_deletions
        .lock()
        .map_err(|_| TelegramError::Api("deletion queue lock was poisoned".to_owned()))?
        .insert((chat_id, message_id));
    Ok(())
}

fn retry_pending_deletions(state: &BotState) {
    let pending = match state.pending_deletions.lock() {
        Ok(mut pending) => std::mem::take(&mut *pending),
        Err(_) => {
            eprintln!("Memelith Telegram failed to acquire deletion queue lock");
            return;
        }
    };
    for (chat_id, message_id) in pending {
        match state.api.delete_message(chat_id, message_id) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) = queue_message_deletion(state, chat_id, message_id) {
                    eprintln!("Memelith Telegram failed to requeue deletion: {error}");
                }
            }
            Err(error) => {
                eprintln!(
                    "Memelith Telegram deletion retry failed for {chat_id}:{message_id}: {error}"
                );
                if let Err(queue_error) = queue_message_deletion(state, chat_id, message_id) {
                    eprintln!("Memelith Telegram failed to requeue deletion: {queue_error}");
                }
            }
        }
    }
}

fn remove_temp_file(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "Memelith Telegram failed to remove {}: {error}",
            path.display()
        );
    }
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            remove_temp_file(&self.path);
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Clone, Debug, Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<PhotoSize>,
    #[serde(default)]
    animation: Option<Animation>,
    #[serde(default)]
    video: Option<Video>,
    #[serde(default)]
    sticker: Option<Sticker>,
}

#[derive(Clone, Debug, Deserialize)]
struct User {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct PhotoSize {
    file_id: String,
    width: u32,
    height: u32,
    file_size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct Animation {
    file_id: String,
    width: u32,
    height: u32,
    #[serde(default)]
    thumbnail: Option<PhotoSize>,
}

#[derive(Clone, Debug, Deserialize)]
struct Video {
    file_id: String,
    width: u32,
    height: u32,
    #[serde(default)]
    thumbnail: Option<PhotoSize>,
}

#[derive(Clone, Debug, Deserialize)]
struct Sticker {
    file_id: String,
    file_unique_id: String,
    width: u32,
    height: u32,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    set_name: Option<String>,
    #[serde(default)]
    is_animated: bool,
    #[serde(default)]
    is_video: bool,
    #[serde(default)]
    thumbnail: Option<PhotoSize>,
}

#[derive(Debug, Deserialize)]
struct StickerSet {
    name: String,
    title: String,
    stickers: Vec<Sticker>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    from: User,
    message: Option<Message>,
    data: Option<String>,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardButton {
    text: String,
    callback_data: String,
}
