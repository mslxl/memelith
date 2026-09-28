use image::{AnimationDecoder, ImageFormat};
use memelith_core::{ImageType, MemeDatabase};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::{fs, path::Path, time::Duration};
use thiserror::Error;

fn prompts() -> &'static Value {
    static PROMPTS: OnceLock<Value> = OnceLock::new();
    PROMPTS.get_or_init(|| {
        serde_json::from_str(include_str!("vlm/prompts.json")).expect("vendored prompts")
    })
}

fn render_prompt(template: &str, fields: &[(&str, String)]) -> String {
    // Substitute only template placeholders; inserted user data is never scanned again.
    static PLACEHOLDER: OnceLock<regex::Regex> = OnceLock::new();
    PLACEHOLDER
        .get_or_init(|| regex::Regex::new(r"\{([a-z_]+)\}").unwrap())
        .replace_all(template, |captures: &regex::Captures<'_>| {
            fields
                .iter()
                .find(|(name, _)| *name == &captures[1])
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| captures[0].to_owned())
        })
        .into_owned()
}

pub fn build_caption_prompt(
    frame_count: usize,
    category: &str,
    description: &str,
    available_categories: &serde_json::Map<String, Value>,
    review: Option<(&str, &Value)>,
) -> String {
    let mut catalog = available_categories.clone();
    catalog.remove(category);
    catalog.remove("");
    let mut prompt = render_prompt(
        prompts()["CATEGORY_CONTEXT_PROMPT"].as_str().unwrap(),
        &[
            ("category", serde_json::to_string(category).unwrap()),
            ("description", serde_json::to_string(description).unwrap()),
            (
                "category_catalog",
                serde_json::to_string_pretty(&catalog).unwrap(),
            ),
        ],
    );
    prompt.push('\n');
    prompt.push_str(prompts()["CAPTION_PROMPT"].as_str().unwrap());
    if let Some((instruction, snapshot)) =
        review.filter(|(instruction, _)| !instruction.trim().is_empty())
    {
        prompt.push('\n');
        prompt.push_str(&render_prompt(
            prompts()["MANUAL_REVIEW_PROMPT"].as_str().unwrap(),
            &[
                (
                    "current_semantic",
                    serde_json::to_string_pretty(snapshot).unwrap(),
                ),
                (
                    "review_instruction",
                    serde_json::to_string(instruction.trim()).unwrap(),
                ),
            ],
        ));
    }
    if frame_count > 1 {
        prompt.push_str(&format!("\n你看到的 {frame_count} 张图片来自同一个 GIF，按从开始到结束的时间顺序等间隔排列。请结合动作变化理解完整含义，不要把它们当成互不相关的图片。\n"));
    }
    prompt
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub reasoning_enabled: bool,
    pub reasoning_effort: Option<String>,
}

impl From<crate::settings::VlmSettings> for VlmConfig {
    fn from(settings: crate::settings::VlmSettings) -> Self {
        Self {
            base_url: settings.base_url,
            api_key: settings.api_key,
            model: settings.model,
            reasoning_enabled: settings.reasoning_enabled,
            reasoning_effort: settings.reasoning_effort,
        }
    }
}

