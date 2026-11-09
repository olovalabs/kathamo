use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Element, ElementId, Entity,
    EntityId, FocusHandle, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement,
    KeyBinding, LayoutId, ListState, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point, RenderOnce, SharedString, Size, StyleRefinement, Styled, Window, div, px,
};
use smol::stream::StreamExt;
use smol::Timer;

use crate::highlighter::HighlightTheme;
use crate::scroll::ScrollableElement;
use crate::text::node::CodeBlock;
use crate::{ActiveTheme, StyledExt, v_flex};
use crate::{
    global_state::GlobalState,
    input::{self},
    text::{
        TextViewStyle,
        node::{self, NodeContext},
    },
};

const CONTEXT: &'static str = "TextView";

// Opt-in parse timings for the streaming chat pipeline. The app enables these
// with `KATHAMO_STREAM_TIMINGS=1`; with the variable unset every hook is one
// atomic load and no samples are retained. Only durations are recorded.
static TIMING_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0")
});
static PARSE_SAMPLES: LazyLock<Mutex<Vec<(&'static str, u64)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static PARSE_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static PARSE_NEXT_REPORT_MS: AtomicU64 = AtomicU64::new(0);

fn record_parse(label: &'static str, start: Instant) {
    if !*TIMING_ENABLED {
        return;
    }
    let micros = start.elapsed().as_micros().min(u128::from(u32::MAX)) as u64;
    {
        let mut samples = PARSE_SAMPLES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        samples.push((label, micros));
    }
    maybe_report_parses();
}

fn maybe_report_parses() {
    let elapsed = PARSE_ORIGIN.elapsed().as_millis() as u64;
    let next = PARSE_NEXT_REPORT_MS.load(Ordering::Relaxed);
    if next == 0 {
        PARSE_NEXT_REPORT_MS.store(elapsed + 5_000, Ordering::Relaxed);
        return;
    }
    if elapsed < next {
        return;
    }
    PARSE_NEXT_REPORT_MS.store(elapsed + 5_000, Ordering::Relaxed);
    let samples = {
        let mut samples = PARSE_SAMPLES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *samples)
    };
    if samples.is_empty() {
        return;
    }
    eprintln!("kathamo markdown parse timings (us, per report window):");
    for label in ["markdown.initial_parse", "markdown.update_parse"] {
        let mut values: Vec<u64> = samples
            .iter()
            .filter(|(sample, _)| *sample == label)
            .map(|(_, value)| *value)
            .collect();
        if values.is_empty() {
            continue;
        }
        values.sort_unstable();
        let percentile = |fraction: f64| {
            let index = ((values.len() as f64 * fraction) as usize).min(values.len() - 1);
            values[index]
        };
        eprintln!(
            "  {label:<24} n={:<6} p50={:<8} p95={:<8} max={}",
            values.len(),
            percentile(0.50),
            percentile(0.95),
            values.last().copied().unwrap_or(0),
        );
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", input::Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", input::Copy, Some(CONTEXT)),
    ]);
}

#[derive(IntoElement, Clone)]
struct TextViewElement {
    list_state: Option<ListState>,
    state: Entity<TextViewState>,
}

impl RenderOnce for TextViewElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, cx| {
            v_flex()
                .size_full()
                .map(|this| match &mut state.parsed_result {
                    Some(Ok(content)) => this.child(content.root_node.render_root(
                        self.list_state.clone(),
                        &content.node_cx,
                        window,
                        cx,
                    )),
                    Some(Err(err)) => this.child(
                        v_flex()
                            .gap_1()
                            .child("Failed to parse content")
                            .child(err.to_string()),
                    ),
                    None => this,
                })
        })
    }
}

/// Type for code block actions generator function.
pub(crate) type CodeBlockActionsFn =
    dyn Fn(&CodeBlock, &mut Window, &mut App) -> AnyElement + Send + Sync;

/// A text view that can render Markdown or HTML.
///
/// ## Goals
///
/// - Provide a rich text rendering component for such as Markdown or HTML,
/// used to display rich text in GPUI application (e.g., Help messages, Release notes)
/// - Support Markdown GFM and HTML (Simple HTML like Safari Reader Mode) for showing most common used markups.
/// - Support Heading, Paragraph, Bold, Italic, StrikeThrough, Code, Link, Image, Blockquote, List, Table, HorizontalRule, CodeBlock ...
///
/// ## Not Goals
///
/// - Customization of the complex style (some simple styles will be supported)
/// - As a Markdown editor or viewer (If you want to like this, you must fork your version).
/// - As a HTML viewer, we not support CSS, we only support basic HTML tags for used to as a content reader.
///
/// See also [`MarkdownElement`], [`HtmlElement`]
#[derive(Clone)]
pub struct TextView {
    id: ElementId,
    init_state: Option<InitState>,
    raw: SharedString,
    state: Entity<TextViewState>,
    style: StyleRefinement,
    selectable: bool,
    scrollable: bool,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
}

