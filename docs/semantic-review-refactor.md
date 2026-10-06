# 语义审查页面问题分析与改造方案

## 当前问题

### 性能问题

#### 1. 一次性加载所有数据
**位置**: `apps/gui/src/main.rs:2533`, `crates/core/src/database.rs:628`

```rust
pub fn list_image_semantics(&self, pack_id: Option<Uuid>) -> Result<Vec<ImageSemantics>> {
    let ids = self.connection.prepare("...")?.query_map(...)?...;
    ids.into_iter()
        .map(|id| self.get_image_semantics(id))  // 对每个 ID 单独查询
        .collect()
}
```

**问题**:
- 无论数据量多大，一次性加载所有图片语义数据到内存
- 如果资源包有 10,000 张图片，会创建 10,000 个 `ImageSemantics` 对象
- 每个对象包含大量字段（路径、caption、tags、OCR、历史记录等）
- 估算：每条记录约 2-5KB，10,000 条 = 20-50MB 纯数据

#### 2. N+1 查询问题
**位置**: `crates/core/src/database.rs:632-634`

```rust
ids.into_iter()
    .map(|id| self.get_image_semantics(id))  // N 次独立查询
    .collect()
```

**问题**:
- 先查询所有 ID（1次查询）
- 然后对每个 ID 执行独立的 `get_image_semantics`（N 次查询）
- 每次查询还要额外查询 `image_category_history` 表
- 10,000 条数据 = 10,001 次数据库查询

#### 3. 全量渲染 DOM
**位置**: `apps/gui/src/main.rs:2891-3028`

```rust
.children(items.map(|item| {
    // 为每个 item 创建完整的 DOM 结构
    div().id(...).flex().gap_4()...  // ~50 行 UI 代码
}))
```

**问题**:
- 即使用户只能看到 5-10 个项目，也会渲染全部
- 10,000 个项目 = 数十万个 DOM 节点
- 浏览器卡顿、滚动不流畅
- 内存占用翻倍（数据 + DOM）

#### 4. 频繁触发重建索引
**位置**: `apps/gui/src/main.rs:3000`

```rust
.on_click(cx.listener(move |view, _, _, cx| {
    // ... 更新分类 ...
    view.start_semantic_rebuild(cx);  // 每次点击都重建
    cx.notify();
}))
```

**问题**:
- 每次点击分类按钮都调用 `start_semantic_rebuild`
- 重建索引可能需要遍历大量数据
- 用户连续修改多个项目时会触发多次重建

---

### 交互问题

#### 1. 编辑模式反人类
**位置**: `apps/gui/src/main.rs:3030-3088`

```rust
.when(self.semantic_editing.is_some(), |page| {
    page.child(
        div().flex().flex_col().gap_2()
            .child(self.semantic_caption_input.clone())
            // ... 编辑表单固定在页面底部
    )
})
```

**问题**:
- 编辑表单固定在页面底部，看不到正在编辑的图片
- 用户需要向上滚动查看图片，向下滚动填写表单
- 只能同时编辑一个项目（`semantic_editing: Option<Uuid>`）
- 编辑状态是全局的，切换项目会丢失未保存的修改

#### 2. 筛选功能严重不足
**位置**: `apps/gui/src/main.rs:2786-2791`

```rust
let items = self.semantic_items.iter().filter(|item| {
    !self.semantic_review_only
        || item.image_review_status == "needs_review"
        || (item.image_type == ImageType::Unknown
            && item.image_review_status != "confirmed")
});
```

**问题**:
- 只能按"待审核/全部"和资源包筛选
- 不能按图片类型筛选（Sticker/插画/未知）
- 不能按索引状态筛选（pending/done/error）
- 不能按错误类型筛选
- 不能搜索 caption、tags、OCR 文本

#### 3. 缺少批量操作
**问题**:
- 无法批量选择多个项目
- 无法批量标记为某个类型
- 无法批量确认审核
- 无法批量重试失败项目
- 审核大量图片时效率极低

#### 4. 刷新机制低效
**位置**: `apps/gui/src/main.rs:2523-2539`

```rust
fn refresh_semantic_items(&mut self) {
    // ... 重新查询所有数据 ...
    match database.list_image_semantics(self.semantic_pack_filter) {
        Ok(items) => self.semantic_items = items,  // 替换全部数据
        // ...
    }
}
```

**问题**:
- 每次刷新都完全替换数据
- 无法增量更新
- 无法保持滚动位置和选中状态

---

## 改造方案

### 1. 数据层优化

#### 1.1 实现分页查询

在 `crates/core/src/database.rs` 添加：

