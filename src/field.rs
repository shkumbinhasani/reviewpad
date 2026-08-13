//! A text field, following the pattern gpui documents in its `examples/input.rs`.
//!
//! The first cut of this was a `div` holding a `String` with keystrokes appended
//! by hand. It could type and not much else. This follows the reference
//! implementation instead, which matters in four specific ways:
//!
//! * **UTF-16 offsets.** [`EntityInputHandler`] speaks the offsets the platform
//!   uses, not Rust byte offsets. Treating one as the other is silently correct
//!   for ASCII and corrupts the text for anything else.
//! * **Actions and key bindings** rather than matching on key names, so the
//!   bindings are declarative and scoped to this field's key context instead of
//!   racing the shortcuts the review view binds.
//! * **Grapheme boundaries** for caret movement, so an emoji or a combining
//!   mark moves as one unit instead of splitting into invalid UTF-8.
//! * **`replace_text_in_range` as the only mutation path**, so typing, pasting,
//!   deleting and IME composition all converge on one implementation.
//!
//! It departs from the example in one respect: that example is single-line, and
//! a review comment needs paragraphs, so this shapes wrapped text with
//! `shape_text` and paints line by line.

use gpui::{
    App, Bounds, Context, CursorStyle, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, Render, SharedString, Style, TextAlign, TextRun, UTF16Selection,
    UnderlineStyle, Window, WrappedLine, actions, div, fill, point, prelude::*, px, relative, size,
};
use smallvec::SmallVec;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

actions!(
    review_field,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectAll,
        Home,
        End,
        Paste,
        Copy,
        Cut,
        Newline,
        ShowCharacterPalette,
    ]
);

/// Key context the bindings are scoped to. Without it, binding `left` and `up`
/// would claim those keys window-wide and break the file-list navigation.
pub const CONTEXT: &str = "ReviewField";

/// Register the field's bindings. Call once when the app starts.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("alt-left", WordLeft, Some(CONTEXT)),
        KeyBinding::new("alt-right", WordRight, Some(CONTEXT)),
        KeyBinding::new("cmd-left", Home, Some(CONTEXT)),
        KeyBinding::new("cmd-right", End, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("enter", Newline, Some(CONTEXT)),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(CONTEXT)),
    ]);
}

/// How the field is drawn. The palette belongs to the caller.
#[derive(Clone)]
pub struct FieldStyle {
    pub text: Hsla,
    pub placeholder: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
    pub font_size: Pixels,
    pub line_height: Pixels,
}