#[derive(PartialEq)]
pub(crate) struct ParsedContent {
    pub(crate) root_node: node::Node,
    pub(crate) node_cx: node::NodeContext,
}

/// The type of the text view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextViewType {
    /// Markdown view
    Markdown,
    /// HTML view
    Html,
}

enum Update {
    Text(SharedString),
    Style(Box<TextViewStyle>),
}

struct UpdateFuture {
    type_: TextViewType,
    highlight_theme: Arc<HighlightTheme>,
    current_style: TextViewStyle,
    current_text: SharedString,
    timer: Timer,
    update_scheduled: bool,
    rx: Pin<Box<smol::channel::Receiver<Update>>>,
    tx_result: smol::channel::Sender<Result<ParsedContent, SharedString>>,
    delay: Duration,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
}

impl UpdateFuture {
    #[allow(clippy::too_many_arguments)]
    fn new(
        type_: TextViewType,
        style: TextViewStyle,
        text: SharedString,
        highlight_theme: Arc<HighlightTheme>,
        rx: smol::channel::Receiver<Update>,
        tx_result: smol::channel::Sender<Result<ParsedContent, SharedString>>,
        delay: Duration,
        code_block_actions: Option<Arc<CodeBlockActionsFn>>,
    ) -> Self {
        Self {
            type_,
            highlight_theme,
            current_style: style,
            current_text: text,
            timer: Timer::never(),
            update_scheduled: false,
            rx: Box::pin(rx),
            tx_result,
            delay,
            code_block_actions,
        }
    }
}

impl Future for UpdateFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        loop {
            let received = match self.rx.poll_next(cx) {
                Poll::Ready(Some(update)) => {
                    let changed = match update {
                        Update::Text(text) if self.current_text != text => {
                            self.current_text = text;
                            true
                        }
                        Update::Style(style) if self.current_style != *style => {
                            self.current_style = *style;
                            true
                        }
                        _ => false,
                    };
                    if changed && !self.update_scheduled {
                        // Throttle updates rather than debouncing them: a steady
                        // stream must still produce intermediate rendered text.
                        let delay = self.delay;
                        self.timer.set_after(delay);
                        self.update_scheduled = true;
                    }
                    true
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => false,
            };

            match self.timer.poll_next(cx) {
                Poll::Ready(Some(_)) => {
                    self.update_scheduled = false;
                    let parse_start = Instant::now();
                    let res = parse_content(
                        self.type_,
                        &self.current_text,
                        self.current_style.clone(),
                        &self.highlight_theme,
                        &self.code_block_actions.clone(),
                    );
                    record_parse("markdown.update_parse", parse_start);
                    _ = self.tx_result.try_send(res);
                    continue;
                }
                Poll::Ready(None) | Poll::Pending if !received => return Poll::Pending,
                Poll::Ready(None) | Poll::Pending => {}
            }
        }
    }
}

#[derive(Clone)]
enum InitState {
    Initializing {
        type_: TextViewType,
        text: SharedString,
        style: Box<TextViewStyle>,
        highlight_theme: Arc<HighlightTheme>,
    },
    Initialized {
        tx: smol::channel::Sender<Update>,
    },
}

pub(crate) struct TextViewState {
    parent_entity: Option<EntityId>,
    tx: Option<smol::channel::Sender<Update>>,
    parsed_result: Option<Result<ParsedContent, SharedString>>,
    focus_handle: Option<FocusHandle>,

    bounds: Bounds<Pixels>,

    selection_positions: (Option<Point<Pixels>>, Option<Point<Pixels>>),

    is_selecting: bool,
    is_selectable: bool,
    list_state: ListState,
}

impl TextViewState {
    fn new(cx: &mut Context<TextViewState>) -> Self {
        let focus_handle = cx.focus_handle();
        Self {
            parent_entity: None,
            tx: None,
            parsed_result: None,
            focus_handle: Some(focus_handle),
            bounds: Bounds::default(),
            selection_positions: (None, None),
            is_selecting: false,
            is_selectable: false,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
        }
    }
}