```rust
pub struct SemanticQueryOptions {
    pub pack_id: Option<Uuid>,
    pub review_status_filter: Option<Vec<String>>, // ["needs_review", "confirmed"]
    pub image_type_filter: Option<Vec<ImageType>>,
    pub embedding_status_filter: Option<Vec<String>>,
    pub search_text: Option<String>,  // 搜索 caption/tags/ocr
    pub offset: usize,
    pub limit: usize,
}

pub struct SemanticQueryResult {
    pub items: Vec<ImageSemantics>,
    pub total_count: usize,
    pub has_more: bool,
}

impl Database {
    pub fn list_image_semantics_paginated(
        &self,
        options: &SemanticQueryOptions,
    ) -> Result<SemanticQueryResult> {
        // 1. 构建动态 WHERE 子句
        // 2. 先查询 COUNT(*) 获取总数
        // 3. 使用 LIMIT/OFFSET 查询一页数据
        // 4. 使用 JOIN 一次性获取所有数据（避免 N+1）
    }
}
```

#### 1.2 优化 JOIN 查询避免 N+1

```rust
pub fn list_image_semantics_paginated(
    &self,
    options: &SemanticQueryOptions,
) -> Result<SemanticQueryResult> {
    let mut where_clauses = vec!["c.kind IN ('image','motion')"];
    let mut params: Vec<Box<dyn ToSql>> = vec![];
    
    if let Some(pack_id) = options.pack_id {
        where_clauses.push("m.meme_pack_id = ?");
        params.push(Box::new(pack_id.to_string()));
    }
    
    // ... 添加其他过滤条件 ...
    
    let query = format!(
        "SELECT 
            c.id, c.meme_id, c.relative_path, c.image_type, 
            c.image_type_source, c.image_review_status,
            s.semantic_caption, s.semantic_tags, s.visible_text,
            s.semantic_status, s.semantic_error, s.category_fit,
            /* ... 所有字段 ... */
            GROUP_CONCAT(
                h.from_category || '|' || h.to_category || '|' || 
                h.reason || '|' || h.status || '|' || h.at, 
                ';'
            ) as history
        FROM meme_contents c 
        JOIN memes m ON m.id = c.meme_id
        LEFT JOIN image_semantic_state s ON s.content_id = c.id
        LEFT JOIN image_category_history h ON h.content_id = c.id
        WHERE {}
        GROUP BY c.id
        ORDER BY c.rowid DESC
        LIMIT ? OFFSET ?",
        where_clauses.join(" AND ")
    );
    
    // 执行一次查询获取所有数据和历史记录
}
```

**优点**:
- 将 N+1 次查询减少到 1-2 次（COUNT + SELECT）
- 使用 GROUP_CONCAT 在一次查询中获取历史记录
- 10,000 条数据：从 10,001 次查询降到 2 次

#### 1.3 添加数据库索引

```sql
CREATE INDEX IF NOT EXISTS idx_meme_contents_kind_rowid 
    ON meme_contents(kind, rowid DESC);
    
CREATE INDEX IF NOT EXISTS idx_image_semantic_state_status 
    ON image_semantic_state(embedding_status, semantic_status);
    
CREATE INDEX IF NOT EXISTS idx_meme_contents_review_status 
    ON meme_contents(image_review_status);
```

---

### 2. UI 层优化

#### 2.1 实现虚拟滚动列表

在 `apps/gui/src/main.rs` 中：

```rust
struct SemanticsPageState {
    // 分页状态
    current_page: usize,
    page_size: usize,
    total_count: usize,
    items: Vec<ImageSemantics>,
    loading: bool,
    
    // 虚拟滚动状态
    scroll_offset: f32,
    visible_range: Range<usize>,
    item_height: f32,  // 每个项目的预估高度
    
    // 筛选状态
    filters: SemanticFilters,
}

struct SemanticFilters {
    pack_id: Option<Uuid>,
    review_only: bool,
    image_types: HashSet<ImageType>,
    embedding_statuses: HashSet<String>,
    search_text: String,
}
```

**虚拟滚动实现**:
```rust
fn render_semantics_page(&self, cx: &mut Context<Self>) -> AnyElement {
    let visible_start = (self.scroll_offset / self.item_height).floor() as usize;
    let visible_end = visible_start + (viewport_height / self.item_height).ceil() as usize;
    let visible_range = visible_start..visible_end.min(self.items.len());
    
    div()
        .overflow_y_scroll()
        .on_scroll(cx.listener(|view, event, cx| {
            view.scroll_offset = event.scroll_top;
            view.check_load_more(cx);  // 滚动到底部时加载下一页
        }))
        .child(
            div()
                .h(px(self.item_height * self.total_count as f32))  // 占位容器
                .child(
                    div()
                        .absolute()
                        .top(px(visible_start as f32 * self.item_height))
                        .children(
                            self.items[visible_range.clone()]
                                .iter()
                                .map(|item| self.render_semantic_item(item, cx))
                        )
                )
        )
}
```