pub struct TextField {
    content: SharedString,
    placeholder: SharedString,
    /// Byte range into `content`; empty means a caret.
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// Range the IME is still composing — provisional text.
    marked_range: Option<Range<usize>>,
    style: FieldStyle,
    focus_handle: FocusHandle,
    last_layout: Option<SmallVec<[WrappedLine; 1]>>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl TextField {
    pub fn new(
        placeholder: impl Into<SharedString>,
        style: FieldStyle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            style,
            focus_handle: cx.focus_handle(),
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    // -- caret and selection -------------------------------------------------

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.sanitize(offset..offset).start;
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.sanitize(offset..offset).start;
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical(false);
        self.move_to(offset, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical(true);
        self.move_to(offset, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical(false);
        self.select_to(offset, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical(true);
        self.select_to(offset, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.previous_word(self.cursor_offset());
        self.move_to(offset, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.next_word(self.cursor_offset());
        self.move_to(offset, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.line_start(self.cursor_offset());
        self.move_to(offset, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.line_end(self.cursor_offset());
        self.move_to(offset, cx);
    }

    // -- editing, all of it funnelled through replace_text_in_range ----------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let selected = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(selected));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let selected = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(selected));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace("\r\n", "\n"), window, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // -- mouse ----------------------------------------------------------------

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        window.focus(&self.focus_handle);
        let offset = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_position(event.position);
            self.select_to(offset, cx);
        }
    }

    // -- geometry across wrapped lines ---------------------------------------

    /// Byte offset under a window-space point.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let (Some(lines), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds) else {
            return self.content.len();
        };
        let line_height = self.style.line_height;
        let mut origin = bounds.origin;
        let mut consumed = 0;

        let last = lines.len().saturating_sub(1);
        for (index, line) in lines.iter().enumerate() {
            let height = line.size(line_height).height;
            if position.y < origin.y + height || index == last {
                let local = position - origin;
                return consumed
                    + match line.index_for_position(local, line_height) {
                        Ok(offset) => offset,
                        Err(offset) => offset,
                    };
            }
            origin.y += height;
            // `shape_text` splits on newlines and drops them from the lines.
            consumed += line.len() + 1;
        }
        self.content.len()
    }

    /// Window-space position of a byte offset.
    fn position_for_index(&self, offset: usize) -> Option<Point<Pixels>> {
        let (lines, bounds) = (self.last_layout.as_ref()?, self.last_bounds?);
        let line_height = self.style.line_height;
        let mut origin = bounds.origin;
        let mut consumed = 0;

        for line in lines.iter() {
            let len = line.len();
            if offset <= consumed + len {
                return line
                    .position_for_index(offset - consumed, line_height)
                    .map(|position| origin + position);
            }
            origin.y += line.size(line_height).height;
            consumed += len + 1;
        }
        None
    }

    /// One visual line up or down, holding the column — so it follows wrapping
    /// as the eye sees it rather than as the string is stored.
    fn vertical(&self, down: bool) -> usize {
        let cursor = self.cursor_offset();
        let (Some(position), Some(bounds)) = (self.position_for_index(cursor), self.last_bounds)
        else {
            return cursor;
        };
        let line_height = self.style.line_height;
        let target = point(
            position.x,
            position.y + if down { line_height } else { -line_height },
        );

        // Off either end: go to the document edge, as a native field does.
        if target.y < bounds.origin.y {
            return 0;
        }
        if target.y >= bounds.origin.y + bounds.size.height {
            return self.content.len();
        }
        self.index_for_position(target)
    }

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..]
            .find('\n')
            .map(|index| offset + index)
            .unwrap_or(self.content.len())
    }

    // -- boundaries -----------------------------------------------------------

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        self.content[..offset]
            .split_word_bound_indices()
            .rfind(|(_, word)| !word.trim().is_empty())
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_word(&self, offset: usize) -> usize {
        self.content[offset..]
            .split_word_bound_indices()
            .find(|(index, word)| *index > 0 && !word.trim().is_empty())
            .map(|(index, _)| offset + index)
            .unwrap_or(self.content.len())
    }

    // -- UTF-16, which is what the platform speaks ---------------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        offset_from_utf16(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        offset_to_utf16(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Clamp a range into the current content and snap it to char boundaries.
    ///
    /// Both halves matter. The platform hands us ranges describing a buffer
    /// state it believes we are in, and our own stored selection can outlive
    /// the text it referred to — the view clears the composer while AppKit is
    /// still mid-keystroke, and the `insertText:` that follows arrives against
    /// content that is already empty. These handlers run in callbacks that
    /// cannot unwind, so an out-of-bounds slice aborts the process rather than
    /// raising an error: the range has to be made valid, not assumed valid.
    fn sanitize(&self, range: Range<usize>) -> Range<usize> {
        let clamp = |offset: usize| {
            let mut offset = offset.min(self.content.len());
            while offset > 0 && !self.content.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        };
        let start = clamp(range.start);
        start..clamp(range.end).max(start)
    }
}

/// Byte offset for a UTF-16 offset. Free functions so they can be tested
/// without standing up a window.
fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    let mut utf8 = 0;
    for character in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += character.len_utf16();
        utf8 += character.len_utf8();
    }
    utf8
}

fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    let mut utf8 = 0;
    for character in text.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += character.len_utf8();
        utf16 += character.len_utf16();
    }
    utf16
}

impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.sanitize(self.range_from_utf16(&range_utf16));
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.sanitize(
            range_utf16
                .as_ref()
                .map(|range| self.range_from_utf16(range))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
        );

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        let caret = range.start + new_text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.sanitize(
            range_utf16
                .as_ref()
                .map(|range| self.range_from_utf16(range))
                .or(self.marked_range.clone())
                .unwrap_or(self.selected_range.clone()),
        );

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|selected| {
                let start = range.start;
                offset_from_utf16(new_text, selected.start) + start
                    ..offset_from_utf16(new_text, selected.end) + start
            })
            .unwrap_or_else(|| {
                let caret = range.start + new_text.len();
                caret..caret
            });
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Where macOS parks the IME candidate window.
        let range = self.sanitize(self.range_from_utf16(&range_utf16));
        let start = self.position_for_index(range.start)?;
        Some(Bounds::new(start, size(px(1.), self.style.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_position(point)))
    }
}

impl Render for TextField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .cursor(CursorStyle::IBeam)
            .w_full()
            .child(FieldElement { field: cx.entity() })
    }
}

/// Shapes and paints the field. Selection is a background run on the shaped
/// text rather than a hand-placed rectangle, so it follows wrapping for free.
struct FieldElement {
    field: Entity<TextField>,
}

struct Prepaint {
    lines: SmallVec<[WrappedLine; 1]>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for FieldElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl FieldElement {
    /// What to draw and the runs to draw it with: the placeholder when empty,
    /// otherwise the content with the selection filled and composition
    /// underlined.
    fn content(&self, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>, bool) {
        let field = self.field.read(cx);
        let font = window.text_style().font();
        let empty = field.content.is_empty();
        let content: SharedString = if empty {
            field.placeholder.clone()
        } else {
            field.content.clone()
        };

        let run = |len: usize, background: Option<Hsla>, underline: bool| TextRun {
            len,
            font: font.clone(),
            color: if empty {
                field.style.placeholder
            } else {
                field.style.text
            },
            background_color: background,
            underline: underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(field.style.text),
                wavy: false,
            }),
            strikethrough: None,
        };

        let selection = field.selected_range.clone();
        let runs = if empty {
            vec![run(content.len(), None, false)]
        } else if !selection.is_empty() {
            vec![
                run(selection.start, None, false),
                run(selection.len(), Some(field.style.selection), false),
                run(content.len() - selection.end, None, false),
            ]
        } else if let Some(marked) = field.marked_range.clone() {
            vec![
                run(marked.start, None, false),
                run(marked.len(), None, true),
                run(content.len() - marked.end, None, false),
            ]
        } else {
            vec![run(content.len(), None, false)]
        };

        (
            content,
            runs.into_iter().filter(|run| run.len > 0).collect(),
            empty,
        )
    }
}

impl Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
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
        let (line_height, font_size) = {
            let style = &self.field.read(cx).style;
            (style.line_height, style.font_size)
        };
        let (content, runs, _) = self.content(window, cx);

        let mut style = Style::default();
        style.size.width = relative(1.).into();

        // Measured rather than fixed, so the box grows as the comment wraps.
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.unwrap_or(match available.width {
                gpui::AvailableSpace::Definite(width) => width,
                _ => px(320.),
            });
            let height = window
                .text_system()
                .shape_text(content.clone(), font_size, &runs, Some(width), None)
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| line.size(line_height).height)
                        .fold(px(0.), |total, height| total + height)
                })
                .unwrap_or(line_height);
            size(width, height.max(line_height))
        });

        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (content, runs, empty) = self.content(window, cx);
        let (line_height, font_size, caret) = {
            let style = &self.field.read(cx).style;
            (style.line_height, style.font_size, style.caret)
        };

        let lines = window
            .text_system()
            .shape_text(content, font_size, &runs, Some(bounds.size.width), None)
            .unwrap_or_default();

        // The field resolves clicks and caret positions against these.
        self.field.update(cx, |field, _| {
            field.last_layout = Some(lines.clone());
            field.last_bounds = Some(bounds);
        });

        let field = self.field.read(cx);
        let cursor = (!empty && field.focus_handle.is_focused(window))
            .then(|| field.position_for_index(field.cursor_offset()))
            .flatten()
            .map(|position| fill(Bounds::new(position, size(px(1.5), line_height)), caret));

        Prepaint { lines, cursor }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, line_height) = {
            let field = self.field.read(cx);
            (field.focus_handle.clone(), field.style.line_height)
        };

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );

        let mut origin = bounds.origin;
        for line in prepaint.lines.iter() {
            line.paint_background(origin, line_height, TextAlign::Left, None, window, cx)
                .ok();
            line.paint(origin, line_height, TextAlign::Left, None, window, cx)
                .ok();
            origin.y += line.size(line_height).height;
        }

        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
    }
}

