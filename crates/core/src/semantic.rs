//! Stable semantic text construction shared by indexing and VLM persistence.

pub const CAPTION_PROMPT_VERSION: &str = "meme-semantic-v8-category-reclassification";

pub fn category_description(category: &str) -> &'static str {
    match category {
        "sticker" => "用于聊天回复、表达情绪、态度或反应的表情包或贴纸",
        "illustration" => "以人物、场景或艺术画面本身为主要内容的插画",
        _ => "",
    }
}

pub fn anchor_caption(caption: &str, category: &str, fit: &str) -> String {
    let caption = caption.trim();
    if caption.is_empty()
        || category.is_empty()
        || category_description(category).is_empty()
        || fit == "conflict"
        || caption.starts_with(&format!("以当前分类“{category}”"))
    {
        return caption.to_owned();
    }
    let description = category_description(category);
    let hint = if description.is_empty() {
        String::new()
    } else {
        format!("（{description}）")
    };
    format!("以当前分类“{category}”{hint}所代表的情绪、态度或用途为主体：{caption}")
}

/// Converts the reference model's category assessment into persisted review state.
/// Manual choices are handled at the database boundary and must never use this helper.
pub fn automatic_category_review(category_fit: &str) -> (&'static str, &'static str) {
    match category_fit {
        "match" => ("automatic", "confirmed"),
        "uncertain" | "conflict" => ("automatic", "needs_review"),
        _ => ("automatic", "needs_review"),
    }
}

/// Builds the embedding text in the same field order as the reference plugin.
pub fn build_semantic_text(
    caption: &str,
    tags: &[String],
    visible_text: &str,
    image_type: &str,
) -> String {
    let category = match image_type {
        "sticker" | "illustration" => format!("category:{image_type}"),
        _ => String::new(),
    };
    let mut parts = vec![format!("图片含义：{}", caption.trim())];
    if !category.is_empty() {
        parts.push(format!("固定分类标签：{category}"));
    }
    let description = category_description(image_type);
    if !description.is_empty() {
        parts.push(format!("分类含义：{description}"));
    }
    let mut seen = std::collections::HashSet::new();
    parts.push(format!(
        "语义标签：{}",
        tags.iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty()
                && !tag.starts_with("category:")
                && !tag.starts_with("分类:")
                && seen.insert(*tag))
            .collect::<Vec<_>>()
            .join("、")
    ));
    parts.push(format!("图片文字：{}", visible_text.trim()));
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{anchor_caption, automatic_category_review, build_semantic_text};

    #[test]
    fn uncertain_and_invalid_categories_are_reviewed() {
        assert_eq!(
            automatic_category_review("match"),
            ("automatic", "confirmed")
        );
        assert_eq!(
            automatic_category_review("uncertain"),
            ("automatic", "needs_review")
        );
        assert_eq!(
            automatic_category_review("conflict"),
            ("automatic", "needs_review")
        );
        assert_eq!(
            automatic_category_review("unexpected"),
            ("automatic", "needs_review")
        );
    }

    #[test]
    fn preserves_reference_field_order_and_type_context() {
        let tags = vec!["尴尬".to_owned(), "自嘲".to_owned()];
        assert_eq!(
            build_semantic_text("  核心梗义  ", &tags, "  原文  ", "sticker"),
            "图片含义：核心梗义\n固定分类标签：category:sticker\n分类含义：用于聊天回复、表达情绪、态度或反应的表情包或贴纸\n语义标签：尴尬、自嘲\n图片文字：原文"
        );
    }

    #[test]
    fn unknown_type_does_not_invent_a_category() {
        let tags = vec!["标签".to_owned()];
        assert_eq!(
            build_semantic_text("梗", &tags, "", "unknown"),
            "图片含义：梗\n语义标签：标签\n图片文字："
        );
    }

    #[test]
    fn unknown_type_does_not_get_a_synthetic_caption_anchor() {
        assert_eq!(
            anchor_caption("模型描述", "unknown", "uncertain"),
            "模型描述"
        );
    }
}