impl VlmConfig {
    pub fn validate(&self) -> Result<(), VlmError> {
        if self.base_url.trim().is_empty()
            || self.api_key.trim().is_empty()
            || self.model.trim().is_empty()
        {
            return Err(VlmError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum VlmError {
    #[error("VLM 配置不完整")]
    InvalidConfig,
    #[error("VLM 请求失败：{0}")]
    Request(#[from] reqwest::Error),
    #[error("VLM HTTP {status}：{body}")]
    Http { status: u16, body: String },
    #[error("VLM 返回缺少可解析内容")]
    EmptyResponse,
    #[error("VLM 返回不是有效 JSON：{0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("VLM 返回字段不合法：{0}")]
    InvalidResult(String),
    #[error("无法读取图片：{0}")]
    Image(#[from] std::io::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CaptionResult {
    pub caption: String,
    pub tags: Vec<String>,
    pub visible_text: String,
    pub category_fit: String,
    pub category_review_reason: String,
    pub suggested_category: String,
}

impl CaptionResult {
    pub fn validate(mut self) -> Result<Self, VlmError> {
        self.caption = self.caption.trim().to_owned();
        let mut seen = HashSet::new();
        self.tags = self
            .tags
            .into_iter()
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty() && seen.insert(tag.clone()))
            .collect();
        if self.caption.trim().is_empty() {
            return Err(VlmError::InvalidResult("caption 不能为空".to_owned()));
        }
        if self.tags.is_empty() {
            return Err(VlmError::InvalidResult(
                "视觉模型结果缺少 caption 或 tags".to_owned(),
            ));
        }
        self.visible_text = self.visible_text.trim().to_owned();
        self.category_fit = self.category_fit.trim().to_lowercase();
        if !matches!(
            self.category_fit.as_str(),
            "match" | "uncertain" | "conflict"
        ) {
            return Err(VlmError::InvalidResult("category_fit 无效".to_owned()));
        }
        self.category_review_reason = self
            .category_review_reason
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(240)
            .collect();
        self.suggested_category = self
            .suggested_category
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        if self.category_fit == "match" {
            self.category_review_reason.clear();
        } else if self.category_review_reason.is_empty() {
            self.category_review_reason = "模型未返回分类判断原因".to_owned();
        }
        if self.category_fit != "conflict" {
            self.suggested_category.clear();
        }
        Ok(self)
    }
}

#[derive(Clone, Copy)]
enum OutputMode {
    Tool,
    Json,
    Text,
}

fn request_body(config: &VlmConfig, prompt: &str, images: &[&str], mode: OutputMode) -> Value {
    let prompt = if matches!(mode, OutputMode::Tool) {
        format!(
            "{prompt}{}",
            prompts()["CAPTION_TOOL_PROMPT_SUFFIX"].as_str().unwrap()
        )
    } else {
        prompt.to_owned()
    };
    let mut content = vec![serde_json::json!({"type": "text", "text": prompt})];
    content.extend(images.iter().map(|url| {
        serde_json::json!({
            "type": "image_url", "image_url": {"url": url}
        })
    }));
    let mut body = serde_json::json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": prompts()["CAPTION_SYSTEM_PROMPT"]},
            {"role": "user", "content": content}
        ],
        "temperature": 0,
        "max_tokens": 900
    });
    if config.reasoning_enabled {
        if let Some(effort) = config.reasoning_effort.as_deref() {
            body["reasoning_effort"] = effort.into();
        }
    }
    match mode {
        OutputMode::Tool => {
            body["tool_choice"] = "required".into();
            body["tools"] = serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "submit_meme_caption",
                    "description": "提交表情包语义、检索标签、图片原文和分类符合判断。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "caption": {"type": "string", "description": "一到两句自然中文，概括核心梗义、复合语气和典型用法。"},
                            "tags": {"type": "array", "items": {"type": "string"}, "description": "6 到 10 个细粒度中文检索标签。"},
                            "visible_text": {"type": "string", "description": "图片中清晰可见的原始文字，没有则为空字符串。"},
                            "category_fit": {"type": "string", "enum": ["match", "uncertain", "conflict"], "description": "图片与当前用户分类的符合判断。"},
                            "category_review_reason": {"type": "string", "description": "需要人工复核时的简短原因；match 时为空字符串。"},
                            "suggested_category": {"type": "string", "description": "仅在明确冲突时选择一个现有分类键，否则为空字符串。"}
                        },
                        "required": ["caption", "tags", "visible_text", "category_fit", "category_review_reason", "suggested_category"],
                        "additionalProperties": false
                    }
                }
            }]);
        }
        OutputMode::Json => body["response_format"] = serde_json::json!({"type": "json_object"}),
        OutputMode::Text => {}
    }
    body
}

fn unsupported(error: &VlmError, tool: bool) -> bool {
    let message = error.to_string().to_lowercase();
    let fields: &[&str] = if tool {
        &[
            "tool_choice",
            "tool choice",
            "tools",
            "function_call",
            "function call",
            "function-calling",
            "function calling",
            "工具调用",
            "函数调用",
        ]
    } else {
        &["response_format", "response format", "结构化输出"]
    };
    let markers: &[&str] = if tool {
        &[
            "unsupported",
            "not support",
            "not yet support",
            "does not support",
            "doesn't support",
            "not enabled",
            "unknown",
            "unrecognized",
            "unexpected",
            "invalid parameter",
            "invalid field",
            "not allowed",
            "only allowed",
            "not permitted",
            "not implemented",
            "extra inputs are not permitted",
            "不支持",
            "不具备",
            "未启用",
            "未知",
            "无法识别",
            "无效参数",
            "不允许",
            "不可用",
            "未实现",
        ]
    } else {
        &[
            "unsupported",
            "not support",
            "not yet support",
            "does not support",
            "unknown",
            "unrecognized",
            "unexpected",
            "invalid",
            "not allowed",
            "not permitted",
            "not implemented",
            "不支持",
            "未知",
            "无效",
            "不允许",
            "不可用",
            "未实现",
        ]
    };
    fields.iter().any(|field| message.contains(field))
        && markers.iter().any(|marker| message.contains(marker))
}