impl TextViewState {

    fn update_bounds(&mut self, bounds: Bounds<Pixels>) {
        if self.bounds.size != bounds.size {
            self.clear_selection();
        }
        self.bounds = bounds;
    }

    fn clear_selection(&mut self) {
        self.selection_positions = (None, None);
        self.is_selecting = false;
    }

    fn start_selection(&mut self, pos: Point<Pixels>) {
        let pos = pos - self.bounds.origin;
        self.selection_positions = (Some(pos), Some(pos));
        self.is_selecting = true;
    }

    fn update_selection(&mut self, pos: Point<Pixels>) {
        let pos = pos - self.bounds.origin;
        if let (Some(start), Some(_)) = self.selection_positions {
            self.selection_positions = (Some(start), Some(pos))
        }
    }

    fn end_selection(&mut self) {
        self.is_selecting = false;
    }

    pub(crate) fn has_selection(&self) -> bool {
        if let (Some(start), Some(end)) = self.selection_positions {
            start != end
        } else {
            false
        }
    }

    pub(crate) fn is_selectable(&self) -> bool {
        self.is_selectable
    }

    pub(crate) fn selection_bounds(&self) -> Bounds<Pixels> {
        selection_bounds(
            self.selection_positions.0,
            self.selection_positions.1,
            self.bounds,
        )
    }

    fn selection_text(&self) -> Option<String> {
        Some(
            self.parsed_result
                .as_ref()?
                .as_ref()
                .ok()?
                .root_node
                .selected_text(),
        )
    }
}

#[derive(IntoElement, Clone)]
pub enum Text {
    String(SharedString),
    TextView(Box<TextView>),
}

impl From<SharedString> for Text {
    fn from(s: SharedString) -> Self {
        Self::String(s)
    }
}

impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Self::String(SharedString::from(s.to_string()))
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Self::String(s.into())
    }
}

impl From<TextView> for Text {
    fn from(e: TextView) -> Self {
        Self::TextView(Box::new(e))
    }
}

impl Text {

    pub fn style(self, style: TextViewStyle) -> Self {
        match self {
            Self::String(s) => Self::String(s),
            Self::TextView(e) => Self::TextView(Box::new(e.style(style))),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::String(s) => s.as_str(),
            Self::TextView(view) => view.raw.as_str(),
        }
    }
}

impl RenderOnce for Text {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        match self {
            Self::String(s) => s.into_any_element(),
            Self::TextView(e) => e.into_any_element(),
        }
    }
}

impl Styled for TextView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl TextView {
    fn create_init_state(
        type_: TextViewType,
        text: &SharedString,
        highlight_theme: &Arc<HighlightTheme>,
        state: &Entity<TextViewState>,
        cx: &mut App,
    ) -> InitState {
        let state = state.read(cx);
        if let Some(tx) = &state.tx {
            InitState::Initialized { tx: tx.clone() }
        } else {
            InitState::Initializing {
                type_,
                text: text.clone(),
                style: Default::default(),
                highlight_theme: highlight_theme.clone(),
            }
        }
    }

    pub fn markdown(
        id: impl Into<ElementId>,
        markdown: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let id: ElementId = id.into();
        let markdown = markdown.into();
        let highlight_theme = cx.theme().highlight_theme.clone();
        let state =
            window.use_keyed_state(SharedString::from(format!("{}/state", id)), cx, |_, cx| {
                TextViewState::new(cx)
            });
        let init_state = Self::create_init_state(
            TextViewType::Markdown,
            &markdown,
            &highlight_theme,
            &state,
            cx,
        );
        if let Some(tx) = &state.read(cx).tx {
            let _ = tx.try_send(Update::Text(markdown.clone()));
        }
        Self {
            id,
            init_state: Some(init_state),
            raw: markdown.clone(),
            style: StyleRefinement::default(),
            state,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
        }
    }

    pub fn html(
        id: impl Into<ElementId>,
        html: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let id: ElementId = id.into();
        let html = html.into();
        let highlight_theme = cx.theme().highlight_theme.clone();
        let state =
            window.use_keyed_state(SharedString::from(format!("{}/state", id)), cx, |_, cx| {
                TextViewState::new(cx)
            });
        let init_state =
            Self::create_init_state(TextViewType::Html, &html, &highlight_theme, &state, cx);
        if let Some(tx) = &state.read(cx).tx {
            let _ = tx.try_send(Update::Text(html.clone()));
        }
        Self {
            id,
            init_state: Some(init_state),
            style: StyleRefinement::default(),
            state,
            raw: html,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
        }
    }