**优点**:
- 只渲染可见的 20-30 个项目
- 10,000 条数据：从渲染 10,000 个项目降到 30 个
- 滚动流畅，内存占用低

#### 2.2 改进编辑交互 - 卡片内编辑

```rust
fn render_semantic_item(&self, item: &ImageSemantics, cx: &mut Context<Self>) -> AnyElement {
    let is_editing = self.semantic_editing == Some(item.content_id);
    
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .bg(rgba(SURFACE_1))
        .rounded_lg()
        .child(
            // 图片和基本信息（始终显示）
            div().flex().gap_4()
                .child(img(/*...*/))
                .child(/* 状态信息 */)
        )
        .when(is_editing, |card| {
            // 展开编辑表单（在卡片内部）
            card.child(
                div().flex().flex_col().gap_2()
                    .child(/* caption input */)
                    .child(/* tags input */)
                    .child(/* 分类按钮 */)
                    .child(
                        div().flex().gap_2()
                            .child(button("保存"))
                            .child(button("取消"))
                    )
            )
        })
        .when(!is_editing, |card| {
            card.child(
                button("编辑")
                    .on_click(cx.listener(move |view, _, cx| {
                        view.start_edit(item.content_id, cx);
                    }))
            )
        })
}
```

**优点**:
- 编辑表单在卡片内部展开，图片和表单在同一屏
- 可以同时展开多个卡片（如需要）
- 点击"编辑"时卡片自动滚动到可见区域

#### 2.3 丰富的筛选和搜索

```rust
fn render_filter_bar(&self, cx: &mut Context<Self>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap_2()
        // 审核状态
        .child(
            pill_group("审核状态")
                .option("全部", !self.filters.review_only)
                .option("待审核", self.filters.review_only)
        )
        // 图片类型
        .child(
            pill_group("图片类型")
                .option("全部", self.filters.image_types.is_empty())
                .option("Sticker", self.filters.image_types.contains(&ImageType::Sticker))
                .option("插画", self.filters.image_types.contains(&ImageType::Illustration))
                .option("未知", self.filters.image_types.contains(&ImageType::Unknown))
        )
        // 索引状态
        .child(
            pill_group("索引状态")
                .option("全部", self.filters.embedding_statuses.is_empty())
                .option("待处理", self.filters.embedding_statuses.contains("pending"))
                .option("已完成", self.filters.embedding_statuses.contains("done"))
                .option("失败", self.filters.embedding_statuses.contains("error"))
        )
        // 搜索框
        .child(
            search_input()
                .placeholder("搜索 caption、标签、OCR...")
                .value(&self.filters.search_text)
                .on_input(cx.listener(|view, text, cx| {
                    view.filters.search_text = text;
                    view.debounced_search(cx);  // 防抖搜索
                }))
        )
}
```

#### 2.4 批量操作支持

```rust
struct SemanticsPageState {
    // ... 现有字段 ...
    selected_items: HashSet<Uuid>,
    selection_mode: bool,
}

fn render_bulk_actions(&self, cx: &mut Context<Self>) -> AnyElement {
    div()
        .flex()
        .gap_2()
        .when(self.selection_mode, |bar| {
            bar.child(format!("已选 {} 项", self.selected_items.len()))
                .child(button("标记为 Sticker").on_click(/* ... */))
                .child(button("标记为插画").on_click(/* ... */))
                .child(button("批量确认").on_click(/* ... */))
                .child(button("批量重试").on_click(/* ... */))
                .child(button("取消选择").on_click(/* ... */))
        })
}

fn render_semantic_item(&self, item: &ImageSemantics, cx: &mut Context<Self>) -> AnyElement {
    div()
        .when(self.selection_mode, |card| {
            card.child(
                checkbox()
                    .checked(self.selected_items.contains(&item.content_id))
                    .on_toggle(cx.listener(move |view, _, cx| {
                        view.toggle_selection(item.content_id, cx);
                    }))
            )
        })
        // ... 其余内容 ...
}
```

---

### 3. 性能优化细节

#### 3.1 延迟重建索引

