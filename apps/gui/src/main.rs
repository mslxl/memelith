mod input;
mod model_download;
mod search;
mod settings;
mod telegram;

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, Context, Corner, Div, ElementId, Entity,
    Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent, ObjectFit, PathPromptOptions,
    Pixels, Point, PromptButton, PromptLevel, SharedString, Stateful, Window,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowOptions, actions, anchored,
    deferred, div, hsla, img, linear_color_stop, linear_gradient, point, prelude::*, px, relative,
    rgb, rgba, size,
};
use input::{TextChanged, TextInput};
use memelith_clip::{ClipModel, ExecutionPolicy};
use memelith_core::{
    APPLICATION_NAME, CollectorContent, CollectorDuplicate, CollectorItem, Meme, MemeContent,
    MemeDatabase, MemePack, NewMeme, NewMemeContent, NewMemeFromCollector, NewMemePack, NewTag,
    SimilarMemeImage, Tag,
};
use thiserror::Error;
use uuid::Uuid;
use waifu_sensor::{
    BuiltinAssets as WaifuBuiltinAssets, CharacterMatch, ExecutionPolicy as WaifuExecutionPolicy,
    MlDanbooruTagger, ModelManager as WaifuModelManager, WaifuSensor,
};

const INBOX_NAME: &str = "Inbox";
const DUPLICATE_IMAGE_MAX_COSINE_DISTANCE: f32 = 0.05;
const MAX_TAG_SUGGESTIONS: usize = 6;
const WAIFU_SENSOR_DATABASE_FILENAME: &str = "waifu-sensor.sqlite3";

actions!(
    memelith,
    [
        FocusNext,
        FocusPrevious,
        ConfirmTextContent,
        CancelTextComposer
    ]
);

// Apple 系统色板（浅色外观）
const ACCENT: u32 = 0x007aff; // systemBlue
const ACCENT_HOVER: u32 = 0x0070e8;
const ACCENT_PRESS: u32 = 0x0063cc;
const SUCCESS: u32 = 0x34c759; // systemGreen
const WARNING: u32 = 0xff9500; // systemOrange
const DANGER: u32 = 0xff3b30; // systemRed
const INK: u32 = 0x1d1d1f; // label
const LABEL_2: u32 = 0x3c3c4399; // secondaryLabel
const LABEL_3: u32 = 0x3c3c4366; // tertiaryLabel
const SEP: u32 = 0x3c3c431f; // 分组内的发丝分隔线

// 液态玻璃材质：根层完全透出窗口模糊底；侧边栏是低不透明度的活性玻璃，
// 内容面板近白高亮，卡片几乎不透明——亮度层级避免白色互叠成灰
const SIDEBAR_GLASS: u32 = 0xffffff8c;
const CONTENT_BG: u32 = 0xfbfafde8;
const GLASS_CARD: u32 = 0xfffffff2; // 分组 / 卡片
const GLASS_STRONG: u32 = 0xffffffff; // 选中 / hover 提亮
const EDGE_DARK: u32 = 0x00000010; // 卡片与控件的定义边

fn card_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0., 0., 0., 0.06),
            offset: point(px(0.), px(2.)),
            blur_radius: px(8.),
            spread_radius: px(-1.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.04),
            offset: point(px(0.), px(0.5)),
            blur_radius: px(1.),
            spread_radius: px(0.),
        },
    ]
}

fn control_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(0., 0., 0., 0.10),
        offset: point(px(0.), px(1.)),
        blur_radius: px(2.5),
        spread_radius: px(0.),
    }]
}

fn accent_fill() -> gpui::Background {
    linear_gradient(
        180.,
        linear_color_stop(rgb(0x2b8fff), 0.),
        linear_color_stop(rgb(ACCENT), 1.),
    )
}

/// 玻璃卡片：亮白浮层 + 极浅定义边 + 轻投影
fn glass_card() -> Div {
    div()
        .rounded(px(14.))
        .bg(rgba(GLASS_CARD))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .shadow(card_shadow())
}

/// 分组列表容器，对应 macOS 设置里的 inset group
fn glass_group() -> Div {
    glass_card().rounded(px(12.)).overflow_hidden()
}

fn group_row() -> Div {
    div()
        .min_h(px(44.))
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .gap_4()
}

fn hairline() -> Div {
    div().h(px(1.)).ml_4().bg(rgba(SEP))
}

fn section_header(label: impl Into<SharedString>) -> Div {
    div()
        .px_2()
        .pb_2()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgba(LABEL_2))
        .child(label.into())
}

/// 主操作按钮：macOS 强调色胶囊（小尺寸、平光、细阴影）
fn primary_pill(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_4()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(accent_fill())
        .text_color(rgb(0xffffff))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .tab_index(0)
        .focus(|style| style.border_2().border_color(rgb(ACCENT)))
        .shadow(control_shadow())
        .hover(|style| style.bg(rgb(ACCENT_HOVER)))
        .active(|style| style.bg(rgb(ACCENT_PRESS)))
}

/// 次级按钮：macOS bordered 白胶囊
fn glass_pill(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_4()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgba(0xffffffd9))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .text_color(rgb(INK))
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .tab_index(0)
        .focus(|style| style.border_2().border_color(rgb(ACCENT)))
        .shadow(control_shadow())
        .hover(|style| style.bg(rgb(0xffffff)))
        .active(|style| style.bg(rgba(0xeceaf0f5)))
}

/// 无边框圆形图标按钮（如移除），hover 才浮现底色
fn icon_button(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(22.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(rgba(LABEL_2))
        .text_xs()
        .cursor_pointer()
        .tab_index(0)
        .focus(|style| style.bg(rgba(0x007aff1f)).text_color(rgb(INK)))
        .hover(|style| style.bg(rgba(0x3c3c4314)).text_color(rgb(INK)))
        .active(|style| style.bg(rgba(0x3c3c4326)))
}

fn carousel_button(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(34.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgba(0xffffffdc))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .shadow(control_shadow())
        .text_color(rgb(INK))
        .text_size(px(22.))
        .cursor_pointer()
        .tab_index(0)
        .focus(|style| style.border_2().border_color(rgb(ACCENT)))
        .hover(|style| style.bg(rgb(0xffffff)))
        .active(|style| style.bg(rgba(0xeceaf0f5)))
}

fn content_action_button(id: impl Into<ElementId>, prominent: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_3()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(8.))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .tab_index(0)
        .when(prominent, |button| {
            button
                .bg(rgba(0x007aff14))
                .text_color(rgb(ACCENT))
                .hover(|style| style.bg(rgba(0x007aff20)))
                .active(|style| style.bg(rgba(0x007aff2e)))
        })
        .when(!prominent, |button| {
            button
                .bg(rgba(0xffffffd9))
                .text_color(rgb(INK))
                .hover(|style| style.bg(rgb(0xffffff)))
                .active(|style| style.bg(rgba(0xeceaf0f5)))
        })
        .focus(|style| style.border_2().border_color(rgb(ACCENT)))
}

fn checkbox(id: impl Into<ElementId>, checked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(16.))
        .flex_none()
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(checked, |style| {
            style
                .bg(rgb(ACCENT))
                .text_color(rgb(0xffffff))
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .child("\u{2713}")
        })
        .when(!checked, |style| {
            style
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgba(EDGE_DARK))
        })
}

fn context_menu() -> Div {
    div()
        .min_w(px(180.))
        .p(px(5.))
        .rounded(px(12.))
        .bg(rgba(GLASS_CARD))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .shadow(card_shadow())
        .flex()
        .flex_col()
}

fn context_menu_item(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .rounded(px(8.))
        .flex()
        .items_center()
        .text_size(px(13.))
        .text_color(rgb(INK))
        .cursor_pointer()
        .tab_index(0)
        .focus(|style| style.bg(rgb(ACCENT)).text_color(rgb(0xffffff)))
        .hover(|style| style.bg(rgb(ACCENT)).text_color(rgb(0xffffff)))
        .child(label)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Collector,
    Add,
    All,
    MemePacks,
    Settings,
}

impl Page {
    const fn label(self) -> &'static str {
        match self {
            Self::Collector => "Collector",
            Self::Add => "添加",
            Self::All => "全部",
            Self::MemePacks => "MemePack",
            Self::Settings => "设置",
        }
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Collector => "\u{25c8}",
            Self::Add => "＋",
            Self::All => "▦",
            Self::MemePacks => "▤",
            Self::Settings => "⚙\u{fe0e}",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Collector => 0,
            Self::Add => 1,
            Self::All => 2,
            Self::MemePacks => 3,
            Self::Settings => 4,
        }
    }
}

#[derive(Clone, Debug)]
enum DraftContent {
    Image {
        path: PathBuf,
        similar_images: Vec<SimilarMemeImage>,
        collector_item_id: Option<Uuid>,
    },
    Motion {
        collector_item_id: Uuid,
        preview_path: Option<PathBuf>,
    },
    Text {
        text: String,
        collector_item_id: Option<Uuid>,
    },
}

impl DraftContent {
    fn collector_item_id(&self) -> Option<Uuid> {
        match self {
            Self::Image {
                collector_item_id, ..
            }
            | Self::Text {
                collector_item_id, ..
            } => *collector_item_id,
            Self::Motion {
                collector_item_id, ..
            } => Some(*collector_item_id),
        }
    }

    fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. } | Self::Motion { .. })
    }

    fn image_path(&self) -> Option<&Path> {
        match self {
            Self::Image { path, .. } => Some(path),
            Self::Motion { preview_path, .. } => preview_path.as_deref(),
            Self::Text { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
enum Notice {
    Info(String),
    Success(String),
    Warning(String),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TelegramRuntimeState {
    Stopped,
    Starting,
    LoadingModel,
    Running,
    Stopping,
    Failed(String),
}

#[derive(Clone, Debug)]
enum ModelSetupState {
    Checking,
    Downloading(model_download::ModelInstallProgress),
    Ready,
    Failed(String),
}

enum ModelSetupMessage {
    Progress(model_download::ModelInstallProgress),
    Finished(Result<model_download::InstalledModels, String>),
}

struct ImageAnalysisResult {
    path: PathBuf,
    result: Result<Vec<SimilarMemeImage>, String>,
}

struct ImageAnalysisBatch {
    database: MemeDatabase,
    results: Vec<ImageAnalysisResult>,
}

struct CollectorImportResult {
    label: String,
    result: Result<CollectorItem, String>,
}

struct CollectorImportBatch {
    database: MemeDatabase,
    results: Vec<CollectorImportResult>,
}

#[derive(Clone, Copy)]
struct CollectorContextMenu {
    item_id: Uuid,
    position: Point<Pixels>,
}

struct CharacterDetectionResult {
    path: PathBuf,
    result: Result<Option<CharacterMatch>, String>,
}

struct CharacterDetectionBatch {
    sensor: WaifuSensor,
    results: Vec<CharacterDetectionResult>,
}

#[derive(Debug, Error)]
enum UiError {
    #[error("无法加载 embedding 模型：{0}")]
    Clip(#[from] memelith_clip::Error),

    #[error("无法访问 Meme 数据库：{0}")]
    Database(#[from] memelith_core::Error),
}

struct OpenedLibrary {
    database: MemeDatabase,
    inbox_id: Uuid,
    memes: Vec<Meme>,
    meme_packs: Vec<MemePack>,
    pack_names: HashMap<Uuid, String>,
    tags: Vec<Tag>,
    collector_items: Vec<CollectorItem>,
}

struct MemelithView {
    database: Option<MemeDatabase>,
    waifu_sensor: Option<WaifuSensor>,
    installed_models: Option<model_download::InstalledModels>,
    model_setup_state: ModelSetupState,
    pending_storage: Option<(PathBuf, bool)>,
    storage_root: Option<PathBuf>,
    inbox_id: Option<Uuid>,
    page: Page,
    memes: Vec<Meme>,
    meme_packs: Vec<MemePack>,
    pack_names: HashMap<Uuid, String>,
    tags: Vec<Tag>,
    collector_items: Vec<CollectorItem>,
    collector_refresh_pending: bool,
    selected_collector_items: HashSet<Uuid>,
    draft_contents: Vec<DraftContent>,
    active_draft_image: usize,
    composing_text: bool,
    name_input: Entity<TextInput>,
    description_input: Entity<TextInput>,
    tags_input: Entity<TextInput>,
    text_content_input: Entity<TextInput>,
    collector_text_input: Entity<TextInput>,
    meme_search_input: Entity<TextInput>,
    meme_search: Result<Option<search::SearchExpression>, search::SearchError>,
    telegram_token_input: Entity<TextInput>,
    telegram_enabled: bool,
    telegram_applied_enabled: bool,
    telegram_token: String,
    telegram_runtime: Option<telegram::TelegramBotHandle>,
    telegram_runtime_generation: u64,
    telegram_runtime_state: TelegramRuntimeState,
    telegram_sticker_syncing: HashSet<Uuid>,
    telegram_token_error: Option<String>,
    telegram_token_revealed: bool,
    notice: Option<Notice>,
    opening_storage: bool,
    analyzing_images: bool,
    detecting_characters: bool,
    collecting: bool,
    show_only_collector_duplicates: bool,
    collector_context_menu: Option<CollectorContextMenu>,
}

impl MemelithView {
    fn new(
        saved_storage: Result<Option<PathBuf>, settings::SettingsError>,
        saved_telegram: Result<settings::TelegramSettings, settings::SettingsError>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tags_input = cx.new(|cx| TextInput::new("用逗号分隔，例如：猫猫, 反应", cx));
        let text_content_input = cx.new(|cx| TextInput::new("输入 Meme 文字", cx));
        let meme_search_input = cx.new(|cx| TextInput::new("搜索 Meme", cx));
        let telegram_token_input = cx.new(|cx| TextInput::new("粘贴 BotFather 提供的 token", cx));
        telegram_token_input.update(cx, |input, cx| input.set_masked(true, cx));
        let (telegram_enabled, telegram_token, telegram_error) = match saved_telegram {
            Ok(settings) => (settings.enabled, settings.token, None),
            Err(error) => (false, String::new(), Some(error)),
        };
        if !telegram_token.is_empty() {
            let token = telegram_token.clone();
            telegram_token_input.update(cx, |input, cx| input.set_text(token, cx));
        }
        let mut view = Self {
            database: None,
            waifu_sensor: None,
            installed_models: None,
            model_setup_state: ModelSetupState::Checking,
            pending_storage: None,
            storage_root: None,
            inbox_id: None,
            page: Page::All,
            memes: Vec::new(),
            meme_packs: Vec::new(),
            pack_names: HashMap::new(),
            tags: Vec::new(),
            collector_items: Vec::new(),
            collector_refresh_pending: false,
            selected_collector_items: HashSet::new(),
            draft_contents: Vec::new(),
            active_draft_image: 0,
            composing_text: false,
            name_input: cx.new(|cx| TextInput::new("可选，例如：震惊", cx)),
            description_input: cx.new(|cx| TextInput::new("可选，补充使用场景", cx)),
            tags_input: tags_input.clone(),
            text_content_input: text_content_input.clone(),
            collector_text_input: cx.new(|cx| TextInput::new("快速收集一段文字", cx)),
            meme_search_input: meme_search_input.clone(),
            meme_search: Ok(None),
            telegram_token_input,
            telegram_enabled,
            telegram_applied_enabled: telegram_enabled,
            telegram_token,
            telegram_runtime: None,
            telegram_runtime_generation: 0,
            telegram_runtime_state: TelegramRuntimeState::Stopped,
            telegram_sticker_syncing: HashSet::new(),
            telegram_token_error: None,
            telegram_token_revealed: false,
            notice: None,
            opening_storage: false,
            analyzing_images: false,
            detecting_characters: false,
            collecting: false,
            show_only_collector_duplicates: false,
            collector_context_menu: None,
        };

        cx.subscribe(&tags_input, |_, _, _: &TextChanged, cx| cx.notify())
            .detach();
        cx.subscribe(&text_content_input, |_, _, _: &TextChanged, cx| cx.notify())
            .detach();
        cx.subscribe(&meme_search_input, |view, input, _: &TextChanged, cx| {
            let query = input.read(cx).text();
            view.meme_search = search::SearchExpression::parse(&query);
            cx.notify();
        })
        .detach();
        cx.subscribe(
            &view.telegram_token_input.clone(),
            |view, _, _: &TextChanged, cx| {
                view.telegram_token_error = None;
                cx.notify();
            },
        )
        .detach();

        match saved_storage {
            Ok(Some(path)) => view.pending_storage = Some((path, false)),
            Ok(None) => {}
            Err(error) => {
                view.notice = Some(Notice::Error(format!("无法读取上次的存储位置：{error}")));
            }
        }
        if let Some(error) = telegram_error {
            view.notice = Some(Notice::Error(format!("无法读取 Telegram 设置：{error}")));
        }
        view.start_model_setup(cx);
        view
    }

    fn start_model_setup(&mut self, cx: &mut Context<Self>) {
        self.installed_models = None;
        self.model_setup_state = ModelSetupState::Checking;
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("memelith-model-setup".to_owned())
            .spawn(move || {
                let result = model_download::ensure_default_models(|progress| {
                    let _ = sender.send(ModelSetupMessage::Progress(progress));
                })
                .map_err(|error| error.to_string());
                let _ = sender.send(ModelSetupMessage::Finished(result));
            });
        if let Err(error) = thread {
            self.model_setup_state =
                ModelSetupState::Failed(format!("无法启动模型下载线程：{error}"));
            cx.notify();
            return;
        }

        cx.spawn(async move |this, cx| {
            loop {
                loop {
                    match receiver.try_recv() {
                        Ok(ModelSetupMessage::Progress(progress)) => {
                            if this
                                .update(cx, |view, cx| {
                                    view.model_setup_state = ModelSetupState::Downloading(progress);
                                    cx.notify();
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(ModelSetupMessage::Finished(result)) => {
                            let _ = this
                                .update(cx, |view, cx| view.apply_model_setup_result(result, cx));
                            return;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            let _ = this.update(cx, |view, cx| {
                                view.model_setup_state =
                                    ModelSetupState::Failed("模型下载线程意外结束".to_owned());
                                cx.notify();
                            });
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    fn apply_model_setup_result(
        &mut self,
        result: Result<model_download::InstalledModels, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(models) => {
                self.installed_models = Some(models);
                self.model_setup_state = ModelSetupState::Ready;
                if let Some((path, persist)) = self.pending_storage.take() {
                    self.activate_storage(path, persist, cx);
                }
            }
            Err(error) => {
                self.model_setup_state = ModelSetupState::Failed(error);
            }
        }
        cx.notify();
    }

    fn retry_model_setup(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.model_setup_state, ModelSetupState::Failed(_)) {
            self.start_model_setup(cx);
        }
    }

    fn activate_storage(&mut self, path: PathBuf, persist: bool, cx: &mut Context<Self>) {
        let Some(models) = self.installed_models.clone() else {
            self.pending_storage = Some((path, persist));
            self.notice = Some(Notice::Info("模型准备完成后会自动打开存储位置".to_owned()));
            cx.notify();
            return;
        };
        if self.opening_storage {
            self.notice = Some(Notice::Info("正在打开存储位置".to_owned()));
            cx.notify();
            return;
        }
        self.stop_telegram_bot();
        self.opening_storage = true;
        self.notice = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let model_directory = models.clip_directory;
            let result = cx
                .background_executor()
                .spawn(async move { open_library(&path, &model_directory) })
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(opened) => {
                        let canonical_root = opened.database.storage_root().to_path_buf();
                        view.database = Some(opened.database);
                        view.waifu_sensor = None;
                        view.storage_root = Some(canonical_root.clone());
                        view.inbox_id = Some(opened.inbox_id);
                        view.memes = opened.memes;
                        view.meme_packs = opened.meme_packs;
                        view.pack_names = opened.pack_names;
                        view.tags = opened.tags;
                        view.collector_items = opened.collector_items;
                        view.collector_refresh_pending = false;
                        view.selected_collector_items.clear();
                        view.collector_context_menu = None;
                        view.reset_add_form(cx);
                        view.collector_text_input
                            .update(cx, |input, cx| input.reset(cx));
                        view.meme_search_input
                            .update(cx, |input, cx| input.reset(cx));
                        view.page = Page::All;
                        if persist {
                            view.notice = match settings::save_storage_root(&canonical_root) {
                                Ok(()) => Some(Notice::Success("存储位置已更新".to_owned())),
                                Err(error) => Some(Notice::Error(format!(
                                    "存储已打开，但无法记住该位置：{error}"
                                ))),
                            };
                        }
                    }
                    Err(error) => {
                        view.notice = Some(Notice::Error(error.to_string()));
                    }
                }
                if view.database.is_some() {
                    view.start_telegram_bot(cx);
                }
                view.opening_storage = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_telegram_bot(&mut self) {
        self.telegram_runtime_generation = self.telegram_runtime_generation.wrapping_add(1);
        if let Some(runtime) = self.telegram_runtime.take() {
            runtime.stop();
        }
        self.telegram_sticker_syncing.clear();
        self.telegram_runtime_state = TelegramRuntimeState::Stopped;
    }

    fn request_telegram_stop(&mut self) {
        if let Some(runtime) = self.telegram_runtime.as_mut() {
            runtime.request_stop();
            self.telegram_runtime_state = TelegramRuntimeState::Stopping;
        } else {
            self.telegram_runtime_state = TelegramRuntimeState::Stopped;
        }
    }

    fn start_telegram_bot(&mut self, cx: &mut Context<Self>) {
        if !self.telegram_applied_enabled {
            self.telegram_runtime_state = TelegramRuntimeState::Stopped;
            return;
        }
        if self.telegram_token.trim().is_empty() {
            let message = "未填写 Bot Token".to_owned();
            self.telegram_runtime_state = TelegramRuntimeState::Failed(message.clone());
            self.telegram_token_error = Some(message.clone());
            self.notice = Some(Notice::Error(format!("Telegram Bot 启动失败：{message}")));
            return;
        }
        let Some(storage_root) = self.storage_root.clone() else {
            self.telegram_runtime_state = TelegramRuntimeState::Stopped;
            return;
        };
        let Some(models) = self.installed_models.as_ref() else {
            let message = "Telegram Bot 无法加载模型：模型尚未准备完成".to_owned();
            self.telegram_runtime_state = TelegramRuntimeState::Failed(message.clone());
            self.notice = Some(Notice::Error(message));
            return;
        };
        let model_directory = models.clip_directory.clone();
        self.telegram_runtime_generation = self.telegram_runtime_generation.wrapping_add(1);
        let generation = self.telegram_runtime_generation;
        self.telegram_runtime_state = TelegramRuntimeState::Starting;
        let (runtime, status_receiver) = match telegram::TelegramBotHandle::start(
            storage_root,
            self.telegram_token.clone(),
            model_directory,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                let message = format!("无法启动 Telegram Bot 线程：{error}");
                self.telegram_runtime_state = TelegramRuntimeState::Failed(message.clone());
                self.notice = Some(Notice::Error(message));
                return;
            }
        };
        self.telegram_runtime = Some(runtime);
        self.observe_telegram_status(generation, status_receiver, cx);
    }

    fn observe_telegram_status(
        &self,
        generation: u64,
        status_receiver: mpsc::Receiver<telegram::TelegramBotStatus>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                loop {
                    match status_receiver.try_recv() {
                        Ok(status) => {
                            if this
                                .update(cx, |view, cx| {
                                    view.apply_telegram_status(generation, status, cx)
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            let _ = this.update(cx, |view, cx| {
                                view.handle_telegram_status_disconnect(generation, cx)
                            });
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    fn apply_telegram_status(
        &mut self,
        generation: u64,
        status: telegram::TelegramBotStatus,
        cx: &mut Context<Self>,
    ) {
        if generation != self.telegram_runtime_generation {
            return;
        }
        match status {
            telegram::TelegramBotStatus::Starting => {
                if self.telegram_runtime_state != TelegramRuntimeState::Stopping {
                    self.telegram_runtime_state = TelegramRuntimeState::Starting;
                }
            }
            telegram::TelegramBotStatus::LoadingModel => {
                if self.telegram_runtime_state != TelegramRuntimeState::Stopping {
                    self.telegram_runtime_state = TelegramRuntimeState::LoadingModel;
                }
            }
            telegram::TelegramBotStatus::Running => {
                if self.telegram_runtime_state != TelegramRuntimeState::Stopping {
                    self.telegram_runtime_state = TelegramRuntimeState::Running;
                }
                if matches!(
                    self.notice.as_ref(),
                    Some(Notice::Success(message))
                        if message == "Telegram 设置已保存，Bot 正在启动"
                ) {
                    self.notice = Some(Notice::Success("Telegram Bot 已启动".to_owned()));
                }
            }
            telegram::TelegramBotStatus::CollectorUpdated => {
                self.collector_refresh_pending = true;
                if self.database.is_some()
                    && let Err(error) = self.refresh_collector()
                {
                    self.notice = Some(Notice::Error(format!(
                        "Telegram 已更新 Collector，但刷新界面失败：{error}"
                    )));
                }
            }
            telegram::TelegramBotStatus::StickerPackSyncStarted { pack_id } => {
                self.telegram_sticker_syncing.insert(pack_id);
            }
            telegram::TelegramBotStatus::StickerPackSynced {
                pack_id,
                title,
                added,
                skipped,
                announce,
            } => {
                self.telegram_sticker_syncing.remove(&pack_id);
                if announce {
                    self.notice = Some(Notice::Success(format!(
                        "贴纸包「{title}」已更新：新增 {added} 张，跳过 {skipped} 张"
                    )));
                }
                if let Err(error) = self.refresh_library() {
                    self.notice = Some(Notice::Error(format!(
                        "贴纸包已同步，但刷新列表失败：{error}"
                    )));
                }
            }
            telegram::TelegramBotStatus::StickerPackSyncFailed {
                pack_id,
                message,
                announce,
                terminal,
            } => {
                if terminal {
                    self.telegram_sticker_syncing.remove(&pack_id);
                }
                if announce {
                    self.notice = Some(Notice::Error(format!("贴纸包更新失败：{message}")));
                }
            }
            telegram::TelegramBotStatus::Failed(message) => {
                self.telegram_runtime.take();
                self.telegram_sticker_syncing.clear();
                if self.telegram_runtime_state == TelegramRuntimeState::Stopping {
                    self.telegram_runtime_state = TelegramRuntimeState::Stopped;
                    if self.telegram_enabled
                        && self.telegram_applied_enabled
                        && self.telegram_token_input.read(cx).text().trim() == self.telegram_token
                    {
                        self.start_telegram_bot(cx);
                    }
                } else {
                    self.telegram_runtime_state = TelegramRuntimeState::Failed(message.clone());
                    self.notice = Some(Notice::Error(format!("Telegram Bot 启动失败：{message}")));
                }
            }
            telegram::TelegramBotStatus::Stopped => {
                self.telegram_runtime.take();
                self.telegram_sticker_syncing.clear();
                self.telegram_runtime_state = TelegramRuntimeState::Stopped;
                if self.telegram_enabled
                    && self.telegram_applied_enabled
                    && self.telegram_token_input.read(cx).text().trim() == self.telegram_token
                {
                    self.start_telegram_bot(cx);
                }
            }
        }
        cx.notify();
    }

    fn handle_telegram_status_disconnect(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.telegram_runtime_generation {
            return;
        }
        match self.telegram_runtime_state {
            TelegramRuntimeState::Starting
            | TelegramRuntimeState::LoadingModel
            | TelegramRuntimeState::Running => {
                self.telegram_runtime.take();
                self.telegram_sticker_syncing.clear();
                let message = "Bot 线程意外退出".to_owned();
                self.telegram_runtime_state = TelegramRuntimeState::Failed(message.clone());
                self.notice = Some(Notice::Error(format!("Telegram Bot 启动失败：{message}")));
                cx.notify();
            }
            TelegramRuntimeState::Stopping => {
                self.telegram_runtime.take();
                self.telegram_sticker_syncing.clear();
                self.telegram_runtime_state = TelegramRuntimeState::Stopped;
                cx.notify();
            }
            TelegramRuntimeState::Stopped | TelegramRuntimeState::Failed(_) => {}
        }
    }

    fn telegram_settings_dirty(&self, cx: &App) -> bool {
        self.telegram_enabled != self.telegram_applied_enabled
            || self.telegram_token_input.read(cx).text().trim() != self.telegram_token
    }

    fn telegram_status_text(&self, cx: &App) -> String {
        let dirty = self.telegram_settings_dirty(cx);
        let status = match &self.telegram_runtime_state {
            TelegramRuntimeState::Stopped if !self.telegram_enabled && !dirty => {
                "已停用".to_owned()
            }
            TelegramRuntimeState::Stopped => "已停止".to_owned(),
            TelegramRuntimeState::Starting => "正在启动…".to_owned(),
            TelegramRuntimeState::LoadingModel => "正在加载模型…".to_owned(),
            TelegramRuntimeState::Running => "正在运行".to_owned(),
            TelegramRuntimeState::Stopping => "正在停止…".to_owned(),
            TelegramRuntimeState::Failed(message) => format!("启动失败：{message}"),
        };
        if dirty {
            format!("{status} · 有未应用的更改")
        } else {
            status
        }
    }

    fn apply_telegram_settings(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let token = self.telegram_token_input.read(cx).text().trim().to_owned();
        if self.telegram_enabled && token.is_empty() {
            self.telegram_token_error = Some("启用 Bot 前需要填写 Token".to_owned());
            self.notice = Some(Notice::Error(
                "启用 Telegram Bot 前请先填写 token".to_owned(),
            ));
            self.telegram_token_input.focus_handle(cx).focus(window);
            cx.notify();
            return;
        }
        let next = settings::TelegramSettings {
            enabled: self.telegram_enabled,
            token: token.clone(),
        };
        match settings::save_telegram_settings(&next) {
            Ok(()) => {
                let restart_required = !self.telegram_applied_enabled
                    || self.telegram_token != token
                    || matches!(
                        self.telegram_runtime_state,
                        TelegramRuntimeState::Stopped
                            | TelegramRuntimeState::Stopping
                            | TelegramRuntimeState::Failed(_)
                    );
                self.telegram_applied_enabled = self.telegram_enabled;
                self.telegram_token = token;
                self.telegram_token_error = None;
                if self.telegram_enabled {
                    if restart_required {
                        self.stop_telegram_bot();
                        self.start_telegram_bot(cx);
                    }
                    match self.telegram_runtime_state {
                        TelegramRuntimeState::Starting | TelegramRuntimeState::LoadingModel => {
                            self.notice = Some(Notice::Success(
                                "Telegram 设置已保存，Bot 正在启动".to_owned(),
                            ));
                        }
                        TelegramRuntimeState::Running => {
                            self.notice = Some(Notice::Success(
                                "Telegram 设置已保存，Bot 正在运行".to_owned(),
                            ));
                        }
                        _ => {}
                    }
                } else {
                    self.request_telegram_stop();
                    self.notice = Some(Notice::Success(if self.telegram_runtime.is_some() {
                        "Telegram 设置已保存，Bot 正在停止".to_owned()
                    } else {
                        "Telegram 设置已保存，Bot 已停用".to_owned()
                    }));
                }
            }
            Err(error) => {
                self.notice = Some(Notice::Error(format!("无法保存 Telegram 设置：{error}")));
            }
        }
        cx.notify();
    }

    fn toggle_telegram(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.telegram_enabled = !self.telegram_enabled;
        if !self.telegram_enabled {
            self.telegram_token_error = None;
            self.request_telegram_stop();
        } else if self.telegram_applied_enabled
            && self.telegram_token_input.read(cx).text().trim() == self.telegram_token
            && matches!(
                self.telegram_runtime_state,
                TelegramRuntimeState::Stopped | TelegramRuntimeState::Failed(_)
            )
        {
            self.start_telegram_bot(cx);
        }
        cx.notify();
    }

    fn toggle_telegram_token_visibility(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.telegram_token_revealed = !self.telegram_token_revealed;
        self.telegram_token_input.update(cx, |input, cx| {
            input.set_masked(!self.telegram_token_revealed, cx)
        });
        cx.notify();
    }

    fn choose_storage(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("内容处理完成后才能更换存储位置".to_owned()));
            cx.notify();
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择存储位置".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                let path = paths.into_iter().next();
                if let Some(path) = path {
                    let _ = this.update(cx, |view, cx| view.activate_storage(path, true, cx));
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("无法打开目录选择器：{error}")));
                    cx.notify();
                });
            }
            Err(error) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("目录选择已中断：{error}")));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn choose_images(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("正在处理草稿图片".to_owned()));
            cx.notify();
            return;
        }
        if self.draft_uses_collector() {
            self.notice = Some(Notice::Info(
                "来自 Collector 的项目不能再混入新的草稿内容".to_owned(),
            ));
            cx.notify();
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("添加图片".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                let database = match this.update(cx, |view, cx| {
                    let database = view.database.take();
                    if database.is_some() {
                        view.analyzing_images = true;
                        view.collector_context_menu = None;
                        view.notice = Some(Notice::Info(format!(
                            "正在计算 {} 张图片的 CLIP 特征并查重…",
                            paths.len()
                        )));
                    } else {
                        view.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
                    }
                    cx.notify();
                    database
                }) {
                    Ok(Some(database)) => database,
                    Ok(None) | Err(_) => return,
                };

                let batch = cx
                    .background_executor()
                    .spawn(async move { analyze_selected_images(database, paths) })
                    .await;

                let _ = this.update(cx, |view, cx| {
                    view.database = Some(batch.database);
                    let mut duplicate_count = 0;
                    let mut errors = Vec::new();
                    for ImageAnalysisResult { path, result } in batch.results {
                        match result {
                            Ok(similar_images) => {
                                if !similar_images.is_empty() {
                                    duplicate_count += 1;
                                }
                                view.draft_contents.push(DraftContent::Image {
                                    path,
                                    similar_images,
                                    collector_item_id: None,
                                });
                            }
                            Err(error) => errors.push(format!(
                                "{}：{error}",
                                path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string())
                            )),
                        }
                    }
                    view.analyzing_images = false;
                    view.notice = if !errors.is_empty() {
                        Some(Notice::Error(format!(
                            "部分图片分析失败：{}",
                            errors.join("；")
                        )))
                    } else if duplicate_count > 0 {
                        Some(Notice::Warning(format!(
                            "发现 {duplicate_count} 张疑似重复图片，已在草稿中标记"
                        )))
                    } else {
                        None
                    };
                    if view.collector_refresh_pending
                        && let Err(error) = view.refresh_collector()
                    {
                        view.notice = Some(Notice::Error(format!(
                            "Telegram 已更新 Collector，但刷新界面失败：{error}"
                        )));
                    }
                    cx.notify();
                });
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("无法打开图片选择器：{error}")));
                    cx.notify();
                });
            }
            Err(error) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("图片选择已中断：{error}")));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn choose_collector_images(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("正在处理其他内容".to_owned()));
            cx.notify();
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("收集图片".into()),
        });
        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                let database = match this.update(cx, |view, cx| {
                    let database = view.database.take();
                    if database.is_some() {
                        view.collecting = true;
                        view.collector_context_menu = None;
                        view.notice = Some(Notice::Info(format!(
                            "正在分析并收集 {} 张图片…",
                            paths.len()
                        )));
                    } else {
                        view.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
                    }
                    cx.notify();
                    database
                }) {
                    Ok(Some(database)) => database,
                    Ok(None) | Err(_) => return,
                };

                let batch = cx
                    .background_executor()
                    .spawn(async move { collect_selected_images(database, paths) })
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.apply_collector_import_batch(batch, false, cx)
                });
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("无法打开图片选择器：{error}")));
                    cx.notify();
                });
            }
            Err(error) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("图片选择已中断：{error}")));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn collect_collector_text(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("正在处理其他内容".to_owned()));
            cx.notify();
            return;
        }
        let text = self.collector_text_input.read(cx).text();
        if text.trim().is_empty() {
            self.notice = Some(Notice::Error("请先输入需要收集的文字".to_owned()));
            cx.notify();
            return;
        }
        let database = self.database.take();
        let Some(database) = database else {
            self.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
            cx.notify();
            return;
        };
        self.collecting = true;
        self.collector_context_menu = None;
        self.notice = Some(Notice::Info("正在分析并收集文字…".to_owned()));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let batch = cx
                .background_executor()
                .spawn(async move { collect_text_item(database, text) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.apply_collector_import_batch(batch, true, cx)
            });
        })
        .detach();
    }

    fn apply_collector_import_batch(
        &mut self,
        batch: CollectorImportBatch,
        reset_text: bool,
        cx: &mut Context<Self>,
    ) {
        self.database = Some(batch.database);
        self.collecting = false;
        let mut collected = 0;
        let mut hash_duplicates = 0;
        let mut similar_duplicates = 0;
        let mut errors = Vec::new();
        for CollectorImportResult { label, result } in batch.results {
            match result {
                Ok(item) => {
                    collected += 1;
                    match item.duplicate {
                        Some(CollectorDuplicate::Hash { .. }) => hash_duplicates += 1,
                        Some(CollectorDuplicate::Similarity { .. }) => similar_duplicates += 1,
                        None => {}
                    }
                }
                Err(error) => errors.push(format!("{label}：{error}")),
            }
        }

        if reset_text && collected > 0 {
            self.collector_text_input
                .update(cx, |input, cx| input.reset(cx));
        }
        let refresh_error = self.refresh_library().err().map(|error| error.to_string());
        self.notice = if let Some(error) = refresh_error {
            Some(Notice::Error(format!(
                "内容已处理，但刷新 Collector 失败：{error}"
            )))
        } else if !errors.is_empty() {
            Some(Notice::Error(format!(
                "已收集 {collected} 项，部分内容失败：{}",
                errors.join("；")
            )))
        } else if hash_duplicates > 0 || similar_duplicates > 0 {
            Some(Notice::Warning(format!(
                "已收集 {collected} 项，其中 {hash_duplicates} 项 Hash 重复、{similar_duplicates} 项疑似重复"
            )))
        } else {
            Some(Notice::Success(format!("已收集 {collected} 项")))
        };
        cx.notify();
    }

    fn toggle_collector_filter(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_only_collector_duplicates = !self.show_only_collector_duplicates;
        self.collector_context_menu = None;
        cx.notify();
    }

    fn toggle_collector_item(&mut self, item_id: Uuid, cx: &mut Context<Self>) {
        let Some(item) = self
            .collector_items
            .iter()
            .find(|item| item.id == item_id)
            .cloned()
        else {
            self.notice = Some(Notice::Error("Collector 条目已经不存在".to_owned()));
            cx.notify();
            return;
        };
        if let Some(duplicate) = &item.duplicate {
            self.notice = Some(Notice::Info(match duplicate {
                CollectorDuplicate::Hash { .. } => "Hash 重复项不能添加为正式项目".to_owned(),
                CollectorDuplicate::Similarity { .. } => {
                    "请先右键选择“这不是重复”再选取此项".to_owned()
                }
            }));
        } else if self.draft_uses_collector() {
            if self.remove_collector_item_from_draft(item_id) {
                self.notice = None;
            } else {
                match self.draft_content_from_collector_item(&item) {
                    Ok(content) => {
                        self.draft_contents.push(content);
                        self.selected_collector_items.insert(item_id);
                        self.notice = None;
                    }
                    Err(message) => self.notice = Some(Notice::Error(message)),
                }
            }
        } else if !self.selected_collector_items.remove(&item_id) {
            self.selected_collector_items.insert(item_id);
            self.notice = None;
        } else {
            self.notice = None;
        }
        cx.notify();
    }

    fn draft_content_from_collector_item(
        &self,
        item: &CollectorItem,
    ) -> Result<DraftContent, String> {
        let storage_root = self
            .storage_root
            .as_ref()
            .ok_or_else(|| "存储位置尚未准备好".to_owned())?;
        match &item.content {
            CollectorContent::Image { relative_path, .. } => {
                let path = MemeDatabase::resolve_media_path_from_root(storage_root, relative_path)
                    .map_err(|error| format!("无法打开 Collector 图片：{error}"))?;
                Ok(DraftContent::Image {
                    path,
                    similar_images: Vec::new(),
                    collector_item_id: Some(item.id),
                })
            }
            CollectorContent::Motion {
                preview_relative_path,
                ..
            } => {
                let preview_path = preview_relative_path
                    .as_ref()
                    .map(|relative_path| {
                        MemeDatabase::resolve_media_path_from_root(storage_root, relative_path)
                            .map_err(|error| format!("无法打开 Collector 图片：{error}"))
                    })
                    .transpose()?;
                Ok(DraftContent::Motion {
                    collector_item_id: item.id,
                    preview_path,
                })
            }
            CollectorContent::Text { text } => Ok(DraftContent::Text {
                text: text.clone(),
                collector_item_id: Some(item.id),
            }),
        }
    }

    fn remove_collector_item_from_draft(&mut self, item_id: Uuid) -> bool {
        let Some(index) = self
            .draft_contents
            .iter()
            .position(|content| content.collector_item_id() == Some(item_id))
        else {
            return false;
        };
        let removed_image_index = self.draft_contents[index].is_image().then(|| {
            self.draft_contents[..index]
                .iter()
                .filter(|content| content.is_image())
                .count()
        });
        self.draft_contents.remove(index);
        self.selected_collector_items.remove(&item_id);
        self.reconcile_active_draft_image_after_removal(removed_image_index);
        true
    }

    fn add_selected_collector_items(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_uses_collector() {
            self.page = Page::Add;
            self.notice = None;
            cx.notify();
            return;
        }
        if self.show_only_collector_duplicates {
            self.notice = Some(Notice::Info(
                "请先关闭“只显示重复项”再添加所选内容".to_owned(),
            ));
            cx.notify();
            return;
        }
        if self.selected_collector_items.is_empty() {
            self.notice = Some(Notice::Info("请先选择要添加的 Collector 内容".to_owned()));
            cx.notify();
            return;
        }
        if !self.draft_contents.is_empty() {
            self.notice = Some(Notice::Warning(
                "“添加”页面已有草稿，请先保存或逐项移除后再从 Collector 添加".to_owned(),
            ));
            cx.notify();
            return;
        }
        let mut draft = Vec::new();
        for item in self.collector_items.iter().rev() {
            if !self.selected_collector_items.contains(&item.id) {
                continue;
            }
            if item.duplicate.is_some() {
                self.notice = Some(Notice::Error(format!(
                    "Collector 条目 {} 仍标记为重复",
                    item.id
                )));
                cx.notify();
                return;
            }
            match self.draft_content_from_collector_item(item) {
                Ok(content) => draft.push(content),
                Err(message) => {
                    self.notice = Some(Notice::Error(message));
                    cx.notify();
                    return;
                }
            }
        }
        if draft.is_empty() {
            self.notice = Some(Notice::Error("选中的 Collector 内容已经不存在".to_owned()));
            cx.notify();
            return;
        }
        self.reset_add_form(cx);
        self.draft_contents = draft;
        self.page = Page::Add;
        self.notice = None;
        cx.notify();
    }

    fn open_collector_context_menu(
        &mut self,
        item_id: Uuid,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.collector_context_menu = None;
            self.notice = Some(Notice::Info(
                "内容处理完成后才能操作 Collector 条目".to_owned(),
            ));
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.collector_items.iter().any(|item| item.id == item_id) {
            self.collector_context_menu = Some(CollectorContextMenu {
                item_id,
                position: event.position,
            });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn close_collector_context_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.collector_context_menu = None;
        cx.notify();
    }

    fn dismiss_collector_similarity(
        &mut self,
        item_id: Uuid,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.collector_context_menu = None;
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info(
                "内容处理完成后才能操作 Collector 条目".to_owned(),
            ));
            cx.notify();
            return;
        }
        let Some(database) = self.database.as_mut() else {
            self.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
            cx.notify();
            return;
        };
        match database.dismiss_collector_similarity(item_id) {
            Ok(updated) => {
                let Some(item) = self
                    .collector_items
                    .iter_mut()
                    .find(|item| item.id == item_id)
                else {
                    self.notice = Some(Notice::Error(
                        "重复状态已更新，但界面中的条目已经不存在".to_owned(),
                    ));
                    cx.notify();
                    return;
                };
                *item = updated;
                self.notice = Some(Notice::Success("已标记为非重复内容".to_owned()));
            }
            Err(error) => {
                self.notice = Some(Notice::Error(format!("无法更新重复状态：{error}")));
            }
        }
        cx.notify();
    }

    fn request_delete_collector_item(
        &mut self,
        item_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.collector_context_menu = None;
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info(
                "内容处理完成后才能操作 Collector 条目".to_owned(),
            ));
            cx.notify();
            return;
        }
        let Some(item) = self.collector_items.iter().find(|item| item.id == item_id) else {
            self.notice = Some(Notice::Error("Collector 条目已经不存在".to_owned()));
            cx.notify();
            return;
        };
        let detail = match &item.content {
            CollectorContent::Image { .. } => {
                "删除后无法从 Collector 恢复；对应的图片文件也会一并移除。"
            }
            CollectorContent::Motion { .. } => {
                "删除后无法从 Collector 恢复；对应的图片文件也会一并移除。"
            }
            CollectorContent::Text { .. } => "删除后无法从 Collector 恢复。",
        };

        let answer = window.prompt(
            PromptLevel::Warning,
            "删除这项 Collector 内容？",
            Some(detail),
            &[PromptButton::ok("删除"), PromptButton::cancel("取消")],
            cx,
        );
        cx.spawn(async move |this, cx| match answer.await {
            Ok(0) => {
                let _ = this.update(cx, |view, cx| {
                    view.delete_collector_item(item_id, cx);
                });
            }
            Ok(_) => {}
            Err(error) => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(Notice::Error(format!("删除确认已中断：{error}")));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn delete_collector_item(&mut self, item_id: Uuid, cx: &mut Context<Self>) {
        let Some(database) = self.database.as_mut() else {
            self.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
            cx.notify();
            return;
        };
        if let Err(error) = database.delete_collector_item(item_id) {
            self.notice = Some(Notice::Error(format!("无法删除 Collector 条目：{error}")));
            cx.notify();
            return;
        }

        self.collector_items.retain(|item| item.id != item_id);
        self.selected_collector_items.remove(&item_id);
        let removed_image_index = self
            .draft_contents
            .iter()
            .position(|content| content.collector_item_id() == Some(item_id))
            .and_then(|removed_index| {
                self.draft_contents[removed_index].is_image().then(|| {
                    self.draft_contents[..removed_index]
                        .iter()
                        .filter(|content| content.is_image())
                        .count()
                })
            });
        let previous_draft_len = self.draft_contents.len();
        self.draft_contents
            .retain(|content| content.collector_item_id() != Some(item_id));
        self.reconcile_active_draft_image_after_removal(removed_image_index);
        let removed_from_draft = self.draft_contents.len() != previous_draft_len;
        self.notice = match self.refresh_library() {
            Ok(()) if removed_from_draft => Some(Notice::Success(
                "已删除 Collector 条目，并从添加草稿中移除".to_owned(),
            )),
            Ok(()) => Some(Notice::Success("已删除 Collector 条目".to_owned())),
            Err(error) => Some(Notice::Error(format!(
                "Collector 条目已删除，但刷新列表失败：{error}"
            ))),
        };
        cx.notify();
    }

    fn detect_characters(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("正在处理草稿图片".to_owned()));
            cx.notify();
            return;
        }
        let image_paths = self
            .draft_contents
            .iter()
            .filter_map(DraftContent::image_path)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        if image_paths.is_empty() {
            self.notice = Some(Notice::Error("请先添加需要识别的图片".to_owned()));
            cx.notify();
            return;
        }
        let Some(storage_root) = self.storage_root.clone() else {
            self.notice = Some(Notice::Error("存储位置尚未准备好".to_owned()));
            cx.notify();
            return;
        };
        let Some(models) = self.installed_models.as_ref() else {
            self.notice = Some(Notice::Error("模型尚未准备完成".to_owned()));
            cx.notify();
            return;
        };
        let waifu_model_path = models.waifu_model_path.clone();

        let sensor = self.waifu_sensor.take();
        self.detecting_characters = true;
        self.collector_context_menu = None;
        self.notice = Some(Notice::Info(format!(
            "正在通过 Waifu Sensor 识别 {} 张图片…",
            image_paths.len()
        )));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    detect_draft_characters(sensor, storage_root, waifu_model_path, image_paths)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.detecting_characters = false;
                match result {
                    Ok(batch) => view.apply_character_detection(batch, cx),
                    Err(error) => {
                        view.notice =
                            Some(Notice::Error(format!("Waifu Sensor 识别失败：{error}")));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_character_detection(
        &mut self,
        batch: CharacterDetectionBatch,
        cx: &mut Context<Self>,
    ) {
        self.waifu_sensor = Some(batch.sensor);
        let mut characters = Vec::new();
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for CharacterDetectionResult { path, result } in batch.results {
            match result {
                Ok(Some(character)) => {
                    if seen.insert(character.name.to_ascii_lowercase()) {
                        characters.push(character.name);
                    }
                }
                Ok(None) => {}
                Err(error) => errors.push(format!(
                    "{}：{error}",
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                )),
            }
        }

        let current_tags = self.tags_input.read(cx).text();
        let (merged_tags, added_characters) = merge_tags(&current_tags, &characters);
        if !added_characters.is_empty() {
            self.tags_input
                .update(cx, |input, cx| input.set_text(merged_tags, cx));
        }

        self.notice = if characters.is_empty() {
            if errors.is_empty() {
                Some(Notice::Info("Waifu Sensor 没有返回角色候选".to_owned()))
            } else {
                Some(Notice::Error(format!(
                    "角色识别失败：{}",
                    errors.join("；")
                )))
            }
        } else if !errors.is_empty() {
            Some(Notice::Warning(format!(
                "已识别角色：{}；部分图片处理失败：{}",
                characters.join("、"),
                errors.join("；")
            )))
        } else if added_characters.is_empty() {
            Some(Notice::Info(format!(
                "识别到角色：{}，对应标签已存在",
                characters.join("、")
            )))
        } else {
            Some(Notice::Success(format!(
                "已将角色加入 Tag：{}",
                added_characters.join("、")
            )))
        };
    }

    fn add_text_content(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.commit_text_content(cx);
    }

    fn confirm_text_content(
        &mut self,
        _: &ConfirmTextContent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_text_content(cx);
    }

    fn commit_text_content(&mut self, cx: &mut Context<Self>) {
        if self.draft_uses_collector() {
            self.notice = Some(Notice::Info(
                "来自 Collector 的项目不能再混入新的草稿内容".to_owned(),
            ));
            cx.notify();
            return;
        }
        let text = self.text_content_input.read(cx).text();
        if text.trim().is_empty() {
            self.notice = Some(Notice::Error("请先输入文字内容".to_owned()));
            cx.notify();
            return;
        }
        self.draft_contents.push(DraftContent::Text {
            text: text.trim().to_owned(),
            collector_item_id: None,
        });
        self.text_content_input
            .update(cx, |input, cx| input.reset(cx));
        self.composing_text = false;
        self.notice = None;
        cx.notify();
    }

    fn begin_text_composer(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_uses_collector() {
            self.notice = Some(Notice::Info(
                "来自 Collector 的项目不能再混入新的草稿内容".to_owned(),
            ));
            cx.notify();
            return;
        }
        self.composing_text = true;
        self.notice = None;
        window.focus(&self.text_content_input.focus_handle(cx));
        cx.notify();
    }

    fn cancel_text_composer(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_text_composer(cx);
    }

    fn cancel_text_composer_action(
        &mut self,
        _: &CancelTextComposer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_text_composer(cx);
    }

    fn dismiss_text_composer(&mut self, cx: &mut Context<Self>) {
        self.text_content_input
            .update(cx, |input, cx| input.reset(cx));
        self.composing_text = false;
        self.notice = None;
        cx.notify();
    }

    fn remove_draft_content(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.detecting_characters {
            self.notice = Some(Notice::Info("角色识别完成后才能修改草稿内容".to_owned()));
            cx.notify();
            return;
        }
        if index >= self.draft_contents.len() {
            self.notice = Some(Notice::Error("要移除的内容已经不存在".to_owned()));
        } else {
            let removed_image_index = self.draft_contents[index].is_image().then(|| {
                self.draft_contents[..index]
                    .iter()
                    .filter(|content| content.is_image())
                    .count()
            });
            let removed = self.draft_contents.remove(index);
            if let Some(item_id) = removed.collector_item_id() {
                self.selected_collector_items.remove(&item_id);
            }
            self.reconcile_active_draft_image_after_removal(removed_image_index);
            self.notice = None;
        }
        cx.notify();
    }

    fn reconcile_active_draft_image_after_removal(&mut self, removed_image_index: Option<usize>) {
        let Some(removed_image_index) = removed_image_index else {
            return;
        };
        let remaining_images = self
            .draft_contents
            .iter()
            .filter(|content| content.is_image())
            .count();
        if remaining_images == 0 {
            self.active_draft_image = 0;
        } else if removed_image_index < self.active_draft_image {
            self.active_draft_image -= 1;
        } else if self.active_draft_image >= remaining_images {
            self.active_draft_image = remaining_images - 1;
        }
    }

    fn show_previous_draft_image(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let image_count = self
            .draft_contents
            .iter()
            .filter(|content| content.is_image())
            .count();
        if image_count <= 1 {
            return;
        }
        self.active_draft_image = if self.active_draft_image == 0 {
            image_count - 1
        } else {
            self.active_draft_image - 1
        };
        cx.notify();
    }

    fn show_next_draft_image(
        &mut self,
        _: &gpui::ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let image_count = self
            .draft_contents
            .iter()
            .filter(|content| content.is_image())
            .count();
        if image_count <= 1 {
            return;
        }
        self.active_draft_image = if self.active_draft_image + 1 == image_count {
            0
        } else {
            self.active_draft_image + 1
        };
        cx.notify();
    }

    fn save_meme(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.analyzing_images || self.detecting_characters || self.collecting {
            self.notice = Some(Notice::Info("图片处理完成后才能保存 Meme".to_owned()));
            cx.notify();
            return;
        }
        if self.draft_contents.is_empty() {
            self.notice = Some(Notice::Error("请至少添加一张图片或一段文字".to_owned()));
            cx.notify();
            return;
        }
        let Some(inbox_id) = self.inbox_id else {
            self.notice = Some(Notice::Error("Inbox 尚未准备好".to_owned()));
            cx.notify();
            return;
        };
        let name = optional_input_text(&self.name_input.read(cx).text());
        let description = optional_input_text(&self.description_input.read(cx).text());
        let tags = parse_tags(&self.tags_input.read(cx).text());
        let collector_ids = self
            .draft_contents
            .iter()
            .filter_map(DraftContent::collector_item_id)
            .collect::<Vec<_>>();
        let collector_draft = !collector_ids.is_empty();
        if collector_draft && collector_ids.len() != self.draft_contents.len() {
            self.notice = Some(Notice::Error(
                "Collector 内容与普通草稿不能混合保存".to_owned(),
            ));
            cx.notify();
            return;
        }

        let Some(database) = self.database.as_mut() else {
            self.notice = Some(Notice::Error("数据库尚未打开".to_owned()));
            cx.notify();
            return;
        };
        let (result, tags_to_attach) = if collector_draft {
            (
                database.promote_collector_items(
                    inbox_id,
                    collector_ids,
                    NewMemeFromCollector {
                        name,
                        description,
                        tags,
                    },
                ),
                Vec::new(),
            )
        } else {
            let contents = self
                .draft_contents
                .iter()
                .map(|content| match content {
                    DraftContent::Image { path, .. } => NewMemeContent::Image {
                        source_path: path.clone(),
                    },
                    DraftContent::Text { text, .. } => NewMemeContent::Text { text: text.clone() },
                    DraftContent::Motion { .. } => {
                        unreachable!("motion drafts always originate from Collector")
                    }
                })
                .collect();
            (
                database.create_meme(
                    inbox_id,
                    NewMeme {
                        name,
                        description,
                        contents,
                    },
                ),
                tags,
            )
        };
        let meme = match result {
            Ok(meme) => meme,
            Err(error) => {
                let message = if matches!(&error, memelith_core::Error::DuplicateCollectorItem(_)) {
                    match self.refresh_library() {
                        Ok(()) => format!("保存失败：{error}；Collector 重复状态已刷新"),
                        Err(refresh_error) => {
                            format!("保存失败：{error}；刷新 Collector 失败：{refresh_error}")
                        }
                    }
                } else {
                    format!("保存失败：{error}")
                };
                self.notice = Some(Notice::Error(message));
                cx.notify();
                return;
            }
        };

        let mut tag_error = None;
        for tag_name in tags_to_attach {
            let tag = match database.list_tags().and_then(|known| {
                if let Some(tag) = known
                    .into_iter()
                    .find(|tag| tag.name.eq_ignore_ascii_case(&tag_name))
                {
                    Ok(tag)
                } else {
                    database.create_tag(NewTag { name: tag_name })
                }
            }) {
                Ok(tag) => tag,
                Err(error) => {
                    tag_error = Some(error.to_string());
                    break;
                }
            };
            if let Err(error) = database.attach_tag_to_meme(meme.id, tag.id) {
                tag_error = Some(error.to_string());
                break;
            }
        }

        self.reset_add_form(cx);
        if let Err(error) = self.refresh_library() {
            self.notice = Some(Notice::Error(format!(
                "Meme 已保存，但刷新列表失败：{error}"
            )));
        } else if let Some(error) = tag_error {
            self.notice = Some(Notice::Error(format!(
                "Meme 已保存，但部分 Tag 未关联：{error}"
            )));
            self.page = Page::All;
        } else {
            self.notice = Some(Notice::Success("Meme 已保存到 Inbox".to_owned()));
            self.page = Page::All;
        }
        cx.notify();
    }

    fn reset_add_form(&mut self, cx: &mut Context<Self>) {
        self.draft_contents.clear();
        self.active_draft_image = 0;
        self.composing_text = false;
        for input in [
            &self.name_input,
            &self.description_input,
            &self.tags_input,
            &self.text_content_input,
        ] {
            input.update(cx, |input, cx| input.reset(cx));
        }
    }

    fn draft_uses_collector(&self) -> bool {
        self.draft_contents
            .iter()
            .any(|content| content.collector_item_id().is_some())
    }

    fn refresh_library(&mut self) -> Result<(), memelith_core::Error> {
        let database = self.database.as_mut().ok_or_else(|| {
            memelith_core::Error::InvalidDatabase("database is not open".to_owned())
        })?;
        let packs = database.list_meme_packs()?;
        let memes = database.list_all_memes()?;
        let tags = database.list_tags()?;
        let collector_items = database.recheck_collector_items()?;
        self.pack_names = packs
            .iter()
            .map(|pack| (pack.id, pack.name.clone()))
            .collect();
        self.meme_packs = packs;
        self.memes = memes;
        self.tags = tags;
        self.apply_collector_items(collector_items);
        Ok(())
    }

    fn refresh_collector(&mut self) -> Result<(), memelith_core::Error> {
        let database = self.database.as_ref().ok_or_else(|| {
            memelith_core::Error::InvalidDatabase("database is not open".to_owned())
        })?;
        let collector_items = database.list_collector_items()?;
        self.apply_collector_items(collector_items);
        Ok(())
    }

    fn apply_collector_items(&mut self, collector_items: Vec<CollectorItem>) {
        self.collector_items = collector_items;
        self.collector_refresh_pending = false;
        let selectable_ids = self
            .collector_items
            .iter()
            .filter(|item| item.duplicate.is_none())
            .map(|item| item.id)
            .collect::<HashSet<_>>();
        self.selected_collector_items
            .retain(|id| selectable_ids.contains(id));
    }

    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        if matches!(page, Page::Collector | Page::All | Page::MemePacks)
            && !self.analyzing_images
            && !self.collecting
            && let Err(error) = self.refresh_library()
        {
            self.notice = Some(Notice::Error(format!("无法刷新 Meme：{error}")));
        }
        self.page = page;
        cx.notify();
    }

    fn show_meme_pack(&mut self, pack_name: &str, cx: &mut Context<Self>) {
        let escaped_name = pack_name.replace('\\', "\\\\").replace('"', "\\\"");
        let query = format!("pack:\"={escaped_name}\"");
        self.meme_search_input
            .update(cx, |input, cx| input.set_text(query, cx));
        self.navigate(Page::All, cx);
    }

    fn check_sticker_pack_update(&mut self, pack_id: Uuid, cx: &mut Context<Self>) {
        if self.telegram_sticker_syncing.contains(&pack_id) {
            return;
        }
        let Some(pack) = self.meme_packs.iter().find(|pack| pack.id == pack_id) else {
            self.notice = Some(Notice::Error("MemePack 已不存在".to_owned()));
            cx.notify();
            return;
        };
        let Some(source) = pack.source.clone() else {
            self.notice = Some(Notice::Error("该 MemePack 没有 Telegram 来源".to_owned()));
            cx.notify();
            return;
        };
        if !telegram::is_sticker_pack_source(&source) {
            self.notice = Some(Notice::Error("该来源不是 Telegram 贴纸包链接".to_owned()));
            cx.notify();
            return;
        }
        let Some(runtime) = self.telegram_runtime.as_ref() else {
            self.notice = Some(Notice::Error(
                "请先在设置中启用并启动 Telegram Bot".to_owned(),
            ));
            cx.notify();
            return;
        };
        match runtime.sync_sticker_pack(pack_id, source) {
            Ok(()) => {
                self.telegram_sticker_syncing.insert(pack_id);
                self.notice = Some(Notice::Info("正在检查贴纸包更新".to_owned()));
            }
            Err(error) => {
                self.notice = Some(Notice::Error(format!("无法检查贴纸包更新：{error}")));
            }
        }
        cx.notify();
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        window.focus_next();
    }

    fn focus_previous(&mut self, _: &FocusPrevious, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    fn render_model_setup(&self, cx: &mut Context<Self>) -> AnyElement {
        let (title, detail, progress, progress_label) = match &self.model_setup_state {
            ModelSetupState::Checking => (
                "正在准备本地模型".to_owned(),
                "正在检查已下载文件；首次运行需要下载约 992 MiB。".to_owned(),
                None,
                None,
            ),
            ModelSetupState::Downloading(progress) => {
                let action = match progress.phase {
                    model_download::ModelInstallPhase::Checking => "正在校验",
                    model_download::ModelInstallPhase::Downloading => "正在下载",
                };
                (
                    "正在准备本地模型".to_owned(),
                    format!("{action} {}", progress.asset_label),
                    Some(progress.fraction()),
                    Some(format!(
                        "{} / {}",
                        format_download_bytes(progress.completed_bytes),
                        format_download_bytes(progress.total_bytes)
                    )),
                )
            }
            ModelSetupState::Failed(error) => {
                ("模型下载失败".to_owned(), error.clone(), None, None)
            }
            ModelSetupState::Ready => (
                "模型已准备完成".to_owned(),
                "正在打开资料库。".to_owned(),
                Some(1.0),
                None,
            ),
        };
        let failed = matches!(self.model_setup_state, ModelSetupState::Failed(_));

        div()
            .id("model-setup-root")
            .size_full()
            .bg(rgba(CONTENT_BG))
            .text_color(rgb(INK))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                div().flex_1().flex().items_center().justify_center().child(
                    div()
                        .w(px(520.))
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_4()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(ACCENT))
                                .child(APPLICATION_NAME.to_uppercase()),
                        )
                        .child(
                            div()
                                .text_size(px(28.))
                                .font_weight(FontWeight::BOLD)
                                .child(title),
                        )
                        .child(
                            div()
                                .w_full()
                                .text_sm()
                                .line_height(px(22.))
                                .text_color(if failed { rgb(DANGER) } else { rgba(LABEL_2) })
                                .child(detail),
                        )
                        .when_some(progress, |element, fraction| {
                            element.child(
                                div()
                                    .w_full()
                                    .h(px(6.))
                                    .rounded_full()
                                    .overflow_hidden()
                                    .bg(rgba(0x3c3c4314))
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(fraction))
                                            .rounded_full()
                                            .bg(rgb(ACCENT)),
                                    ),
                            )
                        })
                        .when_some(progress_label, |element, label| {
                            element.child(
                                div()
                                    .w_full()
                                    .text_xs()
                                    .text_color(rgba(LABEL_3))
                                    .child(label),
                            )
                        })
                        .when(failed, |element| {
                            element.child(
                                primary_pill("retry-model-setup")
                                    .on_click(cx.listener(Self::retry_model_setup))
                                    .child("重试下载"),
                            )
                        }),
                ),
            )
            .into_any_element()
    }

    fn render_onboarding(&self, cx: &mut Context<Self>) -> AnyElement {
        let button_label = if self.opening_storage {
            "正在打开…"
        } else {
            "选择存储位置"
        };
        div()
            .id("onboarding-root")
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .size_full()
            .bg(rgba(CONTENT_BG))
            .text_color(rgb(INK))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                div().flex_1().flex().items_center().justify_center().child(
                    glass_card()
                        .rounded(px(20.))
                        .w(px(540.))
                        .p_10()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_5()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(ACCENT))
                                .child(APPLICATION_NAME.to_uppercase()),
                        )
                        .child(
                            div()
                                .text_size(px(30.))
                                .font_weight(FontWeight::BOLD)
                                .child("先为 Meme 找一个家"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .line_height(px(22.))
                                .text_color(rgba(LABEL_2))
                                .child(
                                    "数据库、图片与文字都会保存在你选择的文件夹中，之后也可以在设置里更换。",
                                ),
                        )
                        .when_some(self.notice.as_ref(), |element, notice| {
                            element.child(render_notice(notice))
                        })
                        .child(
                            primary_pill("choose-storage-onboarding")
                                .on_click(cx.listener(Self::choose_storage))
                                .child(button_label),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let storage_label: SharedString = self
            .storage_root
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| "未选择".into());
        div()
            .w(px(224.))
            .h_full()
            .flex_none()
            .bg(rgba(SIDEBAR_GLASS))
            .border_r_1()
            .border_color(rgba(0x0000000d))
            .flex()
            .flex_col()
            .child(
                // 预留 macOS 红绿灯区域，同时作为窗口拖拽区
                div()
                    .h(px(56.))
                    .flex_none()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                div()
                    .px_5()
                    .pb_3()
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .window_control_area(WindowControlArea::Drag)
                    .child(APPLICATION_NAME),
            )
            .child(
                div().px_4().flex().flex_col().gap_1().children(
                    [
                        Page::Collector,
                        Page::Add,
                        Page::All,
                        Page::MemePacks,
                        Page::Settings,
                    ]
                    .map(|page| {
                        let selected = self.page == page;
                        div()
                            .id(("nav", page.index()))
                            .h(px(32.))
                            .px_3()
                            .rounded(px(9.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .tab_index(0)
                            .focus(|style| {
                                style
                                    .bg(rgba(GLASS_STRONG))
                                    .border_1()
                                    .border_color(rgb(ACCENT))
                            })
                            .text_size(px(13.))
                            .text_color(rgb(INK))
                            .when(selected, |style| {
                                style
                                    .bg(rgba(GLASS_STRONG))
                                    .shadow(control_shadow())
                                    .font_weight(FontWeight::SEMIBOLD)
                            })
                            .when(!selected, |style| {
                                style.hover(|style| style.bg(rgba(0xffffff73)))
                            })
                            .active(|style| style.bg(rgba(0xffffffff)))
                            .on_click(cx.listener(move |view, _, _, cx| view.navigate(page, cx)))
                            .child(
                                div()
                                    .w(px(20.))
                                    .text_center()
                                    .text_color(rgb(ACCENT))
                                    .child(page.icon()),
                            )
                            .child(page.label())
                    }),
                ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px_5()
                    .pb_4()
                    .text_xs()
                    .text_color(rgba(LABEL_3))
                    .truncate()
                    .child(storage_label),
            )
            .into_any_element()
    }

    fn render_header(&self) -> AnyElement {
        let (title, subtitle): (SharedString, SharedString) = match self.page {
            Page::Collector => (
                "Collector".into(),
                format!("快速收集 · {} 项", self.collector_items.len()).into(),
            ),
            Page::Add => (
                "添加 Meme".into(),
                if self.draft_uses_collector() {
                    "补充可选信息后保存到 Inbox".to_owned().into()
                } else {
                    "保存后会进入默认的 Inbox".to_owned().into()
                },
            ),
            Page::All => (
                "全部 Meme".into(),
                format!("共 {} 个", self.memes.len()).into(),
            ),
            Page::MemePacks => (
                "MemePack".into(),
                format!("共 {} 个", self.meme_packs.len()).into(),
            ),
            Page::Settings => (
                "设置".into(),
                "管理本地存储与 Telegram Bot".to_owned().into(),
            ),
        };
        div()
            .flex_none()
            .px_8()
            .pt_6()
            .pb_4()
            .flex()
            .flex_col()
            .gap_1()
            .window_control_area(WindowControlArea::Drag)
            .child(
                div()
                    .text_size(px(26.))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            )
            .child(div().text_sm().text_color(rgba(LABEL_2)).child(subtitle))
            .into_any_element()
    }

    fn render_collector_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let duplicate_count = self
            .collector_items
            .iter()
            .filter(|item| item.duplicate.is_some())
            .count();
        let visible_items = self
            .collector_items
            .iter()
            .filter(|item| !self.show_only_collector_duplicates || item.duplicate.is_some())
            .collect::<Vec<_>>();
        let editing_collector_draft = self.draft_uses_collector();
        let selected_count = if editing_collector_draft {
            self.draft_contents.len()
        } else {
            self.selected_collector_items.len()
        };
        let processing = self.collecting || self.analyzing_images || self.detecting_characters;

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_8()
                    .pb_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        glass_group().child(
                            group_row()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(self.collector_text_input.clone()),
                                )
                                .child(
                                    glass_pill("collect-text")
                                        .when(processing, |button| {
                                            button.opacity(0.45).cursor_default().tab_stop(false)
                                        })
                                        .when(!processing, |button| {
                                            button
                                                .on_click(cx.listener(Self::collect_collector_text))
                                        })
                                        .child("收集文字"),
                                )
                                .child(
                                    primary_pill("collect-images")
                                        .when(processing, |button| {
                                            button.opacity(0.45).cursor_default().tab_stop(false)
                                        })
                                        .when(!processing, |button| {
                                            button.on_click(
                                                cx.listener(Self::choose_collector_images),
                                            )
                                        })
                                        .child(if self.collecting {
                                            "正在收集…"
                                        } else {
                                            "选择图片"
                                        }),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .h(px(32.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .id("collector-duplicates-filter")
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .cursor_pointer()
                                    .tab_index(0)
                                    .focus(|style| style.rounded(px(6.)).bg(rgba(0x007aff14)))
                                    .on_click(cx.listener(Self::toggle_collector_filter))
                                    .child(checkbox(
                                        "collector-duplicates-checkbox",
                                        self.show_only_collector_duplicates,
                                    ))
                                    .child(
                                        div().text_sm().text_color(rgb(INK)).child("只显示重复项"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgba(LABEL_3))
                                            .child(duplicate_count.to_string()),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(div().text_sm().text_color(rgba(LABEL_2)).child(
                                if editing_collector_draft {
                                    format!("草稿 {selected_count} 项")
                                } else {
                                    format!("已选 {selected_count} 项")
                                },
                            ))
                            .child(
                                primary_pill("promote-collector-items")
                                    .when(
                                        selected_count == 0
                                            || processing
                                            || (self.show_only_collector_duplicates
                                                && !editing_collector_draft),
                                        |button| {
                                            button.opacity(0.45).cursor_default().tab_stop(false)
                                        },
                                    )
                                    .when(
                                        selected_count > 0
                                            && !processing
                                            && (!self.show_only_collector_duplicates
                                                || editing_collector_draft),
                                        |button| {
                                            button.on_click(
                                                cx.listener(Self::add_selected_collector_items),
                                            )
                                        },
                                    )
                                    .child(if editing_collector_draft {
                                        "查看草稿"
                                    } else {
                                        "添加"
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("collector-page-scroll")
                    .min_h_0()
                    .flex_1()
                    .overflow_y_scroll()
                    .px_8()
                    .pb_8()
                    .when(visible_items.is_empty(), |element| {
                        element.child(
                            div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(17.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgba(LABEL_2))
                                        .child(if self.show_only_collector_duplicates {
                                            "没有重复项"
                                        } else {
                                            "Collector 为空"
                                        }),
                                )
                                .child(div().text_size(px(13.)).text_color(rgba(LABEL_3)).child(
                                    if self.show_only_collector_duplicates {
                                        "当前没有需要处理的重复内容。"
                                    } else {
                                        "选择图片或收集文字。"
                                    },
                                )),
                        )
                    })
                    .child(
                        div().flex().flex_wrap().gap_4().children(
                            visible_items
                                .into_iter()
                                .map(|item| self.render_collector_item(item, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_collector_item(&self, item: &CollectorItem, cx: &mut Context<Self>) -> AnyElement {
        let item_id = item.id;
        let selected = self.selected_collector_items.contains(&item_id);
        let in_collector_draft = selected && self.draft_uses_collector();
        let (warning_color, warning_label) = match &item.duplicate {
            Some(CollectorDuplicate::Hash { target }) => {
                let target_label = target
                    .meme_name
                    .as_deref()
                    .map(|name| format!("Hash 重复 · {name}"))
                    .unwrap_or_else(|| "Hash 重复".to_owned());
                (Some(DANGER), Some(target_label))
            }
            Some(CollectorDuplicate::Similarity {
                target,
                cosine_distance,
            }) => {
                let similarity = ((1.0 - cosine_distance).clamp(0.0, 1.0) * 100.0) as f64;
                let target_label = target
                    .meme_name
                    .as_deref()
                    .map(|name| format!("疑似与「{name}」重复 · {similarity:.1}%"))
                    .unwrap_or_else(|| format!("疑似重复 · {similarity:.1}%"));
                (Some(WARNING), Some(target_label))
            }
            None => (None, None),
        };
        let preview = match &item.content {
            CollectorContent::Image { relative_path, .. } => self
                .storage_root
                .as_ref()
                .and_then(|root| {
                    MemeDatabase::resolve_media_path_from_root(root, relative_path).ok()
                })
                .map(|path| {
                    div()
                        .size_full()
                        .bg(rgba(0x3c3c4314))
                        .child(img(path).size_full().object_fit(ObjectFit::Cover))
                        .into_any_element()
                })
                .unwrap_or_else(|| {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(rgba(LABEL_3))
                        .child("无法预览")
                        .into_any_element()
                }),
            CollectorContent::Motion {
                preview_relative_path,
                ..
            } => preview_relative_path
                .as_ref()
                .and_then(|relative_path| {
                    self.storage_root.as_ref().and_then(|root| {
                        MemeDatabase::resolve_media_path_from_root(root, relative_path).ok()
                    })
                })
                .map(|path| {
                    div()
                        .size_full()
                        .bg(rgba(0x3c3c4314))
                        .child(img(path).size_full().object_fit(ObjectFit::Cover))
                        .into_any_element()
                })
                .unwrap_or_else(|| {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(rgba(LABEL_3))
                        .child("无法预览")
                        .into_any_element()
                }),
            CollectorContent::Text { text } => div()
                .size_full()
                .p_4()
                .overflow_hidden()
                .bg(rgba(0x5856d61a))
                .text_color(rgb(0x4543b8))
                .text_size(px(15.))
                .line_height(px(22.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(text.clone())
                .into_any_element(),
        };

        glass_card()
            .id(SharedString::from(format!("collector-item-{item_id}")))
            .relative()
            .w(px(210.))
            .h(px(220.))
            .overflow_hidden()
            .cursor_pointer()
            .tab_index(0)
            .focus(|style| style.border_2().border_color(rgb(ACCENT)))
            .when(selected, |card| card.border_2().border_color(rgb(ACCENT)))
            .when_some(warning_color, |card, color| {
                card.border_2().border_color(rgb(color))
            })
            .when(!selected && warning_color.is_none(), |card| {
                card.hover(|style| style.bg(rgba(GLASS_STRONG)))
            })
            .on_click(cx.listener(move |view, event: &gpui::ClickEvent, _, cx| {
                if event.standard_click() {
                    view.toggle_collector_item(item_id, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, event, window, cx| {
                    view.open_collector_context_menu(item_id, event, window, cx)
                }),
            )
            .child(div().h(px(174.)).w_full().overflow_hidden().child(preview))
            .child(
                div().h(px(46.)).px_3().flex().items_center().child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(warning_color.map(rgb).unwrap_or_else(|| rgba(LABEL_2)))
                        .truncate()
                        .child(warning_label.unwrap_or_else(|| match &item.content {
                            CollectorContent::Image { .. } | CollectorContent::Motion { .. } => {
                                if in_collector_draft {
                                    "已在草稿 · 图片".to_owned()
                                } else {
                                    "图片".to_owned()
                                }
                            }
                            CollectorContent::Text { .. } => {
                                if in_collector_draft {
                                    "已在草稿 · 文字".to_owned()
                                } else {
                                    "文字".to_owned()
                                }
                            }
                        })),
                ),
            )
            .when(item.duplicate.is_none(), |card| {
                card.child(
                    div()
                        .absolute()
                        .top(px(10.))
                        .right(px(10.))
                        .p(px(4.))
                        .rounded(px(7.))
                        .bg(rgba(0xffffffd9))
                        .shadow(control_shadow())
                        .child(checkbox(
                            SharedString::from(format!("collector-select-{item_id}")),
                            selected,
                        )),
                )
            })
            .into_any_element()
    }

    fn render_collector_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.collector_context_menu?;
        let item = self
            .collector_items
            .iter()
            .find(|item| item.id == state.item_id)?;
        let item_id = state.item_id;
        let mut menu = context_menu()
            .id("collector-context-menu")
            .on_mouse_down_out(cx.listener(Self::close_collector_context_menu));
        if matches!(item.duplicate, Some(CollectorDuplicate::Similarity { .. })) {
            menu = menu.child(
                context_menu_item("collector-not-duplicate", "这不是重复").on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.dismiss_collector_similarity(item_id, window, cx)
                    },
                )),
            );
        }
        let menu = menu.child(
            context_menu_item("collector-delete", "删除")
                .text_color(rgb(DANGER))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.request_delete_collector_item(item_id, window, cx)
                })),
        );
        Some(
            deferred(
                anchored()
                    .position(state.position)
                    .anchor(Corner::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    fn render_add_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let image_processing = self.analyzing_images || self.detecting_characters;
        let collector_draft = self.draft_uses_collector();
        let has_images = self.draft_contents.iter().any(DraftContent::is_image);
        let has_contents = !self.draft_contents.is_empty();
        let mut content_group = glass_group().flex().flex_col();
        if !has_contents && !self.composing_text {
            content_group = content_group.child(self.render_empty_content_picker(cx));
        } else {
            if has_images {
                content_group = content_group.child(self.render_draft_image_gallery(cx));
            }
            for (index, content) in self.draft_contents.iter().enumerate() {
                let DraftContent::Text { text, .. } = content else {
                    continue;
                };
                content_group = content_group
                    .when(has_images || index > 0, |group| group.child(hairline()))
                    .child(self.render_draft_text(index, text, cx));
            }
            if self.composing_text {
                content_group = content_group
                    .when(has_contents, |group| group.child(hairline()))
                    .child(self.render_text_composer(cx));
            } else if !collector_draft || has_images {
                content_group = content_group
                    .child(hairline())
                    .child(self.render_content_toolbar(has_images, collector_draft, cx));
            }
        }

        div()
            .size_full()
            .id("add-page-scroll")
            .overflow_y_scroll()
            .px_8()
            .pb_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(section_header("内容"))
                            .child(content_group),
                    )
                    .child(
                        div().flex().flex_col().child(section_header("信息")).child(
                            glass_group()
                                .flex()
                                .flex_col()
                                .child(field_row("名称", self.name_input.clone()))
                                .child(hairline())
                                .child(field_row("简介", self.description_input.clone()))
                                .child(hairline())
                                .child(self.render_tag_field(cx)),
                        ),
                    )
                    .child(
                        div().flex().justify_end().child(
                            primary_pill("save-meme")
                                .when(image_processing, |button| {
                                    button.opacity(0.45).cursor_default().tab_stop(false)
                                })
                                .when(!image_processing, |button| {
                                    button.on_click(cx.listener(Self::save_meme))
                                })
                                .child(if self.analyzing_images {
                                    "正在分析图片…"
                                } else if self.detecting_characters {
                                    "正在识别角色…"
                                } else {
                                    "保存到 Inbox"
                                }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_empty_content_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let processing = self.analyzing_images || self.detecting_characters || self.collecting;
        let image_label = if self.analyzing_images {
            "正在分析图片…"
        } else {
            "选择图片"
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("choose-images-empty")
                    .h(px(220.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .bg(rgba(0x007aff0a))
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .tab_index(0)
                    .focus(|style| style.bg(rgba(0x007aff16)))
                    .hover(|style| style.bg(rgba(0x007aff10)))
                    .active(|style| style.bg(rgba(0x007aff1c)))
                    .when(processing, |button| {
                        button.opacity(0.5).cursor_default().tab_stop(false)
                    })
                    .when(!processing, |button| {
                        button.on_click(cx.listener(Self::choose_images))
                    })
                    .child(
                        div()
                            .size(px(48.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x007aff14))
                            .text_size(px(28.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("＋"),
                    )
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(image_label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgba(LABEL_3))
                            .child("可一次选择多张"),
                    ),
            )
            .child(hairline())
            .child(
                div()
                    .h(px(52.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgba(LABEL_2))
                            .child("不需要图片？"),
                    )
                    .child(
                        content_action_button("begin-text-empty", false)
                            .when(processing, |button| {
                                button.opacity(0.45).cursor_default().tab_stop(false)
                            })
                            .when(!processing, |button| {
                                button.on_click(cx.listener(Self::begin_text_composer))
                            })
                            .child("Aa")
                            .child("添加文字"),
                    ),
            )
            .into_any_element()
    }

    fn render_text_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_text = !self.text_content_input.read(cx).text().trim().is_empty();
        div()
            .w_full()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_3()
            .key_context("TextComposer")
            .on_action(cx.listener(Self::confirm_text_content))
            .on_action(cx.listener(Self::cancel_text_composer_action))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("添加文字"),
            )
            .child(self.text_content_input.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        content_action_button("cancel-text-content", false)
                            .on_click(cx.listener(Self::cancel_text_composer))
                            .child("取消"),
                    )
                    .child(
                        content_action_button("confirm-text-content", true)
                            .when(!has_text, |button| {
                                button.opacity(0.45).cursor_default().tab_stop(false)
                            })
                            .when(has_text, |button| {
                                button.on_click(cx.listener(Self::add_text_content))
                            })
                            .child("添加"),
                    ),
            )
            .into_any_element()
    }

    fn render_content_toolbar(
        &self,
        has_images: bool,
        collector_draft: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let processing = self.analyzing_images || self.detecting_characters || self.collecting;
        let image_label = if self.analyzing_images {
            "正在分析…"
        } else {
            "图片"
        };
        let character_label = if self.detecting_characters {
            "正在识别…"
        } else {
            "识别角色"
        };
        div()
            .min_h(px(52.))
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .when(!collector_draft, |toolbar| {
                toolbar
                    .child(
                        content_action_button("choose-more-images", true)
                            .when(processing, |button| {
                                button.opacity(0.45).cursor_default().tab_stop(false)
                            })
                            .when(!processing, |button| {
                                button.on_click(cx.listener(Self::choose_images))
                            })
                            .child("＋")
                            .child(image_label),
                    )
                    .child(
                        content_action_button("begin-text-content", false)
                            .when(processing, |button| {
                                button.opacity(0.45).cursor_default().tab_stop(false)
                            })
                            .when(!processing, |button| {
                                button.on_click(cx.listener(Self::begin_text_composer))
                            })
                            .child("Aa")
                            .child("文字"),
                    )
            })
            .child(div().flex_1())
            .when(has_images, |toolbar| {
                toolbar.child(
                    content_action_button("detect-characters", false)
                        .when(processing, |button| {
                            button.opacity(0.45).cursor_default().tab_stop(false)
                        })
                        .when(!processing, |button| {
                            button.on_click(cx.listener(Self::detect_characters))
                        })
                        .child(character_label),
                )
            })
            .into_any_element()
    }

    fn render_tag_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let input_text = self.tags_input.read(cx).text();
        let suggestions = tag_suggestions(&input_text, &self.tags);
        let mut input_column = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.tags_input.clone());

        if !suggestions.is_empty() {
            let mut suggestion_list = div()
                .w_full()
                .rounded(px(8.))
                .overflow_hidden()
                .border_1()
                .border_color(rgba(EDGE_DARK))
                .bg(rgb(0xffffff))
                .shadow(control_shadow());
            for (index, tag) in suggestions.into_iter().enumerate() {
                let tag_name = tag.name.clone();
                suggestion_list = suggestion_list.child(
                    div()
                        .id(("tag-suggestion", index))
                        .h(px(30.))
                        .px_3()
                        .flex()
                        .items_center()
                        .text_sm()
                        .cursor_pointer()
                        .tab_index(0)
                        .focus(|style| style.bg(rgba(0x007aff1f)))
                        .hover(|style| style.bg(rgba(0x007aff12)))
                        .when(index > 0, |row| row.border_t_1().border_color(rgba(SEP)))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.complete_tag(&tag_name, window, cx)
                        }))
                        .child(tag.name.clone()),
                );
            }
            input_column = input_column.child(suggestion_list);
        }

        group_row()
            .items_start()
            .child(
                div()
                    .w(px(64.))
                    .flex_none()
                    .pt(px(5.))
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Tag"),
            )
            .child(input_column)
            .into_any_element()
    }

    fn complete_tag(&mut self, tag_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.tags_input.read(cx).text();
        let completed = complete_tag_input(&input, tag_name);
        self.tags_input
            .update(cx, |input, cx| input.set_text(completed, cx));
        window.focus(&self.tags_input.focus_handle(cx));
    }

    fn render_draft_image_gallery(&self, cx: &mut Context<Self>) -> AnyElement {
        let image_count = self
            .draft_contents
            .iter()
            .filter(|content| content.is_image())
            .count();
        let (draft_index, path, similar_images) = self
            .draft_contents
            .iter()
            .enumerate()
            .filter_map(|(index, content)| match content {
                DraftContent::Image {
                    path,
                    similar_images,
                    ..
                } => Some((index, Some(path.as_path()), similar_images.as_slice())),
                DraftContent::Motion { preview_path, .. } => {
                    Some((index, preview_path.as_deref(), &[][..]))
                }
                DraftContent::Text { .. } => None,
            })
            .nth(self.active_draft_image)
            .expect("active draft image must refer to an existing image");
        let file_name = path.map_or_else(
            || "无法预览".to_owned(),
            |path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
            },
        );
        let preview = match path {
            Some(path) => img(path.to_path_buf())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgba(LABEL_3))
                .child("无法预览")
                .into_any_element(),
        };

        let mut gallery = div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(360.))
                    .overflow_hidden()
                    .bg(rgba(0x3c3c430a))
                    .child(div().absolute().inset_0().p_4().child(preview))
                    .when(image_count > 1, |viewport| {
                        viewport.child(
                            div()
                                .absolute()
                                .inset_0()
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    carousel_button("previous-draft-image")
                                        .on_click(cx.listener(Self::show_previous_draft_image))
                                        .child("‹"),
                                )
                                .child(
                                    carousel_button("next-draft-image")
                                        .on_click(cx.listener(Self::show_next_draft_image))
                                        .child("›"),
                                ),
                        )
                    })
                    .child(
                        div().absolute().top(px(12.)).right(px(12.)).child(
                            carousel_button("remove-active-draft-image")
                                .when(self.detecting_characters, |button| {
                                    button.opacity(0.45).cursor_default().tab_stop(false)
                                })
                                .when(!self.detecting_characters, |button| {
                                    button.on_click(cx.listener(move |view, _, _, cx| {
                                        view.remove_draft_content(draft_index, cx)
                                    }))
                                })
                                .child("✕"),
                        ),
                    ),
            )
            .child(
                div()
                    .h(px(38.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(rgba(LABEL_3))
                            .child(file_name),
                    )
                    .when(image_count > 1, |footer| {
                        footer.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgba(LABEL_2))
                                .child(format!("{} / {image_count}", self.active_draft_image + 1)),
                        )
                    }),
            );

        if let Some(closest) = similar_images.first() {
            let similarity = ((1.0 - closest.cosine_distance).clamp(0.0, 1.0) * 100.0) as f64;
            let existing_name = closest.meme_name.as_deref().unwrap_or("未命名 Meme");
            let additional = similar_images.len().saturating_sub(1);
            let message = if additional == 0 {
                format!("疑似与「{existing_name}」重复 · 相似度 {similarity:.1}%")
            } else {
                format!(
                    "疑似与「{existing_name}」重复 · 相似度 {similarity:.1}% · 另有 {additional} 项"
                )
            };
            gallery = gallery.child(
                div()
                    .px_4()
                    .pb_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(rgb(WARNING))
                    .child(div().size(px(6.)).rounded_full().bg(rgb(WARNING)))
                    .child(message),
            );
        }

        gallery.into_any_element()
    }

    fn render_draft_text(&self, index: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let preview = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size(px(44.))
                    .flex_none()
                    .rounded(px(10.))
                    .bg(rgba(0x5856d61f))
                    .text_color(rgb(ACCENT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_weight(FontWeight::BOLD)
                    .child("Aa"),
            )
            .child(div().flex_1().text_sm().truncate().child(text.to_owned()));
        group_row()
            .id(("draft-content", index))
            .child(div().flex_1().min_w_0().child(preview))
            .child(
                icon_button(("remove-draft", index))
                    .when(self.detecting_characters, |button| {
                        button.opacity(0.45).cursor_default().tab_stop(false)
                    })
                    .when(!self.detecting_characters, |button| {
                        button.on_click(
                            cx.listener(move |view, _, _, cx| view.remove_draft_content(index, cx)),
                        )
                    })
                    .child("✕"),
            )
            .into_any_element()
    }

    fn render_all_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.meme_search_input.read(cx).text();
        let search_error = self.meme_search.as_ref().err().map(ToString::to_string);
        let visible_memes = self
            .memes
            .iter()
            .enumerate()
            .filter(|(_, meme)| {
                self.meme_search.as_ref().is_ok_and(|expression| {
                    expression.as_ref().is_none_or(|expression| {
                        expression.matches(
                            meme,
                            self.pack_names.get(&meme.meme_pack_id).map(String::as_str),
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        let has_no_matches =
            search_error.is_none() && !query.trim().is_empty() && visible_memes.is_empty();

        div()
            .size_full()
            .id("all-page-scroll")
            .overflow_y_scroll()
            .px_8()
            .pb_8()
            .flex()
            .flex_col()
            .child(
                div()
                    .w(px(520.))
                    .max_w_full()
                    .flex_none()
                    .pb_5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.meme_search_input.clone())
                    .when_some(search_error, |element, error| {
                        element.child(div().px_2().text_xs().text_color(rgb(DANGER)).child(error))
                    }),
            )
            .when(self.memes.is_empty(), |element| {
                element.child(
                    div()
                        .w_full()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(17.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgba(LABEL_2))
                                .child("没有 Meme"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgba(LABEL_3))
                                .child("从“添加”导入图片或文字，开始整理你的收藏。"),
                        )
                        .child(
                            div().pt_2().child(
                                glass_pill("empty-add")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.navigate(Page::Add, cx);
                                    }))
                                    .child("去添加"),
                            ),
                        ),
                )
            })
            .when(has_no_matches, |element| {
                element.child(
                    div()
                        .w_full()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(17.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgba(LABEL_2))
                                .child("没有匹配的 Meme"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgba(LABEL_3))
                                .child("尝试其他关键词。"),
                        ),
                )
            })
            .child(
                div().flex().flex_wrap().gap_5().children(
                    visible_memes
                        .into_iter()
                        .map(|(index, meme)| self.render_meme(index, meme)),
                ),
            )
            .into_any_element()
    }

    fn render_meme(&self, index: usize, meme: &Meme) -> AnyElement {
        let preview = meme
            .contents
            .iter()
            .find_map(|content| match content {
                MemeContent::Image(image) => self.storage_root.as_ref().and_then(|storage_root| {
                    MemeDatabase::resolve_media_path_from_root(storage_root, &image.relative_path)
                        .ok()
                        .map(|path| {
                            div()
                                .h(px(160.))
                                .w_full()
                                .overflow_hidden()
                                .bg(rgba(0x3c3c4314))
                                .child(img(path).size_full().object_fit(ObjectFit::Cover))
                                .into_any_element()
                        })
                }),
                MemeContent::Motion(motion) => {
                    motion
                        .preview_relative_path
                        .as_ref()
                        .and_then(|relative_path| {
                            self.storage_root.as_ref().and_then(|storage_root| {
                                MemeDatabase::resolve_media_path_from_root(
                                    storage_root,
                                    relative_path,
                                )
                                .ok()
                                .map(|path| {
                                    div()
                                        .h(px(160.))
                                        .w_full()
                                        .overflow_hidden()
                                        .bg(rgba(0x3c3c4314))
                                        .child(img(path).size_full().object_fit(ObjectFit::Cover))
                                        .into_any_element()
                                })
                            })
                        })
                }
                MemeContent::Text(_) => None,
            })
            .or_else(|| {
                meme.contents.iter().find_map(|content| match content {
                    MemeContent::Text(text) => Some(
                        div()
                            .h(px(160.))
                            .w_full()
                            .p_5()
                            .bg(rgba(0x5856d61a))
                            .text_color(rgb(0x4543b8))
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .child(text.text.clone())
                            .into_any_element(),
                    ),
                    MemeContent::Image(_) => None,
                    MemeContent::Motion(_) => Some(
                        div()
                            .h(px(160.))
                            .w_full()
                            .bg(rgba(0x3c3c4314))
                            .text_color(rgba(LABEL_2))
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("无法预览")
                            .into_any_element(),
                    ),
                })
            })
            .unwrap_or_else(|| {
                div()
                    .h(px(160.))
                    .w_full()
                    .bg(rgba(0x3c3c4314))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgba(LABEL_3))
                    .child("无法预览")
                    .into_any_element()
            });
        let title = meme
            .name
            .clone()
            .unwrap_or_else(|| "未命名 Meme".to_owned());
        let pack_name = self
            .pack_names
            .get(&meme.meme_pack_id)
            .cloned()
            .unwrap_or_else(|| "未知 MemePack".to_owned());
        glass_card()
            .id(("meme", index))
            .w(px(240.))
            .overflow_hidden()
            .cursor_pointer()
            .hover(|style| style.bg(rgba(GLASS_STRONG)))
            .child(preview)
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgba(LABEL_2))
                            .child(format!("{pack_name} · {} 个内容", meme.contents.len())),
                    )
                    .when_some(meme.description.as_ref(), |element, description| {
                        element.child(
                            div()
                                .text_xs()
                                .text_color(rgba(LABEL_2))
                                .truncate()
                                .child(description.clone()),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_meme_packs_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let meme_counts = self.memes.iter().fold(HashMap::new(), |mut counts, meme| {
            *counts.entry(meme.meme_pack_id).or_insert(0) += 1;
            counts
        });

        div()
            .size_full()
            .id("meme-packs-page-scroll")
            .overflow_y_scroll()
            .px_8()
            .pb_8()
            .when(self.meme_packs.is_empty(), |element| {
                element.child(
                    div()
                        .w_full()
                        .h_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(17.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgba(LABEL_2))
                                .child("没有 MemePack"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgba(LABEL_3))
                                .child("MemePack 会显示在这里。"),
                        ),
                )
            })
            .when(!self.meme_packs.is_empty(), |element| {
                element.child(
                    glass_group().children(self.meme_packs.iter().enumerate().map(
                        |(index, pack)| {
                            let meme_count = meme_counts.get(&pack.id).copied().unwrap_or(0);
                            let pack_name = pack.name.clone();
                            let sticker_source = pack
                                .source
                                .as_ref()
                                .filter(|source| telegram::is_sticker_pack_source(source))
                                .cloned();
                            let pack_id = pack.id;
                            let syncing = self.telegram_sticker_syncing.contains(&pack_id);
                            div()
                                .flex()
                                .flex_col()
                                .when(index > 0, |element| element.child(hairline()))
                                .child(
                                    group_row()
                                        .id(("meme-pack", index))
                                        .cursor_pointer()
                                        .tab_index(0)
                                        .focus(|style| style.bg(rgba(0x007aff1f)))
                                        .hover(|style| style.bg(rgba(0x007aff12)))
                                        .active(|style| style.bg(rgba(0x007aff26)))
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.show_meme_pack(&pack_name, cx);
                                        }))
                                        .child(
                                            div()
                                                .size(px(38.))
                                                .flex_none()
                                                .rounded(px(9.))
                                                .bg(rgba(0x007aff14))
                                                .text_color(rgb(ACCENT))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .font_weight(FontWeight::BOLD)
                                                .child("▤"),
                                        )
                                        .child(
                                            div()
                                                .min_w_0()
                                                .flex_1()
                                                .flex()
                                                .flex_col()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .truncate()
                                                        .child(pack.name.clone()),
                                                )
                                                .when_some(
                                                    pack.description.as_ref(),
                                                    |element, description| {
                                                        element.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgba(LABEL_2))
                                                                .truncate()
                                                                .child(description.clone()),
                                                        )
                                                    },
                                                )
                                                .when_some(
                                                    sticker_source.as_ref(),
                                                    |element, source| {
                                                        element.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(ACCENT))
                                                                .truncate()
                                                                .child(source.clone()),
                                                        )
                                                    },
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .flex()
                                                .items_center()
                                                .gap_3()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgba(LABEL_2))
                                                        .child(format!("{meme_count} 个 Meme")),
                                                )
                                                .when(sticker_source.is_some(), |element| {
                                                    element.child(
                                                        glass_pill(SharedString::from(format!(
                                                            "sync-sticker-pack-{pack_id}"
                                                        )))
                                                        .when(syncing, |button| {
                                                            button
                                                                .opacity(0.55)
                                                                .cursor_default()
                                                                .tab_stop(false)
                                                        })
                                                        .when(!syncing, |button| {
                                                            button.on_click(cx.listener(
                                                                move |view, _, _, cx| {
                                                                    cx.stop_propagation();
                                                                    view.check_sticker_pack_update(
                                                                        pack_id, cx,
                                                                    );
                                                                },
                                                            ))
                                                        })
                                                        .child(if syncing {
                                                            "检查中…"
                                                        } else {
                                                            "检查更新"
                                                        }),
                                                    )
                                                }),
                                        ),
                                )
                        },
                    )),
                )
            })
            .into_any_element()
    }

    fn render_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let storage = self
            .storage_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "未选择".to_owned());
        let storage_section = div().flex().flex_col().child(section_header("存储")).child(
            glass_group().child(
                group_row()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("存储位置"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(LABEL_2))
                                    .truncate()
                                    .child(storage),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(LABEL_3))
                                    .child("更换位置会打开另一个资料库，不会自动迁移当前数据。"),
                            ),
                    )
                    .child(
                        glass_pill("change-storage")
                            .when(
                                self.analyzing_images
                                    || self.detecting_characters
                                    || self.collecting,
                                |button| button.opacity(0.45).cursor_default().tab_stop(false),
                            )
                            .when(
                                !self.analyzing_images
                                    && !self.detecting_characters
                                    && !self.collecting,
                                |button| button.on_click(cx.listener(Self::choose_storage)),
                            )
                            .child("更换位置"),
                    ),
            ),
        );
        let telegram_section = div()
            .flex()
            .flex_col()
            .child(section_header("Telegram Bot"))
            .child(
                glass_group()
                    .child(
                        group_row()
                            .id("telegram-enabled-row")
                            .cursor_pointer()
                            .tab_index(0)
                            .focus(|style| style.bg(rgba(0x007aff12)))
                            .hover(|style| style.bg(rgba(0x3c3c4308)))
                            .active(|style| style.bg(rgba(0x3c3c4312)))
                            .on_click(cx.listener(Self::toggle_telegram))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("接收 Telegram 图片"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgba(LABEL_2))
                                            .child(self.telegram_status_text(cx)),
                                    ),
                            )
                            .child(checkbox("telegram-enabled", self.telegram_enabled)),
                    )
                    .child(hairline())
                    .child(
                        group_row()
                            .items_start()
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_none()
                                    .pt_2()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Bot Token"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .child(self.telegram_token_input.clone()),
                                            )
                                            .child(
                                                glass_pill("toggle-telegram-token")
                                                    .on_click(cx.listener(
                                                        Self::toggle_telegram_token_visibility,
                                                    ))
                                                    .child(if self.telegram_token_revealed {
                                                        "隐藏"
                                                    } else {
                                                        "显示"
                                                    }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgba(LABEL_3))
                                            .child("由 BotFather 提供，保存在本机。"),
                                    )
                                    .when_some(
                                        self.telegram_token_error.as_ref(),
                                        |element, error| {
                                            element.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(DANGER))
                                                    .child(error.clone()),
                                            )
                                        },
                                    ),
                            ),
                    )
                    .child(
                        group_row().justify_end().child(
                            primary_pill("save-telegram-settings")
                                .on_click(cx.listener(Self::apply_telegram_settings))
                                .child("保存 Telegram 设置"),
                        ),
                    ),
            );

        div()
            .size_full()
            .id("settings-page-scroll")
            .overflow_y_scroll()
            .px_8()
            .pb_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
                    .flex()
                    .flex_col()
                    .gap_6()
                    .children([storage_section, telegram_section]),
            )
            .into_any_element()
    }
}