    pub fn text(mut self, raw: impl Into<SharedString>) -> Self {
        let raw: SharedString = raw.into();
        if let Some(init_state) = &mut self.init_state {
            match init_state {
                InitState::Initializing { text, .. } => *text = raw.clone(),
                InitState::Initialized { tx } => {
                    let _ = tx.try_send(Update::Text(raw.clone()));
                }
            }
        }
        self.raw = raw;
        self
    }

    pub fn style(mut self, style: TextViewStyle) -> Self {
        if let Some(init_state) = &mut self.init_state {
            match init_state {
                InitState::Initializing { style: s, .. } => **s = style,
                InitState::Initialized { tx } => {
                    let _ = tx.try_send(Update::Style(Box::new(style)));
                }
            }
        }
        self
    }

    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    fn on_action_copy(state: &Entity<TextViewState>, cx: &mut App) {
        let Some(selected_text) = state.read(cx).selection_text() else {
            return;
        };

        cx.write_to_clipboard(ClipboardItem::new_string(selected_text.trim().to_string()));
    }

    pub fn code_block_actions<F, E>(mut self, f: F) -> Self
    where
        F: Fn(&CodeBlock, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        self.code_block_actions = Some(Arc::new(move |code_block, window, cx| {
            f(&code_block, window, cx).into_any_element()
        }));
        self
    }
}

impl IntoElement for TextView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextView {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some(InitState::Initializing {
            type_,
            text,
            style,
            highlight_theme,
        }) = self.init_state.take()
        {
            let style = *style;
            let highlight_theme = highlight_theme.clone();
            let code_block_actions = self.code_block_actions.clone();
            let (tx, rx) = smol::channel::unbounded::<Update>();
            let (tx_result, rx_result) =
                smol::channel::unbounded::<Result<ParsedContent, SharedString>>();
            let parse_start = Instant::now();
            let parsed_result = parse_content(
                type_,
                &text,
                style.clone(),
                &highlight_theme,
                &code_block_actions,
            );
            record_parse("markdown.initial_parse", parse_start);

            self.state.update(cx, {
                let tx = tx.clone();
                |state, _| {
                    state.parsed_result = Some(parsed_result);
                    state.tx = Some(tx);
                }
            });

            cx.spawn({
                let state = self.state.downgrade();
                async move |cx| {
                    while let Ok(parsed_result) = rx_result.recv().await {
                        if let Some(state) = state.upgrade() {
                            _ = state.update(cx, |state, cx| {
                                state.parsed_result = Some(parsed_result);
                                if let Some(parent_entity) = state.parent_entity {
                                    let app = &mut **cx;
                                    app.notify(parent_entity);
                                }
                                state.clear_selection();
                            });
                        } else {
                            // state released, stopping processing
                            break;
                        }
                    }
                }
            })
            .detach();

            cx.background_spawn(UpdateFuture::new(
                type_,
                style,
                text,
                highlight_theme,
                rx,
                tx_result,
                // Workspace's live preview already generation-debounces its
                // pulldown parse. Keep native-node materialization short so it
                // does not add another visible 200 ms delay to that pipeline.
                Duration::from_millis(10),
                code_block_actions,
            ))
            .detach();

            self.init_state = Some(InitState::Initialized { tx });
        }

        let list_state = &self.state.read(cx).list_state;

        let focus_handle = self
            .state
            .read(cx)
            .focus_handle
            .as_ref()
            .expect("focus_handle should init by TextViewState::new");

        let mut el = div()
            .key_context(CONTEXT)
            .track_focus(focus_handle)
            .size_full()
            .relative()
            .on_action({
                let state = self.state.clone();
                move |_: &input::Copy, _, cx| {
                    Self::on_action_copy(&state, cx);
                }
            })
            .child(TextViewElement {
                list_state: if self.scrollable {
                    Some(list_state.clone())
                } else {
                    None
                },
                state: self.state.clone(),
            })
            .refine_style(&self.style)
            .vertical_scrollbar(list_state)
            .into_any_element();
        let layout_id = el.request_layout(window, cx);
        (layout_id, el)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        request_layout.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let entity_id = window.current_view();
        let is_selectable = self.selectable;

        self.state.update(cx, |state, _| {
            state.parent_entity = Some(entity_id);
            state.update_bounds(bounds);
            state.is_selectable = is_selectable;
        });

