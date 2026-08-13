use anyhow::Result;
use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, ClipboardItem, Context, FocusHandle,
    Focusable, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, KeyDownEvent, MouseButton,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement, StyledText, Window,
    WindowBounds, WindowOptions, div, point, prelude::*, px, rgb, rgba, size,
};
use std::ops::Range;

use reviewpad::{
    git::{DiffLine, DiffSet, FileDiff, LineKind, Repository},
    review::{Review, ReviewComment, thread_of},
    syntax::{DiffHighlight, Grammar, SCOPE_COLORS, Span, SyntaxIndex},
    update,
};

// ReviewPad shares tgip's window language: a transparent, blurred window whose
// sidebar sits directly on the glass, with the content inset as a rounded,
// hairline-bordered card. Colors are translucent white/black over that glass
// rather than opaque panels, so both apps read as one family.

/// Inset between the window edge and its contents — tgip's `outerPadding`.
const OUTER_PADDING: f32 = 10.;
/// tgip derives this from the window radius (20) minus the outer padding.
const CARD_RADIUS: f32 = 10.;
const SIDEBAR_WIDTH: f32 = 262.;
/// Vertical room the sidebar leaves for the traffic lights.
const TITLEBAR_INSET: f32 = 32.;
/// Width of each line-number gutter.
const GUTTER: f32 = 44.;
/// Width of the +/− column, tgip's marker column narrowed for the gutters.
const MARKER: f32 = 24.;
const TAB_WIDTH: usize = 4;

/// Git accent — the gold tgip uses for branch glyphs and dirty badges.
const ACCENT: u32 = 0xffd173;
/// Fill behind gold-on-dark badges and the primary button.
const ACCENT_FILL: u32 = 0x472b0fd9;

// Diff line palette, lifted from tgip's `GitDiffRenderedLineKind`.
const META_BG: u32 = 0x21242eeb;
const META_TEXT: u32 = 0xc7c7c7e6;
const HUNK_BG: u32 = 0x1c2e4deb;
const HUNK_TEXT: u32 = 0xadd4fffa;
const ADD_BG: u32 = 0x123d29f5;
const ADD_TEXT: u32 = 0xc2ffd1fa;
const ADD_MARK: u32 = 0x7afaa6fa;
const DEL_BG: u32 = 0x471a1cf5;
const DEL_TEXT: u32 = 0xffc7ccfa;
const DEL_MARK: u32 = 0xff9499fa;
const CONTEXT_BG: u32 = 0xffffff06;
const CONTEXT_TEXT: u32 = 0xebebebe0;
const NOTE_BG: u32 = 0x403314eb;
const NOTE_TEXT: u32 = 0xffe699f5;
/// Unhighlighted code — One Dark's default foreground, so syntax spans and the
/// text around them belong to the same palette.
const CODE_TEXT: u32 = 0xc8ccd4f5;

// File status tints, matching tgip's `GitChangedFile.tintColor`.
const TINT_ADDED: u32 = 0x6bdb8f;
const TINT_DELETED: u32 = 0xff806b;
const TINT_MODIFIED: u32 = 0x7abdff;

/// Signature on notes written in the app, so a thread shows who said what when
/// an agent replies from the CLI with `--author`.
const AUTHOR: &str = "you";

/// Explicit installed coding face. `SF Mono` is not exposed under that family
/// name on every Mac, which made CoreText silently fall back to an ugly face.
const MONO: &str = "JetBrainsMono Nerd Font Mono";

/// Translucent white — tgip's `adaptiveForeground` on a dark theme.
fn fg(alpha: f32) -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: alpha,
    }
}

/// Translucent black — tgip's `adaptiveScrim`, and the stand-in for its
/// `hudWindow` material (gpui's blur is colorless, so the tint is ours to paint).
fn scrim(alpha: f32) -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.,
        a: alpha,
    }
}

fn hex(value: u32) -> Hsla {
    rgba(value).into()
}

fn tint(value: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = rgb(value).into();
    color.a = alpha;
    color
}

#[derive(Clone)]
struct Anchor {
    file: usize,
    line: usize,
}

pub fn run(repository: Repository, diff: DiffSet, print_on_finish: bool) -> Result<()> {
    let review = Review::open(&repository)?;

    Application::new().run(move |cx: &mut App| {
        open_review_window(cx, repository, diff, review, print_on_finish);
    });
    Ok(())
}