impl Drop for MemelithView {
    fn drop(&mut self) {
        if let Some(runtime) = self.telegram_runtime.take() {
            runtime.stop();
        }
    }
}

impl Render for MemelithView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.installed_models.is_none() {
            return self.render_model_setup(cx);
        }
        if self.storage_root.is_none() {
            return self.render_onboarding(cx);
        }

        let page = match self.page {
            Page::Collector => self.render_collector_page(cx),
            Page::Add => self.render_add_page(cx),
            Page::All => self.render_all_page(cx),
            Page::MemePacks => self.render_meme_packs_page(cx),
            Page::Settings => self.render_settings_page(cx),
        };
        let collector_context_menu = self.render_collector_context_menu(cx);
        div()
            .id("memelith-root")
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .size_full()
            .text_color(rgb(INK))
            .flex()
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .min_w_0()
                    .h_full()
                    .flex_1()
                    .bg(rgba(CONTENT_BG))
                    .flex()
                    .flex_col()
                    .child(self.render_header())
                    .when_some(self.notice.as_ref(), |element, notice| {
                        element.child(div().px_8().pb_3().child(render_notice(notice)))
                    })
                    .child(div().min_h_0().flex_1().child(page)),
            )
            .when_some(collector_context_menu, |root, menu| root.child(menu))
            .into_any_element()
    }
}