fn request(
    client: &Client,
    config: &VlmConfig,
    prompt: &str,
    images: &[&str],
    mode: OutputMode,
) -> Result<Value, VlmError> {
    let response = client
        .post(format!(
            "{}/chat/completions",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.api_key)
        .json(&request_body(config, prompt, images, mode))
        .send()?;
    let status = response.status();
    let text = response.text()?;
    if !status.is_success() {
        return Err(VlmError::Http {
            status: status.as_u16(),
            body: text,
        });
    }
    Ok(serde_json::from_str(&text)?)
}

fn request_json(
    client: &Client,
    config: &VlmConfig,
    prompt: &str,
    images: &[&str],
) -> Result<Value, VlmError> {
    match request(client, config, prompt, images, OutputMode::Json) {
        Err(error) if unsupported(&error, false) => {
            request(client, config, prompt, images, OutputMode::Text)
        }
        result => result,
    }
}

fn tool_payload(response: &Value) -> Option<&str> {
    response["choices"][0]["message"]["tool_calls"]
        .as_array()?
        .iter()
        .find(|call| call["function"]["name"] == "submit_meme_caption")?["function"]["arguments"]
        .as_str()
}

fn parse_caption(raw: &str) -> Result<CaptionResult, VlmError> {
    let payload = match serde_json::from_str::<Value>(raw.trim()) {
        Ok(value) => value,
        Err(_) => {
            let candidates: Vec<Value> = raw
                .char_indices()
                .filter(|(_, c)| *c == '{')
                .filter_map(|(start, _)| {
                    serde_json::Deserializer::from_str(&raw[start..])
                        .into_iter::<Value>()
                        .next()?
                        .ok()
                })
                .filter(Value::is_object)
                .collect();
            candidates
                .iter()
                .rev()
                .find(|value| {
                    value["caption"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty())
                        && value["tags"]
                            .as_array()
                            .is_some_and(|tags| !tags.is_empty())
                })
                .or(candidates.last())
                .cloned()
                .ok_or(VlmError::EmptyResponse)?
        }
    };
    let string = |key: &str| payload[key].as_str().unwrap_or("").to_owned();
    let tags = match &payload["tags"] {
        Value::Array(tags) => tags
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::String(tag) => vec![tag.clone()],
        _ => vec![],
    };
    let missing_fit = !payload
        .as_object()
        .is_some_and(|object| object.contains_key("category_fit"));
    CaptionResult {
        caption: string("caption"),
        tags,
        visible_text: string("visible_text"),
        category_fit: payload["category_fit"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("uncertain")
            .to_owned(),
        category_review_reason: if missing_fit && string("category_review_reason").trim().is_empty()
        {
            "模型未返回分类符合判断".to_owned()
        } else {
            string("category_review_reason")
        },
        suggested_category: string("suggested_category"),
    }
    .validate()
}

fn parse_response(response: &Value) -> Result<CaptionResult, VlmError> {
    let raw = tool_payload(response)
        .or_else(|| response["choices"][0]["message"]["content"].as_str())
        .ok_or(VlmError::EmptyResponse)?;
    parse_caption(raw)
}

pub fn analyze_image(config: &VlmConfig, prompt: &str, image: &str) -> Result<Value, VlmError> {
    analyze_image_data_urls(config, prompt, &[image])
}

pub fn analyze_image_data_urls(
    config: &VlmConfig,
    prompt: &str,
    images: &[&str],
) -> Result<Value, VlmError> {
    config.validate()?;
    if images.is_empty() {
        return Err(VlmError::InvalidResult("至少需要一张图片".to_owned()));
    }
    // Capability failures belong to a provider/model, not to the image being analyzed.
    static JSON_PROVIDERS: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let cache = JSON_PROVIDERS.get_or_init(Default::default);
    let key = (config.base_url.clone(), config.model.clone());
    let mut tool_mode = !cache.lock().unwrap().contains(&key);
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let response = if tool_mode {
        match request(&client, config, prompt, images, OutputMode::Tool) {
            Err(error) if unsupported(&error, true) => {
                cache.lock().unwrap().insert(key.clone());
                tool_mode = false;
                request_json(&client, config, prompt, images)?
            }
            result => result?,
        }
    } else {
        request_json(&client, config, prompt, images)?
    };
    if tool_mode && tool_payload(&response).is_none() {
        cache.lock().unwrap().insert(key);
    }
    let result = match parse_response(&response) {
        Ok(result) => result,
        Err(_) => {
            let retry = format!(
                "{prompt}{}",
                prompts()["CAPTION_RETRY_PROMPT"].as_str().unwrap()
            );
            parse_response(&request_json(&client, config, &retry, images)?)?
        }
    };
    Ok(serde_json::to_value(result)?)
}

pub fn analyze_image_file(
    config: &VlmConfig,
    prompt: &str,
    path: &Path,
) -> Result<CaptionResult, VlmError> {
    let bytes = fs::read(path)?;
    let format = image::guess_format(&bytes)
        .map_err(|_| VlmError::InvalidResult("VLM 仅支持图片，视频和 TGS 暂不支持".to_owned()))?;
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gif") || ext.eq_ignore_ascii_case("webp"))
    {
        let frames = sample_gif_frames(path)?;
        let refs = frames.iter().map(String::as_str).collect::<Vec<_>>();
        let prompt = if frames.len() > 1 {
            format!(
                "{prompt}\n你看到的 {} 张图片来自同一个 GIF，按从开始到结束的时间顺序等间隔排列。请结合动作变化理解完整含义，不要把它们当成互不相关的图片。\n",
                frames.len()
            )
        } else {
            prompt.to_owned()
        };
        let result: CaptionResult =
            serde_json::from_value(analyze_image_data_urls(config, &prompt, &refs)?)?;
        return result.validate();
    }
    let mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        _ => return Err(VlmError::InvalidResult("VLM 不支持此图片格式".to_owned())),
    };
    let image_data_url = format!("data:{mime};base64,{}", base64_encode(&bytes));
    let result: CaptionResult =
        serde_json::from_value(analyze_image(config, prompt, &image_data_url)?)?;
    result.validate()
}

pub fn sample_gif_frames(path: &Path) -> Result<Vec<String>, VlmError> {
    let file = fs::File::open(path)?;
    let frames = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("webp"))
    {
        let decoder = image::codecs::webp::WebPDecoder::new(std::io::BufReader::new(file))
            .map_err(|error| VlmError::InvalidResult(format!("WebP 解码失败：{error}")))?;
        if !decoder.has_animation() {
            return Ok(vec![format!(
                "data:image/webp;base64,{}",
                base64_encode(&fs::read(path)?)
            )]);
        }
        decoder.into_frames().collect_frames()
    } else {
        image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .map_err(|error| VlmError::InvalidResult(format!("GIF 解码失败：{error}")))?
            .into_frames()
            .collect_frames()
    }
    .map_err(|error| VlmError::InvalidResult(format!("动图帧读取失败：{error}")))?;
    if frames.is_empty() {
        return Err(VlmError::InvalidResult("GIF 没有可读取的帧".to_owned()));
    }
    sample_frame_indexes(frames.len())
        .into_iter()
        .map(|index| {
            let mut bytes = Cursor::new(Vec::new());
            frames[index]
                .buffer()
                .write_to(&mut bytes, ImageFormat::Png)
                .map_err(|error| VlmError::InvalidResult(format!("GIF 帧编码失败：{error}")))?;
            Ok(format!(
                "data:image/png;base64,{}",
                base64_encode(&bytes.into_inner())
            ))
        })
        .collect()
}