/// Build a field entity.
pub fn text_field(
    placeholder: impl Into<SharedString>,
    style: FieldStyle,
    cx: &mut App,
) -> Entity<TextField> {
    cx.new(|cx| TextField::new(placeholder, style, cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform hands the input handler UTF-16 offsets. Treating them as
    /// byte offsets is silently correct for ASCII and wrong for everything
    /// else — this is the bug the first version of this file shipped with.
    #[test]
    fn utf16_and_byte_offsets_convert_both_ways() {
        let text = "héllo 🌍";
        // "é" is two bytes but one UTF-16 unit.
        assert_eq!(offset_from_utf16(text, 1), 1);
        assert_eq!(offset_from_utf16(text, 2), 3);
        assert_eq!(offset_to_utf16(text, 3), 2);
        // The emoji starts at byte 7 / unit 6, and is four bytes but two units.
        assert_eq!(offset_to_utf16(text, 7), 6);
        assert_eq!(offset_from_utf16(text, 6), 7);
        assert_eq!(offset_to_utf16(text, 11), 8);
        assert_eq!(offset_from_utf16(text, 8), 11);
    }

    #[test]
    fn ascii_offsets_are_unchanged() {
        let text = "plain";
        for offset in 0..=text.len() {
            assert_eq!(offset_from_utf16(text, offset), offset);
            assert_eq!(offset_to_utf16(text, offset), offset);
        }
    }

    #[test]
    fn the_caret_steps_by_grapheme() {
        let text = "a🌍b";
        // Stepping over the emoji must not land inside it, which would panic
        // on the next slice.
        assert_eq!(next_boundary(text, 1), 5);
        assert_eq!(previous_boundary(text, 5), 1);
    }

    #[test]
    fn combining_marks_move_as_one_unit() {
        // "e" plus a combining acute accent is two chars but one grapheme.
        let text = "e\u{0301}x";
        assert_eq!(next_boundary(text, 0), 3);
        assert_eq!(previous_boundary(text, 3), 0);
    }

    /// The crash this file shipped with: the composer was cleared while AppKit
    /// was mid-keystroke, and the `insertText:` that followed sliced the now
    /// empty content with the old selection. Because that runs in a callback
    /// that cannot unwind, the panic aborted the process outright.
    #[test]
    fn a_stale_range_is_clamped_rather_than_slicing_out_of_bounds() {
        // Selection from before the clear, against content that is now empty.
        assert_eq!(sanitize("", 14..20), 0..0);
        // And against content shorter than the range.
        assert_eq!(sanitize("abc", 1..99), 1..3);
        assert_eq!(sanitize("abc", 99..99), 3..3);
    }

    #[test]
    fn sanitizing_snaps_off_a_char_boundary() {
        // "é" spans bytes 0..2, so byte 1 is interior and snaps back to 0.
        assert_eq!(sanitize("é", 1..2), 0..2);
        // In "aéb" the boundaries are 0, 1, 3, 4 — byte 2 snaps back to 1.
        assert_eq!(sanitize("aéb", 2..4), 1..4);
        assert_eq!(sanitize("aéb", 2..2), 1..1);
    }

    #[test]
    fn a_reversed_range_cannot_survive_sanitizing() {
        // Built explicitly: a reversed literal is a lint, but the platform can
        // still hand us one.
        assert_eq!(
            sanitize("abcdef", Range { start: 4, end: 2 }),
            Range { start: 4, end: 4 }
        );
    }

    /// Mirrors `TextField::sanitize` against a plain string.
    fn sanitize(content: &str, range: Range<usize>) -> Range<usize> {
        let clamp = |offset: usize| {
            let mut offset = offset.min(content.len());
            while offset > 0 && !content.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        };
        let start = clamp(range.start);
        start..clamp(range.end).max(start)
    }

    fn previous_boundary(text: &str, offset: usize) -> usize {
        text.grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(text: &str, offset: usize) -> usize {
        text.grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(text.len())
    }
}