/// Launch the desktop app without a terminal working directory and let the user
/// choose a Git repository with the native directory picker.
pub fn pick_and_run() -> Result<()> {
    Application::new().run(|cx: &mut App| {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Review repository".into()),
        });
        cx.spawn(async move |cx| {
            let selected = receiver.await.ok().and_then(Result::ok).flatten();
            let Some(path) = selected.and_then(|paths| paths.into_iter().next()) else {
                let _ = cx.update(|cx| cx.quit());
                return;
            };
            match Repository::discover(&path)
                .and_then(|repository| repository.diff().map(|diff| (repository, diff)))
            {
                Ok((repository, diff)) => {
                    let review = Review::open(&repository).unwrap_or_default();
                    let _ = cx.update(|cx| {
                        open_review_window(cx, repository, diff, review, false);
                    });
                }
                Err(error) => {
                    eprintln!("error: {error:#}");
                    let _ = cx.update(|cx| cx.quit());
                }
            }
        })
        .detach();
    });
    Ok(())
}

fn open_review_window(
    cx: &mut App,
    repository: Repository,
    diff: DiffSet,
    review: Review,
    print_on_finish: bool,
) {
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some("dev.reviewpad.ReviewPad".into()),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from(format!(
                "ReviewPad — {}",
                repository
                    .root
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default()
            ))),
            appears_transparent: true,
            // Line the lights up with the sidebar's own leading edge, the way
            // tgip draws its window dots inside the sidebar. The vertical offset
            // stays inside the 28pt titlebar so AppKit doesn't clip the buttons.
            traffic_light_position: Some(point(px(20.), px(14.))),
        }),
        window_background: gpui::WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };

    cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| {
            let mut view = ReviewView {
                repository,
                diff,
                review,
                selected_file: 0,
                anchor: None,
                reply_to: None,
                draft: String::new(),
                focus: cx.focus_handle(),
                print_on_finish,
                status: None,
                syntax: SyntaxIndex::new(),
                highlight: DiffHighlight::default(),
                update: None,
                update_task: None,
            };
            view.refresh_highlight();
            view.check_for_update(cx);
            view
        });
        window.focus(&view.read(cx).focus);
        view
    })
    .expect("failed to open ReviewPad window");
    cx.activate(true);
}

struct ReviewView {
    repository: Repository,
    diff: DiffSet,
    review: Review,
    selected_file: usize,
    anchor: Option<Anchor>,
    /// Id of the thread the composer is answering, when it is not anchored to a
    /// diff line.
    reply_to: Option<String>,
    draft: String,
    focus: FocusHandle,
    print_on_finish: bool,
    status: Option<String>,
    syntax: SyntaxIndex,
    highlight: DiffHighlight,
    /// Version of a newer release, once the background check has answered.
    update: Option<String>,
    /// Held so the check is cancelled if the window closes first.
    update_task: Option<gpui::Task<()>>,
}

impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ReviewView {
    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_file = index;
        self.anchor = None;
        self.reply_to = None;
        self.draft.clear();
        self.refresh_highlight();
        cx.notify();
    }

    /// Ask the release feed whether something newer exists. The request runs on
    /// a background thread and the answer is advisory — a failed check leaves
    /// the UI exactly as it was rather than interrupting a review.
    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.update_task = Some(cx.spawn(async move |view, cx| {
            let Some(manifest) = cx.background_spawn(async { update::latest() }).await else {
                return;
            };
            if !update::is_newer(&manifest.version, update::VERSION) {
                return;
            }
            let _ = view.update(cx, |view, cx| {
                view.update = Some(manifest.version);
                cx.notify();
            });
        }));
    }

    /// Reparse the selected file so its hunks can be painted with real syntax
    /// colors. Only the file on screen is parsed, and only when it changes.
    fn refresh_highlight(&mut self) {
        self.highlight = match self.diff.files.get(self.selected_file) {
            Some(file) => DiffHighlight::load(&self.repository, file, &mut self.syntax),
            None => DiffHighlight::default(),
        };
    }

    fn select_line(
        &mut self,
        file: usize,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.anchor = Some(Anchor { file, line });
        self.reply_to = None;
        self.status = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn step_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.diff.files.is_empty() {
            return;
        }
        let last = self.diff.files.len() - 1;
        let next = (self.selected_file as isize + delta).clamp(0, last as isize) as usize;
        if next != self.selected_file {
            self.select_file(next, cx);
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let composing = self.anchor.is_some() || self.reply_to.is_some();

        match key {
            // Outside the composer the arrows walk the file list, so the diff
            // is navigable without reaching for the mouse.
            "up" if !composing => {
                self.step_file(-1, cx);
                return;
            }
            "down" if !composing => {
                self.step_file(1, cx);
                return;
            }
            "v" if modifiers.platform && composing => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.draft.push_str(&text);
                }
            }
            "backspace" if composing => {
                self.draft.pop();
            }
            "enter" if composing && (modifiers.platform || modifiers.control) => {
                self.add_comment(cx);
                return;
            }
            "enter" if composing => self.draft.push('\n'),
            "escape" => {
                self.draft.clear();
                self.anchor = None;
            }
            _ if composing && !modifiers.platform && !modifiers.control && !modifiers.function => {
                if let Some(character) = &event.keystroke.key_char {
                    self.draft.push_str(character);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn add_comment(&mut self, cx: &mut Context<Self>) {
        let body = self.draft.trim().to_string();
        if body.is_empty() {
            self.status = Some("Write a comment first".into());
            cx.notify();
            return;
        }

        // Answering an existing note continues its thread; otherwise the draft
        // opens a new one on the selected line.
        if let Some(target) = self.reply_to.clone() {
            match self.review.add_reply(&target, AUTHOR, body) {
                Ok(_) => self.finish_composing("Reply saved", cx),
                Err(error) => {
                    self.status = Some(format!("{error}"));
                    cx.notify();
                }
            }
            return;
        }

        let Some(anchor) = self.anchor.clone() else {
            self.status = Some("Select a diff line first".into());
            cx.notify();
            return;
        };
        let Some(file) = self.diff.files.get(anchor.file) else {
            return;
        };
        let Some(line) = file.lines.get(anchor.line) else {
            return;
        };
        let Some((side, number)) = line.anchor() else {
            self.status = Some("Select a code line, not a diff header".into());
            cx.notify();
            return;
        };

        let context = file.context_at(anchor.line);
        let path = file.path.clone();
        self.review
            .add_comment(path, side, number, AUTHOR, body, context);
        self.finish_composing("Comment saved", cx);
    }

    /// Clear the composer and persist, reporting whichever outcome the save had.
    fn finish_composing(&mut self, saved: &str, cx: &mut Context<Self>) {
        self.draft.clear();
        self.anchor = None;
        self.reply_to = None;
        self.status = match self.review.save(&self.repository.review_path()) {
            Ok(()) => Some(saved.into()),
            Err(error) => Some(format!("Could not save: {error}")),
        };
        cx.notify();
    }

    fn delete_comment(&mut self, id: String, cx: &mut Context<Self>) {
        if self.review.remove(&id).is_err() {
            return;
        }
        if self.reply_to.as_deref() == Some(id.as_str()) {
            self.reply_to = None;
            self.draft.clear();
        }
        self.status = match self.review.save(&self.repository.review_path()) {
            Ok(()) => Some("Comment removed".into()),
            Err(error) => Some(format!("Could not save: {error}")),
        };
        cx.notify();
    }

    /// Aim the composer at a thread instead of a diff line.
    fn start_reply(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_to = Some(id);
        self.anchor = None;
        self.draft.clear();
        self.status = None;
        window.focus(&self.focus);
        cx.notify();
    }

    fn cancel_comment(&mut self, cx: &mut Context<Self>) {
        self.draft.clear();
        self.anchor = None;
        self.reply_to = None;
        self.status = None;
        cx.notify();
    }

    /// Put the upgrade command on the clipboard — the app never rewrites itself
    /// out from under a review, and a Homebrew copy must not be touched at all.
    fn copy_update_command(&mut self, cx: &mut Context<Self>) {
        let command = update::Install::detect().upgrade_hint();
        cx.write_to_clipboard(ClipboardItem::new_string(command.to_string()));
        self.status = Some(format!("Copied `{command}`"));
        cx.notify();
    }

    fn copy_markdown(&mut self, cx: &mut Context<Self>) {
        let markdown = self.review.markdown(&self.repository.root);
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
        self.status = Some(format!(
            "Copied {} comment{} as Markdown",
            self.review.len(),
            if self.review.len() == 1 { "" } else { "s" }
        ));
        cx.notify();
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.review.save(&self.repository.review_path()) {
            self.status = Some(format!("Could not save: {error}"));
            cx.notify();
            return;
        }
        if self.print_on_finish {
            print!("{}", self.review.markdown(&self.repository.root));
        }
        window.remove_window();
        cx.quit();
    }

    /// tgip's `InspectorButton`: a translucent pill that brightens on hover.
    fn button(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px(px(10.))
            .py(px(6.))
            .rounded(px(6.))
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if primary { tint(ACCENT, 0.96) } else { fg(0.5) })
            .bg(if primary { hex(ACCENT_FILL) } else { fg(0.06) })
            .border_1()
            .border_color(if primary { tint(ACCENT, 0.22) } else { fg(0.) })
            .cursor_pointer()
            .hover(|style| {
                style.bg(if primary {
                    tint(0x6b3f14, 0.92)
                } else {
                    fg(0.12)
                })
            })
            .active(|style| style.opacity(0.7))
            .child(label.into())
    }

    /// A hairline rule — tgip leans on `Divider().opacity(...)` throughout.
    fn divider(alpha: f32) -> gpui::Div {
        div().h(px(1.)).flex_none().bg(fg(alpha))
    }

    /// A small monospace capsule, tgip's badge shape.
    fn capsule(label: impl Into<SharedString>, color: Hsla, fill: Hsla) -> gpui::Div {
        div()
            .flex_none()
            .px(px(8.))
            .py(px(3.))
            .rounded_full()
            .bg(fill)
            .font_family(MONO)
            .text_size(px(10.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(color)
            .child(label.into())
    }

    fn render_file(
        &self,
        index: usize,
        file: &FileDiff,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let selected = index == self.selected_file;
        let name = file
            .path
            .rsplit_once('/')
            .map_or(file.path.as_str(), |(_, name)| name);
        let (status_color, status_symbol) = if file.additions > 0 && file.deletions > 0 {
            (TINT_MODIFIED, "•")
        } else if file.additions > 0 {
            (TINT_ADDED, "+")
        } else {
            (TINT_DELETED, "−")
        };

        div()
            .id(("file", index))
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(9.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(if selected { fg(0.12) } else { fg(0.) })
            .cursor_pointer()
            .hover(|style| style.bg(if selected { fg(0.14) } else { fg(0.06) }))
            .on_click(cx.listener(move |this, _, _, cx| this.select_file(index, cx)))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .text_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(status_color))
                    .child(status_symbol),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(12.5))
                    .font_weight(if selected {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(fg(if selected { 0.95 } else { 0.74 }))
                    .child(name.to_string()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(5.))
                    .text_size(px(10.))
                    .font_family(MONO)
                    .child(
                        div()
                            .text_color(hex(ADD_MARK))
                            .child(format!("+{}", file.additions)),
                    )
                    .child(
                        div()
                            .text_color(hex(DEL_MARK))
                            .child(format!("−{}", file.deletions)),
                    ),
            )
    }

    /// Hunk and metadata rows read as section bars: full bleed, no gutters, so
    /// the eye can use them to find its place in a long diff.
    fn render_band(&self, index: usize, line: &DiffLine) -> gpui::Stateful<gpui::Div> {
        let hunk = line.kind == LineKind::Hunk;
        let (background, foreground) = if hunk {
            (HUNK_BG, HUNK_TEXT)
        } else {
            (META_BG, META_TEXT)
        };
        // `@@ -12,7 +12,9 @@ fn parse(…)` — the trailing context git tacks on is
        // a hint, not part of the range, so it is dimmed.
        let (range, context) = match line.text.match_indices("@@").nth(1) {
            Some((at, _)) if hunk => (
                line.text[..at + 2].to_string(),
                line.text[at + 2..].trim().to_string(),
            ),
            _ => (line.text.clone(), String::new()),
        };

        div()
            .id(("band", index))
            .flex()
            .items_center()
            .gap(px(10.))
            .min_w_full()
            .px(px(14.))
            .py(px(1.))
            .bg(hex(background))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hex(foreground))
                    .child(range),
            )
            .when(!context.is_empty(), |element| {
                element.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(fg(0.42))
                        .child(context),
                )
            })
    }

    fn render_line(
        &self,
        file_index: usize,
        line_index: usize,
        line: &DiffLine,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let selected = self
            .anchor
            .as_ref()
            .is_some_and(|anchor| anchor.file == file_index && anchor.line == line_index);
        let (background, fallback, marker, marker_color) = match line.kind {
            LineKind::Addition => (ADD_BG, ADD_TEXT, "+", ADD_MARK),
            LineKind::Deletion => (DEL_BG, DEL_TEXT, "−", DEL_MARK),
            _ => (CONTEXT_BG, CONTEXT_TEXT, "", CONTEXT_TEXT),
        };

        // The marker gets its own column, so the code starts at a stable
        // offset and lines up with the file it came from.
        let spans = self.highlight.spans(line);
        let (display, highlights) = expand_tabs(line.code(), spans.unwrap_or(&[]));

        let old = line
            .old_line
            .map(|value| value.to_string())
            .unwrap_or_default();
        let new = line
            .new_line
            .map(|value| value.to_string())
            .unwrap_or_default();
        let clickable = line.anchor().is_some();
        let comment_count = line.anchor().map_or(0, |(side, number)| {
            self.review
                .comments
                .iter()
                .filter(|comment| {
                    self.diff.files[file_index].path == comment.path
                        && comment.side == side
                        && comment.line == number
                })
                .count()
        });

        div()
            .id(("line", line_index))
            .flex()
            .items_center()
            .min_w_full()
            .bg(if selected {
                hex(NOTE_BG)
            } else {
                hex(background)
            })
            .border_l_2()
            .border_color(if selected { rgb(ACCENT).into() } else { fg(0.) })
            .when(clickable, |element| {
                element
                    .cursor_pointer()
                    // Hovering lights the gutter rail rather than repainting the
                    // row, so addition/deletion tints survive the hover.
                    .hover(|style| style.border_color(tint(ACCENT, 0.45)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.select_line(file_index, line_index, window, cx)
                        }),
                    )
            })
            .child(
                div()
                    .w(px(GUTTER))
                    .flex_none()
                    .px(px(6.))
                    .text_right()
                    .text_color(fg(0.24))
                    .child(old),
            )
            .child(
                div()
                    .w(px(GUTTER))
                    .flex_none()
                    .px(px(6.))
                    .text_right()
                    .border_r_1()
                    .border_color(fg(0.08))
                    .text_color(fg(0.32))
                    .child(new),
            )
            .child(
                div()
                    .w(px(MARKER))
                    .flex_none()
                    .text_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hex(marker_color))
                    .child(marker),
            )
            .child(
                div()
                    .pr_3()
                    .flex_1()
                    .whitespace_nowrap()
                    .text_color(if spans.is_some() {
                        hex(CODE_TEXT)
                    } else {
                        hex(fallback)
                    })
                    .child(StyledText::new(display).with_highlights(highlights)),
            )
            .when(comment_count > 0, |element| {
                element.child(
                    div()
                        .mr_2()
                        .px(px(7.))
                        .py(px(1.))
                        .flex_none()
                        .rounded_full()
                        .bg(hex(ACCENT_FILL))
                        .border_1()
                        .border_color(fg(0.1))
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(ACCENT))
                        .child(comment_count.to_string()),
                )
            })
    }

    /// A thread: the anchored note, then its replies, each labelled with the id
    /// an agent would pass to `reviewpad reply`.
    fn render_inline_comment(
        &self,
        index: usize,
        comment: ReviewComment,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = comment.id.clone();
        let replies = comment
            .replies
            .iter()
            .enumerate()
            .map(|(position, reply)| {
                let reply_id = reply.id.clone();
                div()
                    .id(("reply", index * 64 + position))
                    .mt(px(8.))
                    .pt(px(8.))
                    .border_t_1()
                    .border_color(fg(0.08))
                    .child(
                        div()
                            .mb(px(3.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(fg(0.72))
                                            .child(reply.author.clone()),
                                    )
                                    .child(
                                        div()
                                            .font_family(MONO)
                                            .text_size(px(10.))
                                            .text_color(fg(0.34))
                                            .child(reply.id.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("remove-reply", index * 64 + position))
                                    .px_2()
                                    .text_color(fg(0.34))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(hex(DEL_MARK)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.delete_comment(reply_id.clone(), cx)
                                    }))
                                    .child("Remove"),
                            ),
                    )
                    .child(
                        div()
                            .whitespace_normal()
                            .text_color(fg(0.85))
                            .child(reply.body.clone()),
                    )
            })
            .collect::<Vec<_>>();

        let remove_id = id.clone();
        let reply_id = id.clone();

        div()
            .id(("inline-comment", index))
            .pl(px(112.))
            .pr(px(18.))
            .py(px(8.))
            .bg(fg(0.03))
            .child(
                div()
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_l_2()
                    .border_color(rgb(ACCENT))
                    .bg(hex(NOTE_BG))
                    .child(
                        div()
                            .mb_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .text_size(px(11.))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(hex(NOTE_TEXT))
                                            .child(format!(
                                                "{} · line {} · {}",
                                                comment.author,
                                                comment.line,
                                                comment.side.label()
                                            )),
                                    )
                                    .child(
                                        div()
                                            .font_family(MONO)
                                            .text_size(px(10.))
                                            .text_color(fg(0.4))
                                            .child(comment.id.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .id(("reply-to", index))
                                            .px_2()
                                            .text_color(fg(0.4))
                                            .cursor_pointer()
                                            .hover(|style| style.text_color(rgb(ACCENT)))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.start_reply(reply_id.clone(), window, cx)
                                            }))
                                            .child("Reply"),
                                    )
                                    .child(
                                        div()
                                            .id(("remove-inline", index))
                                            .px_2()
                                            .text_color(fg(0.4))
                                            .cursor_pointer()
                                            .hover(|style| style.text_color(hex(DEL_MARK)))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_comment(remove_id.clone(), cx)
                                            }))
                                            .child("Remove"),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .whitespace_normal()
                            .text_color(fg(0.9))
                            .child(comment.body),
                    )
                    .children(replies),
            )
    }

    fn render_inline_composer(
        &self,
        line: Option<&DiffLine>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let heading = match (&self.reply_to, line.and_then(DiffLine::anchor)) {
            (Some(target), _) => format!("Reply to {target}"),
            (None, Some((side, number))) => {
                format!("Comment on line {number} · {}", side.label())
            }
            (None, None) => "New review comment".to_string(),
        };
        let placeholder = if self.reply_to.is_some() {
            "Write a reply…"
        } else {
            "Write a review comment…"
        };
        let draft = if self.draft.is_empty() {
            placeholder.to_string()
        } else {
            format!("{}▍", self.draft)
        };

        div()
            .id("composer")
            .pl(px(112.))
            .pr(px(18.))
            .py(px(10.))
            .bg(fg(0.03))
            .child(
                div()
                    .p(px(12.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(tint(ACCENT, 0.3))
                    .bg(hex(NOTE_BG))
                    .child(
                        div()
                            .mb_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .child(
                                div()
                                    .text_color(hex(NOTE_TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(heading),
                            )
                            .child(
                                div()
                                    .id("cancel-inline")
                                    .px_2()
                                    .text_color(fg(0.4))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(fg(0.85)))
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_comment(cx)))
                                    .child("Cancel"),
                            ),
                    )
                    .child(
                        div()
                            .id("inline-editor")
                            .min_h(px(62.))
                            .p(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(fg(0.1))
                            .bg(scrim(0.32))
                            .whitespace_normal()
                            .text_color(fg(if self.draft.is_empty() { 0.4 } else { 0.92 }))
                            .cursor_text()
                            .child(draft),
                    )
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(fg(0.4))
                                    .child("⌘↵ add comment · Esc cancel"),
                            )
                            .child(
                                Self::button("add-inline", "Add comment", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.add_comment(cx))),
                            ),
                    ),
            )
    }

    fn render_diff_rows(
        &self,
        file_index: usize,
        file: &FileDiff,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();

        for (line_index, line) in file.lines.iter().enumerate() {
            // The path and mode are already in the header, so the raw `---`,
            // `+++` and `index` lines are dropped rather than restyled.
            if line.kind == LineKind::Header && is_noise(&line.text) {
                continue;
            }

            if matches!(line.kind, LineKind::Header | LineKind::Hunk) || line.text.starts_with('\\')
            {
                rows.push(self.render_band(line_index, line).into_any_element());
                continue;
            }

            rows.push(
                self.render_line(file_index, line_index, line, cx)
                    .into_any_element(),
            );

            if let Some((side, number)) = line.anchor() {
                for (comment_index, comment) in self.review.comments.iter().enumerate() {
                    if comment.path == file.path && comment.side == side && comment.line == number {
                        let answering = self
                            .reply_to
                            .as_deref()
                            .is_some_and(|target| thread_of(target) == comment.id);
                        rows.push(
                            self.render_inline_comment(comment_index, comment.clone(), cx)
                                .into_any_element(),
                        );
                        if answering {
                            rows.push(
                                self.render_inline_composer(Some(line), cx)
                                    .into_any_element(),
                            );
                        }
                    }
                }
            }

            if self
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.file == file_index && anchor.line == line_index)
            {
                rows.push(
                    self.render_inline_composer(Some(line), cx)
                        .into_any_element(),
                );
            }
        }

        rows
    }

    /// The changed-file list, grouped by directory the way tgip nests terminal
    /// tabs under their folder, with a rail standing in for its tree spine.
    fn render_file_groups(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (index, file) in self.diff.files.iter().enumerate() {
            let parent = file.path.rsplit_once('/').map_or("", |(parent, _)| parent);
            match groups.iter_mut().find(|(key, _)| *key == parent) {
                Some((_, files)) => files.push(index),
                None => groups.push((parent, vec![index])),
            }
        }

        groups
            .into_iter()
            .map(|(parent, files)| {
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(10.))
                            .pt(px(8.))
                            .pb(px(3.))
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg(0.4))
                            .child(if parent.is_empty() {
                                "./".to_string()
                            } else {
                                format!("{parent}/")
                            }),
                    )
                    .child(
                        div()
                            .ml(px(9.))
                            .pl(px(6.))
                            .border_l_1()
                            .border_color(fg(0.09))
                            .flex()
                            .flex_col()
                            .children(
                                files.into_iter().map(|index| {
                                    self.render_file(index, &self.diff.files[index], cx)
                                }),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn empty_state(title: &'static str, message: &'static str) -> gpui::Div {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .items_center()
            .justify_center()
            .child(div().text_size(px(22.)).text_color(fg(0.34)).child("⌕"))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg(0.82))
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(fg(0.5))
                    .child(message),
            )
    }
}