fn sample_frame_indexes(frame_count: usize) -> Vec<usize> {
    let samples = frame_count.min(5);
    if samples <= 1 {
        return (0..samples).collect();
    }
    (0..samples)
        .map(|position| {
            (position as f64 * (frame_count - 1) as f64 / (samples - 1) as f64).round_ties_even()
                as usize
        })
        .collect()
}

pub fn propose_revision(
    config: &VlmConfig,
    path: &Path,
    current: &memelith_core::ImageSemantics,
    instruction: &str,
) -> Result<CaptionResult, VlmError> {
    let category = match current.image_type {
        ImageType::Unknown => "unknown",
        ImageType::Sticker => "sticker",
        ImageType::Illustration => "illustration",
    };
    let catalog = serde_json::json!({
        "sticker": memelith_core::semantic::category_description("sticker"),
        "illustration": memelith_core::semantic::category_description("illustration"),
    });
    let snapshot = serde_json::json!({
        "caption":current.caption.as_deref().unwrap_or(""),
        "tags":current.semantic_tags,
        "visible_text":current.visible_text.as_deref().unwrap_or(""),
        "current_category":category,
        "original_category":current.reclassification_history.iter().rev()
            .map(|record| record.from_category.as_str())
            .find(|original| *original != category && catalog.as_object().unwrap().contains_key(*original)).unwrap_or(""),
        "reclassification_status":current.reclassification_history.last().map(|record| record.status.as_str()).unwrap_or(""),
        "reclassification_reason":current.reclassification_history.last().map(|record| record.reason.as_str()).unwrap_or(""),
    });
    let prompt = build_caption_prompt(
        1,
        category,
        memelith_core::semantic::category_description(category),
        catalog.as_object().unwrap(),
        Some((instruction, &snapshot)),
    );
    let mut result = analyze_image_file(config, &prompt, path)?;
    if result.suggested_category == category
        || !catalog
            .as_object()
            .unwrap()
            .contains_key(&result.suggested_category)
    {
        result.suggested_category.clear();
    }
    result.caption =
        memelith_core::semantic::anchor_caption(&result.caption, category, &result.category_fit);
    Ok(result)
}