fn analyze_selected_images(mut database: MemeDatabase, paths: Vec<PathBuf>) -> ImageAnalysisBatch {
    let results = paths
        .into_iter()
        .map(|path| {
            let result = database
                .find_similar_images(&path, DUPLICATE_IMAGE_MAX_COSINE_DISTANCE)
                .map_err(|error| error.to_string());
            ImageAnalysisResult { path, result }
        })
        .collect();
    ImageAnalysisBatch { database, results }
}

fn collect_selected_images(
    mut database: MemeDatabase,
    paths: Vec<PathBuf>,
) -> CollectorImportBatch {
    let results = paths
        .into_iter()
        .map(|path| {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let result = database
                .collect_image(&path)
                .map_err(|error| error.to_string());
            CollectorImportResult { label, result }
        })
        .collect();
    CollectorImportBatch { database, results }
}

fn collect_text_item(mut database: MemeDatabase, text: String) -> CollectorImportBatch {
    let result = database
        .collect_text(text)
        .map_err(|error| error.to_string());
    CollectorImportBatch {
        database,
        results: vec![CollectorImportResult {
            label: "文字".to_owned(),
            result,
        }],
    }
}

fn detect_draft_characters(
    sensor: Option<WaifuSensor>,
    storage_root: PathBuf,
    waifu_model_path: PathBuf,
    paths: Vec<PathBuf>,
) -> Result<CharacterDetectionBatch, String> {
    let mut sensor = match sensor {
        Some(sensor) => sensor,
        None => open_waifu_sensor(&storage_root, &waifu_model_path)
            .map_err(|error| error.to_string())?,
    };
    let top_one = NonZeroUsize::new(1).expect("one must be non-zero");
    let results = paths
        .into_iter()
        .map(|path| {
            let result = waifu_sensor::image::open(&path)
                .map_err(|error| error.to_string())
                .and_then(|image| {
                    sensor
                        .predict(&image, top_one)
                        .map_err(|error| error.to_string())
                })
                .map(|matches| matches.into_iter().next());
            CharacterDetectionResult { path, result }
        })
        .collect();
    Ok(CharacterDetectionBatch { sensor, results })
}