/// Diff plumbing that carries no information the header doesn't already show.
fn is_noise(text: &str) -> bool {
    text.starts_with("--- ") || text.starts_with("+++ ") || text.starts_with("index ")
}

/// Render tabs as aligned spaces and carry the syntax spans across, since a raw
/// `\t` has no width the text system can lay out consistently.
fn expand_tabs(code: &str, spans: &[Span]) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
    if !code.contains('\t') {
        return (SharedString::from(code.to_string()), styles(spans, |at| at));
    }

    let mut text = String::with_capacity(code.len());
    // Maps every byte offset in `code` to its offset in `text`.
    let mut moved = Vec::with_capacity(code.len() + 1);
    let mut column = 0;

    for (offset, character) in code.char_indices() {
        while moved.len() <= offset {
            moved.push(text.len());
        }
        if character == '\t' {
            let width = TAB_WIDTH - (column % TAB_WIDTH);
            for _ in 0..width {
                text.push(' ');
            }
            column += width;
        } else {
            text.push(character);
            column += 1;
        }
    }
    while moved.len() <= code.len() {
        moved.push(text.len());
    }

    let highlights = styles(spans, |at| moved[at.min(moved.len() - 1)]);
    (SharedString::from(text), highlights)
}

/// Turn scope indices into gpui text runs, remapping offsets on the way.
fn styles(spans: &[Span], remap: impl Fn(usize) -> usize) -> Vec<(Range<usize>, HighlightStyle)> {
    spans
        .iter()
        .map(|(range, scope)| {
            let (color, italic) = SCOPE_COLORS[*scope];
            (
                remap(range.start)..remap(range.end),
                HighlightStyle {
                    color: Some(rgb(color).into()),
                    font_style: italic.then_some(FontStyle::Italic),
                    ..Default::default()
                },
            )
        })
        .collect()
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root_name = self
            .repository
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repository.root.display().to_string());
        let root_path = self.repository.root.display().to_string();
        let selected_file = self.diff.files.get(self.selected_file);
        let selected_path = selected_file
            .map(|file| file.path.clone())
            .unwrap_or_else(|| "Working tree is clean".into());
        let selected_name = selected_path
            .rsplit_once('/')
            .map_or(selected_path.as_str(), |(_, name)| name)
            .to_string();
        let selected_stats = selected_file.map(|file| (file.additions, file.deletions));
        let language = self.highlight.grammar.map(Grammar::label);
        let file_count = self.diff.files.len();
        let note_count = self.review.len();
        let change_summary = format!(
            "{file_count} changed file{} · {note_count} review note{}",
            if file_count == 1 { "" } else { "s" },
            if note_count == 1 { "" } else { "s" }
        );
        let diff_rows = selected_file
            .map(|file| self.render_diff_rows(self.selected_file, file, cx))
            .unwrap_or_default();
        let file_groups = self.render_file_groups(cx);

        // Sidebar — part of the window itself, painted straight onto the glass
        // with no panel fill or divider, exactly like tgip's.
        let sidebar = div()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            // Room for the traffic lights, which sit at the sidebar's own inset.
            .child(div().h(px(TITLEBAR_INSET)).flex_none())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(12.))
                    .pb(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(ACCENT))
                                    .child("⑂"),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(fg(0.92))
                                    .child(root_name),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(10.))
                            .text_color(fg(0.34))
                            .child(root_path),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(fg(0.5))
                            .child(change_summary),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(14.))
                    .py(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(fg(0.76))
                            .child("Changed Files"),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(fg(0.44))
                            .child(file_count.to_string()),
                    ),
            )
            .child(
                div()
                    .id("file-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(4.))
                    .pb(px(6.))
                    .children(file_groups),
            )
            .when_some(self.update.clone(), |element, version| {
                let command = update::Install::detect().upgrade_hint();
                element.child(
                    div()
                        .id("update-banner")
                        .mx(px(8.))
                        .mb(px(2.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .bg(hex(ACCENT_FILL))
                        .border_1()
                        .border_color(tint(ACCENT, 0.22))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .cursor_pointer()
                        .hover(|style| style.bg(tint(0x6b3f14, 0.92)))
                        .on_click(cx.listener(|this, _, _, cx| this.copy_update_command(cx)))
                        .child(
                            div()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(ACCENT))
                                .child(format!("ReviewPad {version} available")),
                        )
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(px(10.))
                                .text_color(fg(0.45))
                                .child(command),
                        ),
                )
            })
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(8.))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg(0.3))
                            .child(format!("{note_count} ⌸")),
                    )
                    .child(div().flex_1())
                    .child(
                        Self::button("copy", "Copy Markdown", false)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_markdown(cx))),
                    )
                    .child(
                        Self::button("finish", "Finish", true)
                            .on_click(cx.listener(|this, _, window, cx| this.finish(window, cx))),
                    ),
            );

        // The inset card: rounded to the window radius minus the outer padding,
        // hairline border, soft drop shadow — tgip's content container.
        let card = div()
            .flex_1()
            .min_w_0()
            .ml(px(OUTER_PADDING))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(CARD_RADIUS))
            .bg(scrim(0.28))
            .border_1()
            .border_color(fg(0.14))
            .shadow(vec![BoxShadow {
                color: scrim(0.32),
                offset: point(px(0.), px(10.)),
                blur_radius: px(20.),
                spread_radius: px(0.),
            }])
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(12.))
                    .px(px(18.))
                    .py(px(14.))
                    .bg(fg(0.03))
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(fg(0.92))
                                    .child(selected_name),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(MONO)
                                    .text_size(px(11.))
                                    .text_color(fg(0.46))
                                    .child(selected_path),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .when_some(self.status.clone(), |element, status| {
                                element.child(
                                    div().text_size(px(11.)).text_color(fg(0.5)).child(status),
                                )
                            })
                            .when_some(language, |element, language| {
                                element.child(Self::capsule(language, fg(0.5), fg(0.06)))
                            })
                            .when_some(selected_stats, |element, (additions, deletions)| {
                                element.child(
                                    div()
                                        .flex()
                                        .gap(px(6.))
                                        .px(px(9.))
                                        .py(px(3.))
                                        .rounded_full()
                                        .bg(fg(0.07))
                                        .font_family(MONO)
                                        .text_size(px(10.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(
                                            div()
                                                .text_color(hex(ADD_MARK))
                                                .child(format!("+{additions}")),
                                        )
                                        .child(
                                            div()
                                                .text_color(hex(DEL_MARK))
                                                .child(format!("−{deletions}")),
                                        ),
                                )
                            }),
                    ),
            )
            .child(Self::divider(0.1))
            .child(
                div()
                    .id("diff-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .font_family(MONO)
                    .text_size(px(13.))
                    .line_height(px(22.))
                    .children(diff_rows)
                    .when(self.diff.files.is_empty(), |element| {
                        element.child(Self::empty_state(
                            "Working tree is clean",
                            "No tracked, staged, or untracked changes are waiting for review.",
                        ))
                    }),
            );

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key))
            .size_full()
            .flex()
            .p(px(OUTER_PADDING))
            // Stands in for tgip's dark `hudWindow` material: gpui's window blur
            // is colorless, so the tint is painted here. The sidebar carries no
            // fill of its own, so this alpha is what sets how translucent it
            // reads — raise it to make the sidebar more solid.
            .bg(scrim(0.6))
            .text_color(fg(0.9))
            .text_sm()
            .child(sidebar)
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_expand_to_the_next_stop_and_carry_spans() {
        let (text, highlights) = expand_tabs("\tlet x", &[(1..4, 9)]);
        assert_eq!(text.as_ref(), "    let x");
        assert_eq!(highlights[0].0, 4..7);
    }

    #[test]
    fn untabbed_code_is_passed_through() {
        let (text, highlights) = expand_tabs("let x", &[(0..3, 9)]);
        assert_eq!(text.as_ref(), "let x");
        assert_eq!(highlights[0].0, 0..3);
    }

    #[test]
    fn diff_plumbing_is_treated_as_noise() {
        assert!(is_noise("index 1a2b3c..4d5e6f 100644"));
        assert!(is_noise("+++ b/src/app.rs"));
        assert!(!is_noise("new file mode 100644"));
    }
}