pub fn analyze_and_persist(
    database: Arc<Mutex<MemeDatabase>>,
    config: VlmConfig,
    content_id: uuid::Uuid,
    image_path: &Path,
) -> Result<(), String> {
    let semantics = {
        let database = database.lock().map_err(|_| "数据库锁已损坏".to_owned())?;
        let semantics = database
            .get_image_semantics(content_id)
            .map_err(|error| error.to_string())?;
        if semantics.provenance == "manual" {
            return Ok(());
        }
        database
            .begin_image_semantics(content_id)
            .map_err(|error| error.to_string())?;
        semantics
    };
    let category = match semantics.image_type {
        ImageType::Unknown => "unknown",
        ImageType::Sticker => "sticker",
        ImageType::Illustration => "illustration",
    };
    let catalog = serde_json::json!({
        "sticker": memelith_core::semantic::category_description("sticker"),
        "illustration": memelith_core::semantic::category_description("illustration"),
    });
    let description = catalog[category]
        .as_str()
        .unwrap_or("尚未确定图片类型，等待判断或人工确认");
    let prompt = build_caption_prompt(1, category, description, catalog.as_object().unwrap(), None);
    let result =
        analyze_image_file(&config, &prompt, image_path).map_err(|error| error.to_string());
    let mut database = database.lock().map_err(|_| "数据库锁已损坏".to_owned())?;
    match result {
        Ok(result) => {
            let caption = memelith_core::semantic::anchor_caption(
                &result.caption,
                category,
                &result.category_fit,
            );
            let image_type = match result.suggested_category.as_str() {
                "sticker" => ImageType::Sticker,
                "illustration" => ImageType::Illustration,
                _ if result.category_fit != "conflict" => semantics.image_type,
                _ => ImageType::Unknown,
            };
            let (source, review) = if matches!(image_type, ImageType::Unknown) {
                ("automatic", "needs_review")
            } else {
                memelith_core::semantic::automatic_category_review(&result.category_fit)
            };
            if let Err(error) = database.save_vlm_semantics(
                content_id,
                image_type,
                source,
                review,
                &caption,
                &result.tags,
                &result.visible_text,
                Some((
                    &result.category_fit,
                    &result.category_review_reason,
                    &result.suggested_category,
                )),
            ) {
                let _ = database.fail_image_semantics(content_id, &error.to_string());
                return Err(error.to_string());
            }
            database
                .rebuild_image_semantics(content_id)
                .map_err(|error| error.to_string())
        }
        Err(error) => {
            let _ = database.fail_image_semantics(content_id, &error);
            Err(error)
        }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CaptionResult, VlmConfig, VlmError, analyze_image, base64_encode};

    #[test]
    fn non_image_media_is_rejected_before_any_request() {
        let directory = tempfile::tempdir().unwrap();
        let config = VlmConfig {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            api_key: "unused".to_owned(),
            model: "unused".to_owned(),
            reasoning_enabled: false,
            reasoning_effort: None,
        };
        for extension in ["mp4", "webm", "tgs", "png"] {
            let path = directory.path().join(format!("unsupported.{extension}"));
            std::fs::write(&path, b"not an image").unwrap();
            assert!(matches!(
                super::analyze_image_file(&config, "original prompt", &path),
                Err(VlmError::InvalidResult(_))
            ));
        }
    }

    #[test]
    fn gif_frames_preserve_pixels_and_upstream_sampling_order() {
        use image::{Frame, Rgba, RgbaImage};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("animation.gif");
        let frames = (0..7)
            .map(|index| Frame::new(RgbaImage::from_pixel(2, 2, Rgba([index * 30, 0, 0, 255]))))
            .collect::<Vec<_>>();
        {
            let mut encoder =
                image::codecs::gif::GifEncoder::new(std::fs::File::create(&path).unwrap());
            encoder.encode_frames(frames.clone()).unwrap();
        }
        let sampled = super::sample_gif_frames(&path).unwrap();
        assert_eq!(sampled.len(), 5);
        for (url, index) in sampled.iter().zip([0, 2, 3, 4, 6]) {
            let mut bytes = std::io::Cursor::new(Vec::new());
            frames[index]
                .buffer()
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            assert_eq!(
                url,
                &format!(
                    "data:image/png;base64,{}",
                    base64_encode(&bytes.into_inner())
                )
            );
        }
    }

    fn mock_server(
        replies: Vec<(u16, serde_json::Value)>,
    ) -> (VlmConfig, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = VlmConfig {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            api_key: "mock-key".to_owned(),
            model: "mock-vision".to_owned(),
            reasoning_enabled: true,
            reasoning_effort: Some("high".to_owned()),
        };
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, reply) in replies {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(5))
                        }
                        Err(error) => panic!("mock request missing: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&mut socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, "POST /v1/chat/completions HTTP/1.1\r\n");
                let mut length = 0;
                let mut authorized = false;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    authorized |= lower.trim() == "authorization: bearer mock-key";
                }
                assert!(authorized);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                requests.push(serde_json::from_slice(&body).unwrap());
                let reply = reply.to_string();
                write!(socket, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
            requests
        });
        (config, server)
    }

    fn caption_reply(tool: bool) -> serde_json::Value {
        let caption = serde_json::json!({"caption":"惊讶", "tags":["惊讶"], "visible_text":"原文", "category_fit":"match"}).to_string();
        let message = if tool {
            serde_json::json!({"tool_calls":[{"function":{"name":"submit_meme_caption", "arguments":caption}}]})
        } else {
            serde_json::json!({"content":caption})
        };
        serde_json::json!({"choices":[{"message":message}]})
    }

    #[test]
    fn tool_request_preserves_reference_options_and_images() {
        let (config, server) = mock_server(vec![(200, caption_reply(true))]);
        let images = ["data:image/png;base64,AA==", "data:image/png;base64,AQ=="];
        let value = super::analyze_image_data_urls(&config, "original prompt", &images).unwrap();
        assert_eq!(value["visible_text"], "原文");
        let requests = server.join().unwrap();
        let body = &requests[0];
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["max_tokens"], 900);
        assert_eq!(body["tool_choice"], "required");
        assert!(body.get("response_format").is_none());
        assert_eq!(
            body["messages"][0]["content"],
            super::prompts()["CAPTION_SYSTEM_PROMPT"]
        );
        assert_eq!(
            body["messages"][1]["content"][2]["image_url"]["url"],
            images[1]
        );
    }

    #[test]
    fn unsupported_tool_and_json_modes_fall_back_and_cache_per_provider() {
        let (config, server) = mock_server(vec![
            (400, serde_json::json!({"error":"tools unsupported"})),
            (
                400,
                serde_json::json!({"error":"response_format unsupported"}),
            ),
            (200, caption_reply(false)),
            (200, caption_reply(false)),
        ]);
        analyze_image(&config, "prompt", "data:image/png;base64,AA==").unwrap();
        analyze_image(&config, "prompt", "data:image/png;base64,AA==").unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].get("tools").is_some());
        assert!(requests[1].get("tools").is_none());
        assert_eq!(requests[1]["response_format"]["type"], "json_object");
        assert!(requests[2].get("response_format").is_none());
        assert!(requests[3].get("tools").is_none());
    }

    #[test]
    fn invalid_payload_retries_once_in_json_mode_without_old_response() {
        let (config, server) = mock_server(vec![
            (
                200,
                serde_json::json!({"choices":[{"message":{"content":"invalid output"}}]}),
            ),
            (200, caption_reply(false)),
        ]);
        analyze_image(&config, "prompt", "data:image/png;base64,AA==").unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            requests[1]["messages"][1]["content"][0]["text"],
            format!(
                "prompt{}",
                super::prompts()["CAPTION_RETRY_PROMPT"].as_str().unwrap()
            )
        );
        assert!(requests[1].get("tools").is_none());
    }

    #[test]
    fn authentication_errors_do_not_trigger_capability_fallback_or_retry() {
        let (config, server) =
            mock_server(vec![(401, serde_json::json!({"error":"invalid API key"}))]);
        assert!(matches!(
            analyze_image(&config, "prompt", "data:image/png;base64,AA=="),
            Err(VlmError::Http { status: 401, .. })
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn gif_sampling_matches_python_round_and_includes_endpoints() {
        assert!(super::sample_frame_indexes(0).is_empty());
        assert_eq!(super::sample_frame_indexes(1), vec![0]);
        assert_eq!(super::sample_frame_indexes(7), vec![0, 2, 3, 4, 6]);
        assert_eq!(super::sample_frame_indexes(10), vec![0, 2, 4, 7, 9]);
    }

    #[test]
    fn prompt_substitution_does_not_expand_placeholders_in_user_data() {
        let snapshot = serde_json::json!({"caption":"literal {review_instruction}","tags":["tag"],"visible_text":"OCR"});
        let prompt = super::build_caption_prompt(
            1,
            "sticker",
            "literal {category_catalog}",
            &serde_json::Map::new(),
            Some(("correct the meaning", &snapshot)),
        );
        assert!(prompt.contains("literal {review_instruction}"));
        assert!(prompt.contains("literal {category_catalog}"));
        assert!(prompt.contains("人工复审意见：\n\"correct the meaning\""));
    }

    #[test]
    fn revision_request_uses_reference_snapshot_and_rejects_invented_category() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let current = memelith_core::ImageSemantics {
            content_id: uuid::Uuid::from_u128(1),
            meme_id: uuid::Uuid::from_u128(2),
            relative_path: "review.png".into(),
            image_type: memelith_core::ImageType::Sticker,
            image_type_source: "manual".to_owned(),
            image_review_status: "confirmed".to_owned(),
            caption: Some("old caption".to_owned()),
            semantic_tags: vec!["old tag".to_owned()],
            visible_text: Some("old OCR".to_owned()),
            status: "done".to_owned(),
            error: None,
            prompt_version: None,
            embedding_provider: None,
            embedding_model: None,
            embedding_dimension: None,
            text_hash: None,
            category_fit: Some("uncertain".to_owned()),
            category_review_reason: Some("old reason".to_owned()),
            suggested_category: None,
            provenance: "manual".to_owned(),
            embedding_status: "pending".to_owned(),
            embedding_error: None,
            index_version: 1,
            built_at: None,
            reclassification_history: vec![memelith_core::CategoryReclassification {
                from_category: "illustration".to_owned(),
                to_category: "sticker".to_owned(),
                reason: "previous classification reason".to_owned(),
                status: "auto_reclassified".to_owned(),
                at: "2026-09-06T00:00:00Z".to_owned(),
            }],
        };
        let response = serde_json::json!({"caption":"new caption","tags":["new tag"],"visible_text":"new OCR","category_fit":"conflict","category_review_reason":"different meaning","suggested_category":"invented"});
        let (config, server) = mock_server(vec![(
            200,
            serde_json::json!({"choices":[{"message":{"tool_calls":[{"function":{"name":"submit_meme_caption","arguments":response.to_string()}}]}}]}),
        )]);
        let proposal =
            super::propose_revision(&config, &path, &current, "这是自嘲，不是质问").unwrap();
        assert_eq!(proposal.caption, "new caption");
        assert!(proposal.suggested_category.is_empty());
        assert_eq!(current.caption.as_deref(), Some("old caption"));
        let requests = server.join().unwrap();
        let prompt = requests[0]["messages"][1]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(prompt.contains("【人工复审纠错】"));
        assert!(prompt.contains("\"current_category\": \"sticker\""));
        assert!(prompt.contains("\"original_category\": \"illustration\""));
        assert!(prompt.contains("\"reclassification_status\": \"auto_reclassified\""));
        assert!(prompt.contains("previous classification reason"));
        assert!(prompt.contains("old OCR"));
        assert!(prompt.contains("这是自嘲，不是质问"));
        assert!(prompt.contains(super::prompts()["CAPTION_PROMPT"].as_str().unwrap()));
    }

    #[test]
    fn vlm_job_persists_searchable_ocr_and_preserves_manual_classification() {
        for manual in [false, true] {
            verify_persisted_vlm_job(manual);
        }
    }

    fn verify_persisted_vlm_job(manual: bool) {
        struct TestProvider;
        impl memelith_core::EmbeddingProvider for TestProvider {
            fn model_id(&self) -> &str {
                "vlm-flow-test"
            }
            fn dimension(&self) -> usize {
                4
            }
            fn embed_text(
                &mut self,
                _: &str,
            ) -> Result<Vec<f32>, memelith_core::EmbeddingProviderError> {
                Ok(vec![1., 2., 3., 4.])
            }
            fn embed_image(
                &mut self,
                _: &image::DynamicImage,
            ) -> Result<Vec<f32>, memelith_core::EmbeddingProviderError> {
                Ok(vec![1., 2., 3., 4.])
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([7, 8, 9, 255]))
            .save(&source)
            .unwrap();
        let mut database =
            memelith_core::MemeDatabase::open(directory.path().join("library"), TestProvider)
                .unwrap();
        let pack = database
            .create_meme_pack(memelith_core::NewMemePack {
                name: "test".to_owned(),
                description: None,
                author: None,
                source: None,
            })
            .unwrap();
        let meme = database
            .create_meme(
                pack.id,
                memelith_core::NewMeme {
                    name: None,
                    description: None,
                    contents: vec![memelith_core::NewMemeContent::Image {
                        source_path: source,
                    }],
                },
            )
            .unwrap();
        let id = meme.contents[0].id();
        if manual {
            database
                .set_manual_image_types(&[(id, memelith_core::ImageType::Illustration)])
                .unwrap();
        }
        let path = database
            .resolve_media_path(database.get_image_semantics(id).unwrap().relative_path)
            .unwrap();
        let before = database.search_memes_semantic("query", 0).unwrap();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(database));
        let payload = serde_json::json!({
            "caption":"惊讶", "tags":["惊讶"], "visible_text":"原文",
            "category_fit":"conflict", "category_review_reason":"用于回复",
            "suggested_category":"sticker"
        });
        let (config, server) = mock_server(vec![(
            200,
            serde_json::json!({
                "choices":[{"message":{"tool_calls":[{"function":{
                    "name":"submit_meme_caption", "arguments":payload.to_string()
                }}]}}]
            }),
        )]);
        super::analyze_and_persist(shared.clone(), config, id, &path).unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let mut database = shared.lock().unwrap();
        let item = database.get_image_semantics(id).unwrap();
        assert_eq!(item.status, "done");
        assert_eq!(item.embedding_status, "done");
        if manual {
            assert_eq!(item.image_type, memelith_core::ImageType::Illustration);
            assert_eq!(item.image_type_source, "manual");
            assert_eq!(item.image_review_status, "confirmed");
            assert_eq!(item.category_fit.as_deref(), Some("conflict"));
            assert!(item.reclassification_history.is_empty());
        } else {
            assert_eq!(item.image_type, memelith_core::ImageType::Sticker);
            assert_eq!(item.image_type_source, "automatic");
            assert_eq!(item.image_review_status, "needs_review");
            assert_eq!(item.category_fit.as_deref(), Some("uncertain"));
            assert_eq!(item.reclassification_history.len(), 1);
            assert_eq!(item.reclassification_history[0].from_category, "unknown");
            assert_eq!(item.reclassification_history[0].to_category, "sticker");
        }
        assert_eq!(item.visible_text.as_deref(), Some("原文"));
        assert_eq!(
            item.prompt_version.as_deref(),
            Some(memelith_core::semantic::CAPTION_PROMPT_VERSION)
        );
        assert_eq!(
            database.search_vlm_semantics("query", 0).unwrap()[0].meme_id,
            meme.id
        );
        assert_eq!(database.search_memes_semantic("query", 0).unwrap(), before);
        let expression = crate::search::SearchExpression::parse("原文")
            .unwrap()
            .unwrap();
        assert!(expression.matches(&database.get_meme(meme.id).unwrap(), Some("test"), &[]));
    }

    #[test]
    fn vendored_prompts_are_byte_identical_to_researched_revision() {
        use sha2::{Digest, Sha256};
        for (name, hash) in [
            (
                "CAPTION_PROMPT",
                "f28e438251d250b280469dcffe54c7e2b9996629c390623a34aa53d53e703335",
            ),
            (
                "CATEGORY_CONTEXT_PROMPT",
                "5fa20cf1a0c57332b9d056f2bdb97264b2e86dfce23bacb6c243a82bf55824ed",
            ),
            (
                "MANUAL_REVIEW_PROMPT",
                "0cb6360b8121ccf805bbe9236ac6f89ede943d89a4d0a19e4cdbfca759a31713",
            ),
            (
                "CAPTION_TOOL_PROMPT_SUFFIX",
                "1eb7553e0e71e39a7ba87c5596ad9d5fda51c7725121d47bfb49345d1d6ff7d4",
            ),
            (
                "CAPTION_RETRY_PROMPT",
                "c166daa21ae821a6844cc27e992d67c3b9b42a7a8db4f1e3d8d0770902d1847b",
            ),
            (
                "CAPTION_SYSTEM_PROMPT",
                "e0ab55a6dfbc1171485357f964709f84174421b9419ed739b6a8658e9b8ca1a2",
            ),
        ] {
            assert_eq!(
                hex::encode(Sha256::digest(
                    super::prompts()[name].as_str().unwrap().as_bytes()
                )),
                hash,
                "{name}"
            );
        }
    }

    #[test]
    fn parser_accepts_reference_legacy_and_fenced_payloads() {
        let result = super::parse_caption("analysis {\"unused\":true}\n```json\n{\"caption\":\"梗\",\"tags\":[\" 标签 \",\"标签\"]}\n```").unwrap();
        assert_eq!(result.tags, vec!["标签"]);
        assert_eq!(result.category_fit, "uncertain");
        assert_eq!(result.category_review_reason, "模型未返回分类符合判断");
        assert!(super::parse_caption("{\"caption\":\"梗\",\"tags\":[]}").is_err());
    }

    #[test]
    fn rejects_incomplete_configuration_before_network_access() {
        let config = VlmConfig {
            base_url: "".to_owned(),
            api_key: "key".to_owned(),
            model: "vision".to_owned(),
            reasoning_enabled: true,
            reasoning_effort: Some("high".to_owned()),
        };
        assert!(matches!(
            analyze_image(&config, "prompt", "data:image/png;base64,AA=="),
            Err(VlmError::InvalidConfig)
        ));
    }

    #[test]
    fn validates_reference_result_shape() {
        let result = CaptionResult {
            caption: "表达惊讶".to_owned(),
            tags: (0..6).map(|i| format!("标签{i}")).collect(),
            visible_text: String::new(),
            category_fit: "match".to_owned(),
            category_review_reason: String::new(),
            suggested_category: String::new(),
        };
        assert!(result.clone().validate().is_ok());
        let mut invalid = result;
        invalid.category_fit = "bad".to_owned();
        assert!(matches!(
            invalid.validate(),
            Err(VlmError::InvalidResult(_))
        ));
    }

    #[test]
    fn base64_encoding_matches_data_url_requirements() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn review_categories_fill_in_missing_reason_like_upstream() {
        let result = CaptionResult {
            caption: "表达惊讶".to_owned(),
            tags: (0..6).map(|i| format!("标签{i}")).collect(),
            visible_text: String::new(),
            category_fit: "uncertain".to_owned(),
            category_review_reason: String::new(),
            suggested_category: String::new(),
        };
        assert_eq!(
            result.validate().unwrap().category_review_reason,
            "模型未返回分类判断原因"
        );
    }

    #[test]
    fn multi_image_request_rejects_empty_frame_list() {
        let config = VlmConfig {
            base_url: "http://localhost".to_owned(),
            api_key: "key".to_owned(),
            model: "vision".to_owned(),
            reasoning_enabled: false,
            reasoning_effort: None,
        };
        assert!(matches!(
            super::analyze_image_data_urls(&config, "prompt", &[]),
            Err(VlmError::InvalidResult(_))
        ));
    }
}