fn open_waifu_sensor(storage_root: &Path, model_path: &Path) -> waifu_sensor::Result<WaifuSensor> {
    let bundle = WaifuBuiltinAssets::bundle()?;
    let model_manifest = WaifuBuiltinAssets::model_manifest()?;
    WaifuModelManager::verify(&model_manifest, model_path)?;
    let classes = WaifuBuiltinAssets::model_classes()?;
    let tagger = MlDanbooruTagger::load_with_classes(
        model_path,
        &classes,
        bundle.feature_schema.clone(),
        WaifuExecutionPolicy::Auto,
    )?;
    let connection = waifu_sensor::rusqlite::Connection::open(
        storage_root.join(WAIFU_SENSOR_DATABASE_FILENAME),
    )?;
    WaifuSensor::open(connection, &bundle, tagger).map(|(sensor, _)| sensor)
}

fn open_library(storage_root: &Path, model_directory: &Path) -> Result<OpenedLibrary, UiError> {
    let model = ClipModel::load(model_directory, ExecutionPolicy::Auto)?;
    let mut database = MemeDatabase::open(storage_root, model)?;
    let packs = database.list_meme_packs()?;
    let inbox_id = if let Some(inbox) = packs.iter().find(|pack| pack.name == INBOX_NAME) {
        inbox.id
    } else {
        database
            .create_meme_pack(NewMemePack {
                name: INBOX_NAME.to_owned(),
                description: Some("默认收件箱".to_owned()),
                author: None,
                source: None,
            })?
            .id
    };
    let packs = database.list_meme_packs()?;
    let memes = database.list_all_memes()?;
    let tags = database.list_tags()?;
    let collector_items = database.recheck_collector_items()?;
    Ok(OpenedLibrary {
        database,
        inbox_id,
        memes,
        pack_names: packs
            .iter()
            .map(|pack| (pack.id, pack.name.clone()))
            .collect(),
        meme_packs: packs,
        tags,
        collector_items,
    })
}