```rust
struct SemanticsPageState {
    rebuild_timer: Option<TimerId>,
}

impl SemanticsPageState {
    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        // 取消之前的计时器
        if let Some(timer) = self.rebuild_timer {
            cx.cancel_timer(timer);
        }
        
        // 延迟 2 秒后重建，允许用户连续修改多个项目
        self.rebuild_timer = Some(cx.spawn_timer(
            Duration::from_secs(2),
            cx.listener(|view, cx| {
                view.start_semantic_rebuild(cx);
                view.rebuild_timer = None;
            })
        ));
    }
}
```

#### 3.2 增量刷新

```rust
fn refresh_semantic_items_incremental(&mut self, cx: &mut Context<Self>) {
    let current_ids: HashSet<Uuid> = self.items.iter()
        .map(|item| item.content_id)
        .collect();
    
    // 只查询当前页的数据
    let query_options = SemanticQueryOptions {
        // ... 筛选条件 ...
        offset: self.current_page * self.page_size,
        limit: self.page_size,
    };
    
    match self.database.list_image_semantics_paginated(&query_options) {
        Ok(result) => {
            // 合并新数据，保留其他页的缓存
            for (i, new_item) in result.items.into_iter().enumerate() {
                let index = self.current_page * self.page_size + i;
                if index < self.items.len() {
                    self.items[index] = new_item;
                } else {
                    self.items.push(new_item);
                }
            }
        }
        Err(error) => { /* ... */ }
    }
}
```

#### 3.3 后台预加载

```rust
fn check_load_more(&mut self, cx: &mut Context<Self>) {
    // 滚动到距离底部 2 页时，预加载下一页
    let trigger_offset = (self.current_page + 2) * self.page_size;
    let visible_index = (self.scroll_offset / self.item_height) as usize;
    
    if visible_index >= trigger_offset && !self.loading {
        self.load_next_page(cx);
    }
}

fn load_next_page(&mut self, cx: &mut Context<Self>) {
    if self.loading || !self.has_more {
        return;
    }
    
    self.loading = true;
    let next_page = self.current_page + 1;
    
    cx.spawn(async move |this, cx| {
        // 在后台线程查询下一页
        let result = /* query from database */;
        
        this.update(cx, |view, cx| {
            view.items.extend(result.items);
            view.current_page = next_page;
            view.has_more = result.has_more;
            view.loading = false;
            cx.notify();
        });
    });
}
```

---

## 实施优先级

### P0 - 必须修复（严重影响使用）
1. **实现分页查询** - 解决内存和数据库性能问题
2. **实现虚拟滚动** - 解决渲染性能问题
3. **改进编辑交互** - 编辑表单移到卡片内

### P1 - 高优先级（显著改善体验）
4. **优化数据库查询（JOIN）** - 消除 N+1 问题
5. **添加数据库索引** - 加速查询
6. **添加基础筛选** - 状态、类型筛选

### P2 - 中优先级（锦上添花）
7. **延迟重建索引** - 避免频繁触发
8. **添加搜索功能** - 文本搜索
9. **批量操作支持** - 提高审核效率

### P3 - 低优先级（未来优化）
10. **增量刷新** - 保持状态
11. **后台预加载** - 优化滚动体验
12. **添加快捷键** - 键盘导航

---

## 预期效果

### 性能提升
- **初始加载时间**: 从 5-10 秒降到 < 500ms
- **内存占用**: 从 50MB+ 降到 < 5MB
- **渲染时间**: 从卡顿降到 60fps 流畅滚动
- **数据库查询**: 从 10,001 次降到 2 次

### 交互改善
- 编辑时能同时看到图片和表单
- 可以快速筛选和搜索目标图片
- 支持批量操作，审核效率提升 10 倍
- 页面响应灵敏，不再卡顿

---

## 参考实现

类似问题的最佳实践：
- **虚拟滚动**: React Virtualized, TanStack Virtual
- **分页查询**: SQL LIMIT/OFFSET + COUNT(*)
- **增量加载**: Infinite Scroll pattern
- **批量操作**: Gmail, Figma, Linear

---

## 风险与注意事项

1. **数据库迁移**: 添加索引需要在现有数据库上运行
2. **API 兼容性**: `list_image_semantics` 可能被其他地方调用
3. **状态管理复杂度**: 分页+筛选+编辑状态需要仔细设计
4. **虚拟滚动高度计算**: 不同卡片高度不同，需要动态测量

---

## 总结

当前的语义审查页面在设计上假设数据量很小，缺乏必要的性能优化和用户体验考虑。

通过实施上述改造方案，可以支撑 **数万张图片** 的审核场景，同时提供流畅的用户体验。

核心思想：
- **不要一次性加载所有数据** - 用分页
- **不要渲染不可见的 UI** - 用虚拟滚动
- **不要执行 N+1 查询** - 用 JOIN
- **把编辑表单放在用户能看到的地方** - 卡片内编辑