        GlobalState::global_mut(cx)
            .text_view_state_stack
            .push(self.state.clone());
        request_layout.paint(window, cx);
        GlobalState::global_mut(cx).text_view_state_stack.pop();

        if self.selectable {
            let is_selecting = self.state.read(cx).is_selecting;
            let has_selection = self.state.read(cx).has_selection();

            window.on_mouse_event({
                let state = self.state.clone();
                move |event: &MouseDownEvent, phase, _, cx| {
                    if !bounds.contains(&event.position) || !phase.bubble() {
                        return;
                    }

                    state.update(cx, |state, _| {
                        state.start_selection(event.position);
                    });
                    cx.notify(entity_id);
                }
            });

            if is_selecting {
                // move to update end position.
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |event: &MouseMoveEvent, phase, _, cx| {
                        if !phase.bubble() {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.update_selection(event.position);
                        });
                        cx.notify(entity_id);
                    }
                });

                // up to end selection
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |_: &MouseUpEvent, phase, _, cx| {
                        if !phase.bubble() {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.end_selection();
                        });
                        cx.notify(entity_id);
                    }
                });
            }

            if has_selection {
                // down outside to clear selection
                window.on_mouse_event({
                    let state = self.state.clone();
                    move |event: &MouseDownEvent, _, _, cx| {
                        if bounds.contains(&event.position) {
                            return;
                        }

                        state.update(cx, |state, _| {
                            state.clear_selection();
                        });
                        cx.notify(entity_id);
                    }
                });
            }
        }
    }
}

fn parse_content(
    type_: TextViewType,
    text: &str,
    style: TextViewStyle,
    highlight_theme: &HighlightTheme,
    code_block_actions: &Option<Arc<CodeBlockActionsFn>>,
) -> Result<ParsedContent, SharedString> {
    let mut node_cx = NodeContext {
        style: style.clone(),
        code_block_actions: code_block_actions.clone(),
        ..NodeContext::default()
    };

    let res = match type_ {
        TextViewType::Markdown => {
            super::format::markdown::parse(text, &style, &mut node_cx, highlight_theme)
        }
        TextViewType::Html => super::format::html::parse(text, &mut node_cx),
    };
    res.map(move |root_node| ParsedContent { root_node, node_cx })
}

fn selection_bounds(
    start: Option<Point<Pixels>>,
    end: Option<Point<Pixels>>,
    bounds: Bounds<Pixels>,
) -> Bounds<Pixels> {
    if let (Some(start), Some(end)) = (start, end) {
        let start = start + bounds.origin;
        let end = end + bounds.origin;

        let origin = Point {
            x: start.x.min(end.x),
            y: start.y.min(end.y),
        };
        let size = Size {
            width: (start.x - end.x).abs(),
            height: (start.y - end.y).abs(),
        };

        return Bounds { origin, size };
    }

    Bounds::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, point, px, size};

    #[test]
    fn test_text_view_state_selection_bounds() {
        assert_eq!(
            selection_bounds(None, None, Default::default()),
            Bounds::default()
        );
        assert_eq!(
            selection_bounds(None, Some(point(px(10.), px(20.))), Default::default()),
            Bounds::default()
        );
        assert_eq!(
            selection_bounds(Some(point(px(10.), px(20.))), None, Default::default()),
            Bounds::default()
        );

        // 10,10 start
        //   |------|
        //   |      |
        //   |------|
        //         50,50
        assert_eq!(
            selection_bounds(
                Some(point(px(10.), px(10.))),
                Some(point(px(50.), px(50.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        // 10,10
        //   |------|
        //   |      |
        //   |------|
        //         50,50 start
        assert_eq!(
            selection_bounds(
                Some(point(px(50.), px(50.))),
                Some(point(px(10.), px(10.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        //        50,10 start
        //   |------|
        //   |      |
        //   |------|
        // 10,50
        assert_eq!(
            selection_bounds(
                Some(point(px(50.), px(10.))),
                Some(point(px(10.), px(50.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
        //        50,10
        //   |------|
        //   |      |
        //   |------|
        // 10,50 start
        assert_eq!(
            selection_bounds(
                Some(point(px(10.), px(50.))),
                Some(point(px(50.), px(10.))),
                Default::default()
            ),
            Bounds {
                origin: point(px(10.), px(10.)),
                size: size(px(40.), px(40.))
            }
        );
    }
}