fn field_row(label: impl Into<SharedString>, input: Entity<TextInput>) -> AnyElement {
    group_row()
        .child(
            div()
                .w(px(64.))
                .flex_none()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(input))
        .into_any_element()
}

fn render_notice(notice: &Notice) -> AnyElement {
    let (dot, message) = match notice {
        Notice::Info(message) => (ACCENT, message),
        Notice::Success(message) => (SUCCESS, message),
        Notice::Warning(message) => (WARNING, message),
        Notice::Error(message) => (DANGER, message),
    };
    div()
        .w_full()
        .px_4()
        .py_3()
        .rounded(px(12.))
        .bg(rgba(GLASS_CARD))
        .border_1()
        .border_color(rgba(EDGE_DARK))
        .shadow(card_shadow())
        .flex()
        .items_center()
        .gap_3()
        .child(div().size(px(8.)).flex_none().rounded_full().bg(rgb(dot)))
        .child(div().text_sm().text_color(rgb(INK)).child(message.clone()))
        .into_any_element()
}

fn format_download_bytes(bytes: u64) -> String {
    const MEBIBYTE: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MEBIBYTE)
}

fn optional_input_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn parse_tags(value: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    value
        .split([',', '，'])
        .filter_map(optional_input_text)
        .filter(|tag| seen.insert(tag.to_ascii_lowercase()))
        .collect()
}

fn merge_tags(value: &str, additions: &[String]) -> (String, Vec<String>) {
    let mut tags = parse_tags(value);
    let mut seen = tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut added = Vec::new();
    for addition in additions {
        if seen.insert(addition.to_ascii_lowercase()) {
            tags.push(addition.clone());
            added.push(addition.clone());
        }
    }
    let merged = if tags.is_empty() {
        String::new()
    } else {
        format!("{}, ", tags.join(", "))
    };
    (merged, added)
}

fn tag_suggestions<'a>(value: &str, tags: &'a [Tag]) -> Vec<&'a Tag> {
    let (fragment_start, query) = current_tag_fragment(value);
    if query.is_empty() {
        return Vec::new();
    }

    let selected = parse_tags(&value[..fragment_start])
        .into_iter()
        .map(|tag| tag.to_lowercase())
        .collect::<HashSet<_>>();
    let query = query.to_lowercase();
    let mut matches = tags
        .iter()
        .filter_map(|tag| {
            let normalized_name = tag.name.to_lowercase();
            if selected.contains(&normalized_name) {
                return None;
            }
            normalized_name.find(&query).map(|position| (position, tag))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_position, left), (right_position, right)| {
        left_position
            .cmp(right_position)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
    matches
        .into_iter()
        .take(MAX_TAG_SUGGESTIONS)
        .map(|(_, tag)| tag)
        .collect()
}

fn complete_tag_input(value: &str, tag_name: &str) -> String {
    let (fragment_start, _) = current_tag_fragment(value);
    let prefix = value[..fragment_start].trim_end();
    if prefix.is_empty() {
        format!("{tag_name}, ")
    } else {
        format!("{prefix} {tag_name}, ")
    }
}

fn current_tag_fragment(value: &str) -> (usize, &str) {
    let fragment_start = value
        .char_indices()
        .rev()
        .find(|(_, character)| matches!(character, ',' | '，'))
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    (fragment_start, value[fragment_start..].trim())
}

fn main() {
    Application::new().run(|cx: &mut App| {
        input::init(cx);
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, None),
            KeyBinding::new("shift-tab", FocusPrevious, None),
            KeyBinding::new("enter", ConfirmTextContent, Some("TextComposer")),
            KeyBinding::new("escape", CancelTextComposer, Some("TextComposer")),
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let saved_storage = settings::load_storage_root();
        let saved_telegram = settings::load_telegram_settings();
        let bounds = Bounds::centered(None, size(px(1120.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(820.), px(560.))),
                app_id: Some("org.memelith.app".to_owned()),
                window_background: WindowBackgroundAppearance::Blurred,
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(APPLICATION_NAME.into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(9.), px(9.))),
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| MemelithView::new(saved_storage, saved_telegram, cx)),
        )
        .expect("failed to open the Memelith window");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{optional_input_text, parse_tags};

    #[test]
    fn parses_trimmed_case_insensitive_unique_tags() {
        assert_eq!(
            parse_tags(" Cat, reaction，cat, 中文， "),
            vec!["Cat", "reaction", "中文"]
        );
    }

    #[test]
    fn preserves_absence_for_blank_optional_fields() {
        assert_eq!(optional_input_text("  "), None);
        assert_eq!(optional_input_text("  title  "), Some("title".to_owned()));
    }
}
