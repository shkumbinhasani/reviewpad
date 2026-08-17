use anyhow::Result;
use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, ClipboardItem, Context, CursorStyle, Entity,
    FocusHandle, Focusable, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, KeyDownEvent,
    ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, PathPromptOptions,
    Pixels, Point, Render, SharedString, StatefulInteractiveElement, StyledText, Window,
    WindowBounds, WindowOptions, canvas, div, img, list, point, prelude::*, px, relative, rgb,
    rgba, size, svg, uniform_list,
};
use std::{ops::Range, path::PathBuf, time::Duration};

use core_video::pixel_buffer::CVPixelBuffer;
use gpui::surface;
use reviewpad::{
    avatar,
    field::{self, FieldStyle, TextField},
    git::{Base, DiffLine, DiffSet, FileDiff, LineKind, Repository},
    media::{self, Medium, Probe},
    player::Player,
    review::{Anchor, OrderedF64, Review, ReviewComment, Spot, thread_of},
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
/// Minimum width of a line-number gutter, widened per file by `gutter_width`.
const GUTTER: f32 = 44.;
/// Width of the +/− column, tgip's marker column narrowed for the gutters.
const MARKER: f32 = 24.;
/// Edge of an author's avatar tile.
const AVATAR: f32 = 24.;
/// Height of a sidebar row. Fixed, so the list can virtualize.
const SIDEBAR_ROW: f32 = 30.;
/// Width of the notes column beside a render.
const MEDIA_NOTES: f32 = 300.;
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

/// Sidebar text. It used to run through a dozen alphas from 0.30 up, and the
/// faint end was unreadable over a blurred desktop — one weight throughout.
fn ink() -> Hsla {
    fg(0.9)
}

fn hex(value: u32) -> Hsla {
    rgba(value).into()
}

fn tint(value: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = rgb(value).into();
    color.a = alpha;
    color
}

/// Brand marks, compiled into the binary. gpui resolves `svg()` paths through
/// the app's asset source, and embedding keeps the `.app` bundle a single file.
struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
            "icons/claude.svg" => include_bytes!("../assets/icons/claude.svg"),
            "icons/openai.svg" => include_bytes!("../assets/icons/openai.svg"),
            "icons/gemini.svg" => include_bytes!("../assets/icons/gemini.svg"),
            "icons/copilot.svg" => include_bytes!("../assets/icons/copilot.svg"),
            "icons/cursor.svg" => include_bytes!("../assets/icons/cursor.svg"),
            _ => return Ok(None),
        };
        Ok(Some(std::borrow::Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![
            "icons/claude.svg".into(),
            "icons/openai.svg".into(),
            "icons/gemini.svg".into(),
            "icons/copilot.svg".into(),
            "icons/cursor.svg".into(),
        ])
    }
}

/// Native window dragging.
///
/// gpui 0.2.2 leaves `Window::start_window_move` as an empty default on macOS,
/// so this goes to AppKit directly. `performWindowDragWithEvent:` is the call a
/// custom titlebar uses, and it is what tgip's draggable background does.
mod window_drag {
    #[cfg(target_os = "macos")]
    pub(super) fn start() {
        use objc::{msg_send, runtime::Object, sel, sel_impl};

        unsafe {
            let app: *mut Object = msg_send![objc::class!(NSApplication), sharedApplication];
            if app.is_null() {
                return;
            }
            let event: *mut Object = msg_send![app, currentEvent];
            if event.is_null() {
                return;
            }
            let window: *mut Object = msg_send![event, window];
            if window.is_null() {
                return;
            }
            let _: () = msg_send![window, performWindowDragWithEvent: event];
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn start() {}
}

/// The Dock icon.
///
/// macOS reads it from the `.app` bundle around the executable, and neither way
/// of reaching the panel from a terminal has one: PATH points at a symlink into
/// the bundle, so AppKit resolves the symlink's own directory and finds no
/// bundle, and the standalone tarball ships the bare binary with no bundle to
/// find. An MCP client inherits the same fate — it launches this binary by the
/// path it was started with. Handing AppKit the icon covers every launch,
/// bundled or not.
mod app_icon {
    #[cfg(target_os = "macos")]
    pub(super) fn set() {
        use objc::{msg_send, runtime::Object, sel, sel_impl};

        /// The same `.icns` the bundle carries, so the icon in the Dock cannot
        /// disagree with the one in Finder.
        const ICON: &[u8] = include_bytes!("../assets/ReviewPad.icns");

        unsafe {
            // Autoreleased, and drained by the run loop this is called from.
            let data: *mut Object = msg_send![
                objc::class!(NSData),
                dataWithBytes: ICON.as_ptr()
                length: ICON.len()
            ];
            let image: *mut Object = msg_send![objc::class!(NSImage), alloc];
            // An `.icns` carries every size, so the Dock, ⌘-Tab and the app
            // switcher each pick the rendition they want.
            let image: *mut Object = msg_send![image, initWithData: data];
            if image.is_null() {
                return;
            }
            let app: *mut Object = msg_send![objc::class!(NSApplication), sharedApplication];
            if app.is_null() {
                return;
            }
            // AppKit takes its own copy. This one is never released: a single
            // image, set once at startup, is not worth the lifetime question.
            let _: () = msg_send![app, setApplicationIconImage: image];
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn set() {}
}

/// Mark a region as window chrome: dragging it moves the window.
fn draggable(element: gpui::Div) -> gpui::Div {
    element.on_mouse_down(MouseButton::Left, |_, _, _| window_drag::start())
}

/// Opt an element out of the window drag around it. Mouse events bubble in
/// gpui, so anything clickable sitting on draggable chrome has to say so, or
/// pressing it starts dragging the window instead. Only the press is stopped —
/// `on_click` fires on release and still runs.
fn holds_the_mouse(element: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    element.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// The diff row the composer is aimed at: indices into the rendered file,
/// not a review anchor.
#[derive(Clone)]
struct Target {
    file: usize,
    line: usize,
}

/// A note's marker on the picture: where it points, and the frame it points at.
struct Marker {
    /// Position in the review, used when there is no frame to show.
    number: usize,
    id: String,
    spot: Spot,
    seconds: Option<f64>,
    frame: Option<u32>,
}

/// Which divider is being dragged, while it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dragging {
    Sidebar,
    Notes,
}

/// What removing an attachment does.
type Detach = Box<dyn Fn(&mut ReviewView, &mut Window, &mut Context<ReviewView>)>;

/// One row of the diff, as a description rather than an element.
///
/// The pane used to build every row of the selected file each frame — 7,086 of
/// them for a transcript in a real project. Planning them costs a vector of
/// small enums; only the rows on screen are ever built.
#[derive(Clone)]
enum DiffRow {
    /// A hunk header or a metadata line, drawn as a full-width band.
    Band(usize),
    /// A line of code.
    Code(usize),
    /// A thread hanging off the line above.
    Thread(usize),
    /// The composer, under whichever row it is answering.
    Composer(usize),
}

/// One row of the sidebar. Flat and uniform so the list can skip straight to
/// the rows on screen instead of building all of them.
#[derive(Clone)]
enum SidebarRow {
    Folder(String),
    File(usize),
}

/// What a note is pinned to, carried on the message as an attachment.
#[derive(Clone, Default)]
struct Attachment {
    /// The moment, as a timecode and the frame it lands on.
    time: Option<String>,
    /// The place, in the media's own pixels.
    place: Option<String>,
    /// The note to jump to when either is clicked.
    jump: Option<String>,
}

impl Attachment {
    fn is_empty(&self) -> bool {
        self.time.is_none() && self.place.is_none()
    }
}

/// One message to draw inside a thread — the root note or any of its replies.
struct Message<'a> {
    /// Stable element key, distinct across every message on screen.
    key: usize,
    /// Hover group of the thread this belongs to.
    group: &'a str,
    author: &'a str,
    id: &'a str,
    /// Anchor line, shown on the root only.
    meta: Option<String>,
    /// Where it is pinned, shown on the root only.
    attachment: Option<Attachment>,
    body: String,
}

pub fn run(repository: Repository, base: Base, diff: DiffSet, print_on_finish: bool) -> Result<()> {
    let mut review = Review::open(&repository)?;
    review.base = Some(base.label());

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            app_icon::set();
            field::bind_keys(cx);
            open_review_window(cx, repository, base, diff, review, print_on_finish);
        });
    Ok(())
}

/// Launch the desktop app without a terminal working directory and let the user
/// choose a Git repository with the native directory picker.
pub fn pick_and_run() -> Result<()> {
    Application::new().with_assets(Assets).run(|cx: &mut App| {
        app_icon::set();
        field::bind_keys(cx);
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
                        open_review_window(cx, repository, Base::WorkingTree, diff, review, false);
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
    base: Base,
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
                base,
                diff,
                review,
                selected_file: 0,
                anchor: None,
                reply_to: None,
                draft: field::text_field(
                    "Write a review comment…",
                    FieldStyle {
                        text: fg(0.92),
                        placeholder: fg(0.38),
                        selection: tint(ACCENT, 0.28),
                        caret: rgb(ACCENT).into(),
                        font_size: px(13.),
                        line_height: px(19.),
                    },
                    cx,
                ),
                focus: cx.focus_handle(),
                print_on_finish,
                status: None,
                syntax: SyntaxIndex::new(),
                highlight: DiffHighlight::default(),
                medium: Medium::Text,
                probe: None,
                time: 0.,
                player: None,
                frame: None,
                _pump: None,
                noting_media: false,
                noting_time: true,
                media_size: None,
                sidebar_width: SIDEBAR_WIDTH,
                notes_width: MEDIA_NOTES,
                dragging: None,
                pending_spot: None,
                media_bounds: None,
                stage_bounds: None,
                timeline_bounds: None,
                sidebar_rows: Vec::new(),
                diff_rows: Vec::new(),
                diff_list: ListState::new(0, ListAlignment::Top, px(600.)),
                portrait: None,
                _portrait_task: None,
                update: None,
                update_task: None,
            };
            view.plan_sidebar();
            view.plan_diff();
            view.refresh_highlight();
            view.refresh_medium(cx);
            view.check_for_update(cx);
            view.load_portrait(cx);
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
    /// What is being reviewed — uncommitted work, or a branch range.
    base: Base,
    diff: DiffSet,
    review: Review,
    selected_file: usize,
    anchor: Option<Target>,
    /// Id of the thread the composer is answering, when it is not anchored to a
    /// diff line.
    reply_to: Option<String>,
    /// The composer. A real field, so it selects, moves and composes like any
    /// other text input rather than accumulating keystrokes.
    draft: Entity<TextField>,
    focus: FocusHandle,
    print_on_finish: bool,
    status: Option<String>,
    syntax: SyntaxIndex,
    highlight: DiffHighlight,
    /// What the selected file is, and where we are in it when it is a video.
    medium: Medium,
    probe: Option<Probe>,
    /// Scrub position, in seconds.
    time: f64,
    /// The clip, decoded by AVFoundation. Frames arrive as buffers gpui binds
    /// directly, so there is no cache and nothing on disk.
    player: Option<Player>,
    /// The most recent frame, held so the pane keeps drawing between frames.
    frame: Option<CVPixelBuffer>,
    /// Pulls frames and follows the player's clock.
    _pump: Option<gpui::Task<()>>,
    /// Whether a note is being written about the media on screen, and whether
    /// the moment is still attached to it.
    noting_media: bool,
    noting_time: bool,
    /// Natural size of the media, for saying a place in pixels rather than
    /// percentages.
    media_size: Option<(u32, u32)>,
    /// Widths of the two panels, and which divider is under the pointer.
    sidebar_width: f32,
    notes_width: f32,
    dragging: Option<Dragging>,
    /// A place the pointer put down, if one was — the pin is an attachment to
    /// the note, not a requirement for making one.
    pending_spot: Option<Spot>,
    /// Painted bounds of the image and the scrubber, which is what turns a
    /// click into a normalized spot or a time.
    media_bounds: Option<Bounds<Pixels>>,
    /// The area the media is drawn into, which the video rect is fitted inside.
    stage_bounds: Option<Bounds<Pixels>>,
    /// The sidebar's rows, rebuilt when the file list changes rather than on
    /// every frame — a playing video repaints the window 15 to 60 times a
    /// second, and this list runs to hundreds of rows in a real project.
    sidebar_rows: Vec<SidebarRow>,
    /// The selected file's rows, and the list that scrolls them.
    diff_rows: Vec<DiffRow>,
    diff_list: ListState,
    timeline_bounds: Option<Bounds<Pixels>>,
    /// The local user's Gravatar, once the background lookup has answered.
    portrait: Option<std::sync::Arc<gpui::Image>>,
    _portrait_task: Option<gpui::Task<()>>,
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
        self.clear_draft(cx);
        self.refresh_highlight();
        self.refresh_medium(cx);
        self.plan_diff();
        cx.notify();
    }

    /// Look up the local user's Gravatar in the background. Everyone already
    /// has a monogram, so this only ever upgrades what is on screen — a missing
    /// account, a blocked network or an opt-out all just leave it alone.
    fn load_portrait(&mut self, cx: &mut Context<Self>) {
        let Some(email) = self.repository.user_email() else {
            return;
        };
        self._portrait_task = Some(cx.spawn(async move |view, cx| {
            let Some(bytes) = cx
                .background_spawn(async move { avatar::fetch_gravatar(&email) })
                .await
            else {
                return;
            };
            let image = std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes));
            let _ = view.update(cx, |view, cx| {
                view.portrait = Some(image);
                cx.notify();
            });
        }));
    }

    /// A comment author's chip: their mark on a brand-colored tile, or a
    /// monogram, with the user's own portrait layered over it once one loads.
    fn render_avatar(&self, author: &str) -> gpui::Div {
        let identity = avatar::identity(author);
        let portrait = (author == AUTHOR).then(|| self.portrait.clone()).flatten();
        // gpui paints an SVG as a mask tinted with the *element's own* text
        // color — it does not inherit one — so the ink is set on the mark
        // itself rather than on the tile around it.
        let ink = if identity.is_light() {
            scrim(0.8)
        } else {
            fg(0.96)
        };

        div()
            .flex_none()
            .relative()
            .size(px(AVATAR))
            .rounded(px(6.))
            .overflow_hidden()
            .bg(tint(identity.color, 0.95))
            // Lit the way the buttons are, so an author's tile belongs to the
            // same surface rather than sitting flat on it.
            .border_1()
            .border_color(fg(0.14))
            .shadow(vec![BoxShadow {
                color: scrim(0.3),
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            }])
            .flex()
            .items_center()
            .justify_center()
            .map(|element| match identity.icon {
                Some(icon) => element.child(
                    svg()
                        .path(icon)
                        .size(px(AVATAR - 9.))
                        .flex_none()
                        .text_color(ink),
                ),
                None => element.child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(AVATAR))
                        .font_weight(FontWeight::BOLD)
                        .text_color(ink)
                        .child(identity.label.clone()),
                ),
            })
            .when_some(portrait, |element, image| {
                element.child(
                    img(image)
                        .absolute()
                        .inset_0()
                        .size(px(AVATAR))
                        .rounded(px(6.)),
                )
            })
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(6.))
                    .bg(gpui::linear_gradient(
                        180.,
                        gpui::linear_color_stop(fg(0.2), 0.),
                        gpui::linear_color_stop(fg(0.), 0.62),
                    )),
            )
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

    /// Flatten the changed files into sidebar rows, grouped by directory.
    fn plan_sidebar(&mut self) {
        let mut rows = Vec::new();
        let mut current: Option<&str> = None;

        for (index, file) in self.diff.files.iter().enumerate() {
            let parent = file.path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if current != Some(parent) {
                rows.push(SidebarRow::Folder(if parent.is_empty() {
                    "./".to_string()
                } else {
                    format!("{parent}/")
                }));
                current = Some(parent);
            }
            rows.push(SidebarRow::File(index));
        }

        self.sidebar_rows = rows;
    }

    /// Work out what the selected file is and, for a video, how long it runs.
    /// A rendered file reaches the sidebar like any other change; this decides
    /// whether it is read as a diff or looked at.
    fn refresh_medium(&mut self, cx: &mut Context<Self>) {
        self.pending_spot = None;
        self.noting_media = false;
        self.noting_time = true;
        self.media_size = None;
        self.time = 0.;
        self.probe = None;
        self.player = None;
        self.frame = None;
        self._pump = None;

        let Some(file) = self.diff.files.get(self.selected_file) else {
            self.medium = Medium::Text;
            return;
        };
        self.medium = Medium::of(&file.path);
        if self.medium == Medium::Image {
            // Read from the header; nothing is decoded.
            self.media_size = image::image_dimensions(self.repository.root.join(&file.path)).ok();
        }
        if self.medium != Medium::Video {
            return;
        }

        let video = self.repository.root.join(&file.path);
        match Player::open(&video) {
            Ok(player) => {
                self.player = Some(player);
                self.start_pump(cx);
            }
            Err(error) => {
                self.status = Some(format!("Could not open the clip: {error}"));
            }
        }
    }

    /// Follow the player: pull each new frame and read its clock.
    ///
    /// AVFoundation owns the timing, so this only asks what the current frame
    /// and time are — it never advances anything itself. That is what keeps
    /// picture, sound and the scrubber in agreement.
    fn start_pump(&mut self, cx: &mut Context<Self>) {
        let step = Duration::from_millis(8);
        self._pump = Some(cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(step).await;
                let alive = view.update(cx, |view, cx| {
                    let Some(player) = view.player.as_mut() else {
                        return false;
                    };
                    // Duration and frame rate only become readable once the
                    // item has loaded, so they are picked up here.
                    if view.probe.is_none() && player.is_ready() {
                        let duration = player.duration();
                        let fps = player.fps();
                        if duration > 0. {
                            view.probe = Some(Probe { duration, fps });
                        }
                    }

                    let time = player.current_time();
                    let frame = player.frame();
                    let arrived = frame.is_some();
                    let playing = player.is_playing();
                    let finished = playing && player.is_finished();
                    if finished {
                        player.pause();
                    }

                    if arrived {
                        if let Some(buffer) = frame.as_ref() {
                            view.media_size =
                                Some((buffer.get_width() as u32, buffer.get_height() as u32));
                        }
                        view.frame = frame;
                    }
                    // Only follow the clock while it is running; otherwise the
                    // scrub position is ours to set.
                    if playing {
                        view.time = time;
                    }
                    if arrived || playing || finished {
                        cx.notify();
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Move to a moment. The player seeks precisely, so the frame that appears
    /// is the frame the comment will name.
    fn seek(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let duration = self.probe.map(|probe| probe.duration).unwrap_or(0.);
        self.time = seconds.clamp(0., duration);
        if let Some(player) = self.player.as_ref() {
            player.seek(self.time);
        }
        cx.notify();
    }

    fn is_playing(&self) -> bool {
        self.player
            .as_ref()
            .is_some_and(|player| player.is_playing())
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        if player.is_playing() {
            player.pause();
        } else {
            // Replay from the start once it has run out.
            if player.is_finished() {
                player.seek(0.);
            }
            player.play();
        }
        cx.notify();
    }

    /// Reparse the selected file so its hunks can be painted with real syntax
    /// colors. Only the file on screen is parsed, and only when it changes.
    fn refresh_highlight(&mut self) {
        self.highlight = match self.diff.files.get(self.selected_file) {
            Some(file) => DiffHighlight::load(&self.repository, &self.base, file, &mut self.syntax),
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
        self.anchor = Some(Target { file, line });
        self.reply_to = None;
        self.status = None;
        self.plan_diff();
        self.draft.update(cx, |draft, cx| {
            draft.set_placeholder("Write a review comment…", cx);
        });
        self.draft.read(cx).focus(window);
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

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let composing = self.anchor.is_some() || self.reply_to.is_some();

        match key {
            // Outside the composer the arrows walk the file list, so the diff
            // is navigable without reaching for the mouse. Inside it, the field
            // has already consumed them.
            "up" if !composing => self.step_file(-1, cx),
            "down" if !composing => self.step_file(1, cx),
            // Submit and dismiss stay with the view; every other keystroke is
            // the field's business.
            "enter" if composing && (modifiers.platform || modifiers.control) => {
                self.add_comment(cx)
            }
            "space" if !composing && self.medium == Medium::Video => self.toggle_play(cx),
            "escape" => {
                self.cancel_comment(cx);
                self.reclaim_focus(window);
            }
            _ => {}
        }
    }

    fn add_comment(&mut self, cx: &mut Context<Self>) {
        let body = self.draft.read(cx).text().trim().to_string();
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

        // A place on an image or a moment in a video, put down by pointer.
        if let Some(spot) = self.pending_spot {
            let Some(file) = self.diff.files.get(self.selected_file) else {
                return;
            };
            let path = file.path.clone();
            let anchor = match self.medium {
                Medium::Video => Anchor::Time {
                    seconds: OrderedF64(self.time),
                    frame: self.probe.map(|probe| probe.frame_at(self.time)),
                    spot: Some(spot),
                },
                _ => Anchor::Spot { spot },
            };
            self.review
                .add_comment(path, anchor, AUTHOR, body, String::new());
            self.pending_spot = None;
            self.finish_composing("Comment saved", cx);
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
        self.review.add_comment(
            path,
            Anchor::Line { side, line: number },
            AUTHOR,
            body,
            context,
        );
        self.finish_composing("Comment saved", cx);
    }

    /// Empty the composer.
    fn clear_draft(&mut self, cx: &mut Context<Self>) {
        self.draft.update(cx, |draft, cx| draft.clear(cx));
    }

    /// Clear the composer and persist, reporting whichever outcome the save had.
    fn finish_composing(&mut self, saved: &str, cx: &mut Context<Self>) {
        self.clear_draft(cx);
        self.anchor = None;
        self.reply_to = None;
        self.status = match self.review.save(&self.repository.review_path()) {
            Ok(()) => Some(saved.into()),
            Err(error) => Some(format!("Could not save: {error}")),
        };
        self.plan_diff();
        cx.notify();
    }

    fn delete_comment(&mut self, id: String, cx: &mut Context<Self>) {
        if self.review.remove(&id).is_err() {
            return;
        }
        if self.reply_to.as_deref() == Some(id.as_str()) {
            self.reply_to = None;
            self.clear_draft(cx);
        }
        self.status = match self.review.save(&self.repository.review_path()) {
            Ok(()) => Some("Comment removed".into()),
            Err(error) => Some(format!("Could not save: {error}")),
        };
        self.plan_diff();
        cx.notify();
    }

    /// Aim the composer at a thread instead of a diff line.
    fn start_reply(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_to = Some(id);
        self.anchor = None;
        self.clear_draft(cx);
        self.status = None;
        self.plan_diff();
        self.draft.update(cx, |draft, cx| {
            draft.set_placeholder("Write a reply…", cx);
        });
        self.draft.read(cx).focus(window);
        cx.notify();
    }

    fn cancel_comment(&mut self, cx: &mut Context<Self>) {
        self.clear_draft(cx);
        self.anchor = None;
        self.reply_to = None;
        self.pending_spot = None;
        self.noting_media = false;
        self.status = None;
        self.plan_diff();
        cx.notify();
    }

    /// Return focus to the view so the file-list shortcuts work again.
    fn reclaim_focus(&self, window: &mut Window) {
        window.focus(&self.focus);
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

    /// A button with some weight to it.
    ///
    /// Flat fills read as labels rather than controls, so this layers what a
    /// physical key does to light: a gradient sheen down the face that lifts on
    /// hover, a hairline ring, and a soft shadow underneath to sit it above the
    /// glass. The sheen is what makes the top edge read as lit — drawing that
    /// edge as its own line looks like a line, which is what it is.
    fn button(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        primary: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let _ = cx;
        let (face, ring, text) = if primary {
            (hex(ACCENT_FILL), tint(ACCENT, 0.45), tint(ACCENT, 0.98))
        } else {
            (fg(0.08), fg(0.14), ink())
        };

        holds_the_mouse(div().id(id))
            .group("button")
            .relative()
            .overflow_hidden()
            .flex_none()
            .h(px(30.))
            .px(px(12.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(face)
            .border_1()
            .border_color(ring)
            .shadow(vec![BoxShadow {
                color: scrim(0.28),
                offset: point(px(0.), px(1.)),
                blur_radius: px(3.),
                spread_radius: px(0.),
            }])
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .cursor_pointer()
            // The sheen, brightening under the pointer.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_full()
                    .bg(gpui::linear_gradient(
                        180.,
                        gpui::linear_color_stop(fg(0.22), 0.),
                        gpui::linear_color_stop(fg(0.), 0.62),
                    ))
                    .opacity(0.5)
                    .group_hover("button", |style| style.opacity(1.)),
            )
            .child(label.into())
            .active(|style| style.opacity(0.82))
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

        holds_the_mouse(div().id(("file", index)))
            .group("file-row")
            .relative()
            .overflow_hidden()
            .w_full()
            // Fixed box: every state has the same height, padding and border,
            // so nothing about the row moves as the pointer crosses it.
            .h(px(26.))
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(9.))
            .rounded(px(8.))
            // The selected row is lit the way the secondary buttons are —
            // neutral, not the accent, which was far too loud for something a
            // whole column of rows can be. Its ring, sheen and shadow are what
            // separate it from a hover, rather than a brighter colour.
            .bg(if selected { fg(0.1) } else { fg(0.) })
            .border_1()
            .border_color(if selected { fg(0.16) } else { fg(0.) })
            .when(selected, |element| {
                element.shadow(vec![BoxShadow {
                    color: scrim(0.28),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(3.),
                    spread_radius: px(0.),
                }])
            })
            .cursor_pointer()
            .hover(|style| if selected { style } else { style.bg(fg(0.06)) })
            .on_click(cx.listener(move |this, _, _, cx| this.select_file(index, cx)))
            // The face, under everything else so the name stays legible.
            .when(selected, |element| {
                element.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(8.))
                        .bg(gpui::linear_gradient(
                            180.,
                            gpui::linear_color_stop(fg(0.2), 0.),
                            gpui::linear_color_stop(fg(0.), 0.62),
                        ))
                        .opacity(0.6)
                        .group_hover("file-row", |style| style.opacity(1.)),
                )
            })
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
                    // Constant weight: bolding on selection reflows the name
                    // and the row changes size under the pointer. The colour
                    // and the ring carry the state instead.
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ink())
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
            .font_family(MONO)
            .text_size(px(13.))
            .line_height(px(22.))
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
        gutter: Pixels,
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
                        && comment.anchor.line() == Some((side, number))
                })
                .count()
        });

        div()
            .id(("line", line_index))
            .flex()
            .items_center()
            .min_w_full()
            .font_family(MONO)
            .text_size(px(13.))
            .line_height(px(22.))
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
                    .w(gutter)
                    .flex_none()
                    .px(px(6.))
                    .text_right()
                    .whitespace_nowrap()
                    .text_color(fg(0.24))
                    .child(old),
            )
            .child(
                div()
                    .w(gutter)
                    .flex_none()
                    .px(px(6.))
                    .text_right()
                    .whitespace_nowrap()
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
                    .whitespace_nowrap()
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

    /// One message in a thread: who wrote it, its id, and the prose. Replies
    /// reuse this so a thread reads as a conversation rather than a stack of
    /// differently-shaped boxes.
    fn render_message(&self, message: Message<'_>, cx: &mut Context<Self>) -> gpui::Div {
        let Message {
            key,
            group,
            author,
            id,
            meta,
            attachment,
            body,
        } = message;
        let remove_id = id.to_string();
        let reply_id = id.to_string();

        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(self.render_avatar(author))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(fg(0.88))
                            .child(author.to_string()),
                    )
                    .when_some(meta, |element, meta| {
                        element.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(fg(0.4))
                                .child(meta),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(fg(0.07))
                            .font_family(MONO)
                            .text_size(px(10.))
                            .text_color(fg(0.45))
                            .child(id.to_string()),
                    )
                    .child(div().flex_1())
                    // Actions stay out of the way until the thread is hovered,
                    // so a diff full of notes is not also full of buttons.
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .text_size(px(11.))
                            .opacity(0.)
                            .group_hover(group.to_string(), |style| style.opacity(1.))
                            .child(
                                div()
                                    .id(("reply-to", key))
                                    .px(px(8.))
                                    .py(px(2.))
                                    .rounded_full()
                                    .text_color(ink())
                                    .cursor_pointer()
                                    .hover(|style| style.bg(fg(0.08)).text_color(tint(ACCENT, 1.)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_reply(reply_id.clone(), window, cx)
                                    }))
                                    .child("Reply"),
                            )
                            .child(
                                div()
                                    .id(("remove-message", key))
                                    .px(px(8.))
                                    .py(px(2.))
                                    .rounded_full()
                                    .text_color(ink())
                                    .cursor_pointer()
                                    .hover(|style| style.bg(fg(0.08)).text_color(hex(DEL_MARK)))
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
                    .text_size(px(13.))
                    .line_height(px(19.))
                    .text_color(fg(0.84))
                    .child(body),
            )
            .when_some(
                attachment.filter(|attachment| !attachment.is_empty()),
                |element, attachment| {
                    // The moment and the place are separate tags, each its own
                    // way back to what the note points at.
                    let (time, place) = (attachment.time, attachment.place);
                    let (to_time, to_place) = (attachment.jump.clone(), attachment.jump);
                    element.child(
                        div()
                            .mt(px(2.))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .when_some(time, |element, label| {
                                element.child(
                                    holds_the_mouse(div().id(("tag-time", key)))
                                        .flex()
                                        .items_center()
                                        .px(px(7.))
                                        .py(px(2.))
                                        .rounded_full()
                                        .bg(hex(ACCENT_FILL))
                                        .border_1()
                                        .border_color(tint(ACCENT, 0.3))
                                        .font_family(MONO)
                                        .text_size(px(10.))
                                        .text_color(tint(ACCENT, 0.95))
                                        .cursor_pointer()
                                        .hover(|style| style.border_color(tint(ACCENT, 0.7)))
                                        .when_some(to_time, |element, jump| {
                                            element.on_click(cx.listener(move |this, _, _, cx| {
                                                this.jump_to(jump.clone(), cx)
                                            }))
                                        })
                                        .child(label),
                                )
                            })
                            .when_some(place, |element, label| {
                                element.child(
                                    holds_the_mouse(div().id(("tag-place", key)))
                                        .flex()
                                        .items_center()
                                        .px(px(7.))
                                        .py(px(2.))
                                        .rounded_full()
                                        .bg(fg(0.08))
                                        .font_family(MONO)
                                        .text_size(px(10.))
                                        .text_color(ink())
                                        .cursor_pointer()
                                        .hover(|style| style.bg(fg(0.14)))
                                        .when_some(to_place, |element, jump| {
                                            element.on_click(cx.listener(move |this, _, _, cx| {
                                                this.jump_to(jump.clone(), cx)
                                            }))
                                        })
                                        .child(label),
                                )
                            }),
                    )
                },
            )
    }

    /// A thread: the anchored note and everything it started, indented to the
    /// code column and railed in the git accent.
    fn render_inline_comment(
        &self,
        index: usize,
        comment: ReviewComment,
        indent: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let group = format!("thread-{index}");
        let replies = comment
            .replies
            .iter()
            .enumerate()
            .map(|(position, reply)| {
                div()
                    .pt(px(9.))
                    .mt(px(9.))
                    .border_t_1()
                    .border_color(fg(0.07))
                    .child(self.render_message(
                        Message {
                            key: index * 64 + position + 1,
                            group: &group,
                            author: &reply.author,
                            id: &reply.id,
                            meta: None,
                            attachment: None,
                            body: reply.body.clone(),
                        },
                        cx,
                    ))
            })
            .collect::<Vec<_>>();

        let root = self.render_message(
            Message {
                key: index * 64,
                group: &group,
                author: &comment.author,
                id: &comment.id,
                meta: match comment.anchor {
                    Anchor::Line { .. } | Anchor::File => Some(comment.anchor.label()),
                    _ => None,
                },
                attachment: Some(Attachment {
                    time: match comment.anchor {
                        Anchor::Time {
                            seconds,
                            frame: Some(frame),
                            ..
                        } => Some(format!("{} f{frame}", media::timecode(seconds.0))),
                        Anchor::Time { seconds, .. } => Some(media::timecode(seconds.0)),
                        _ => None,
                    },
                    place: comment.anchor.spot().map(|spot| self.spot_label(spot)),
                    jump: Some(comment.id.clone()),
                }),
                body: comment.body.clone(),
            },
            cx,
        );

        div()
            .id(("inline-comment", index))
            .group(group)
            .pl(indent)
            .pr(px(18.))
            .py(px(6.))
            .bg(fg(0.02))
            .child(
                div()
                    .px(px(13.))
                    .py(px(11.))
                    .rounded(px(8.))
                    .bg(fg(0.05))
                    .child(root)
                    .children(replies),
            )
    }

    /// The draft. Shaped like the thread it will join, so committing a comment
    /// does not make the block jump.
    fn render_inline_composer(
        &self,
        line: Option<&DiffLine>,
        indent: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let replying = self.reply_to.is_some();
        let focused = self.draft.read(cx).is_focused(window);
        let heading = match (&self.reply_to, line.and_then(DiffLine::anchor)) {
            (Some(target), _) => format!("Replying to {target}"),
            (None, Some((side, number))) => {
                format!("New comment · line {number} · {}", side.label())
            }
            (None, None) => "New comment".to_string(),
        };

        div()
            .id("composer")
            .pl(indent)
            .pr(px(18.))
            // A reply tucks under the thread it answers; a new comment stands
            // on its own beneath the line.
            .pt(px(if replying { 0. } else { 6. }))
            .pb(px(8.))
            .bg(fg(0.02))
            .child(
                div()
                    .px(px(13.))
                    .py(px(11.))
                    .rounded(px(8.))
                    .bg(fg(0.06))
                    .child(
                        div()
                            .mb(px(8.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(tint(ACCENT, 0.92))
                                    .child(heading),
                            )
                            .child(
                                div().flex().items_center().gap(px(2.)).child(
                                    div()
                                        .id("cancel-inline")
                                        .px(px(8.))
                                        .py(px(2.))
                                        .rounded_full()
                                        .text_size(px(11.))
                                        .text_color(ink())
                                        .cursor_pointer()
                                        .hover(|style| style.bg(fg(0.08)).text_color(fg(0.9)))
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.cancel_comment(cx)),
                                        )
                                        .child("Cancel"),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .id("inline-editor")
                            .min_h(px(56.))
                            .px(px(11.))
                            .py(px(9.))
                            .rounded(px(6.))
                            .border_1()
                            // The composer sits inside a diff that also takes
                            // key input, so the border says which one has the
                            // keyboard.
                            .border_color(if focused { tint(ACCENT, 0.6) } else { fg(0.1) })
                            .bg(scrim(0.28))
                            .child(self.draft.clone()),
                    )
                    .when(self.noting_media, |element| {
                        // Two attachments, each coming off on its own: the
                        // moment, and the place.
                        let time = self.time_label();
                        let place = self.pending_spot.map(|spot| self.spot_label(spot));
                        element.child(
                            div()
                                .mt(px(8.))
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(6.))
                                .when_some(time.filter(|_| self.noting_time), |element, label| {
                                    element.child(Self::render_tag(
                                        "tag-time",
                                        label,
                                        Some(Box::new(|this, _window, cx| {
                                            this.noting_time = false;
                                            cx.notify();
                                        })),
                                        cx,
                                    ))
                                })
                                .when_some(place, |element, label| {
                                    element.child(Self::render_tag(
                                        "tag-place",
                                        label,
                                        Some(Box::new(|this, _window, cx| this.clear_spot(cx))),
                                        cx,
                                    ))
                                })
                                .when(self.pending_spot.is_none(), |element| {
                                    element.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(fg(0.4))
                                            .child("click the picture to attach a place"),
                                    )
                                }),
                        )
                    })
                    .child(
                        div()
                            .mt(px(8.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(11.)).text_color(fg(0.35)).child(
                                if replying {
                                    "⌘↵ reply · Esc cancel"
                                } else {
                                    "⌘↵ add comment · Esc cancel"
                                },
                            ))
                            .child(
                                Self::button(
                                    "add-inline",
                                    if replying { "Reply" } else { "Add comment" },
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.add_comment(cx))),
                            ),
                    ),
            )
    }

    /// Work out which rows the selected file shows, without building any.
    fn plan_diff(&mut self) {
        let mut rows = Vec::new();
        let Some(file) = self.diff.files.get(self.selected_file) else {
            self.diff_rows = rows;
            self.diff_list = ListState::new(0, ListAlignment::Top, px(600.));
            return;
        };

        for (index, line) in file.lines.iter().enumerate() {
            // The path and mode are already in the header, so the raw `---`,
            // `+++` and `index` lines are dropped rather than restyled.
            if line.kind == LineKind::Header && is_noise(&line.text) {
                continue;
            }
            if matches!(line.kind, LineKind::Header | LineKind::Hunk) || line.text.starts_with('\\')
            {
                rows.push(DiffRow::Band(index));
                continue;
            }

            rows.push(DiffRow::Code(index));

            if let Some((side, number)) = line.anchor() {
                for (position, comment) in self.review.comments.iter().enumerate() {
                    if comment.path == file.path && comment.anchor.line() == Some((side, number)) {
                        rows.push(DiffRow::Thread(position));
                        if self
                            .reply_to
                            .as_deref()
                            .is_some_and(|target| thread_of(target) == comment.id)
                        {
                            rows.push(DiffRow::Composer(index));
                        }
                    }
                }
            }

            if self
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.file == self.selected_file && anchor.line == index)
            {
                rows.push(DiffRow::Composer(index));
            }
        }

        self.diff_list = ListState::new(rows.len(), ListAlignment::Top, px(600.));
        self.diff_rows = rows;
    }

    /// Build one planned row.
    fn render_diff_row(&self, index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.diff_rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(file) = self.diff.files.get(self.selected_file) else {
            return div().into_any_element();
        };
        let gutter = gutter_width(file);

        match row {
            DiffRow::Band(line) => match file.lines.get(line) {
                Some(diff_line) => self.render_band(line, diff_line).into_any_element(),
                None => div().into_any_element(),
            },
            DiffRow::Code(line) => match file.lines.get(line) {
                Some(diff_line) => self
                    .render_line(self.selected_file, line, diff_line, gutter, cx)
                    .into_any_element(),
                None => div().into_any_element(),
            },
            DiffRow::Thread(position) => match self.review.comments.get(position) {
                Some(comment) => self
                    .render_inline_comment(position, comment.clone(), code_indent(gutter), cx)
                    .into_any_element(),
                None => div().into_any_element(),
            },
            DiffRow::Composer(line) => self
                .render_inline_composer(file.lines.get(line), code_indent(gutter), window, cx)
                .into_any_element(),
        }
    }

    /// One sidebar row: a directory heading, or a file under it.
    ///
    /// Rows are a fixed height so the list can place them without measuring —
    /// the rail that stands in for tgip's tree spine is drawn by the file rows
    /// themselves rather than by a container around them.
    fn render_sidebar_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.sidebar_rows.get(index) {
            Some(SidebarRow::Folder(label)) => div()
                .h(px(SIDEBAR_ROW))
                .w_full()
                .flex()
                .items_center()
                .px(px(10.))
                .child(
                    div()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(10.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ink())
                        .child(label.clone()),
                )
                .into_any_element(),
            Some(SidebarRow::File(file)) => {
                let file = *file;
                match self.diff.files.get(file) {
                    Some(diff) => div()
                        .h(px(SIDEBAR_ROW))
                        .w_full()
                        .flex()
                        .items_center()
                        .ml(px(9.))
                        .pl(px(6.))
                        .pr(px(4.))
                        .border_l_1()
                        .border_color(fg(0.09))
                        .child(self.render_file(file, diff, cx))
                        .into_any_element(),
                    None => div().h(px(SIDEBAR_ROW)).into_any_element(),
                }
            }
            None => div().h(px(SIDEBAR_ROW)).into_any_element(),
        }
    }

    /// Note whatever the pointer landed on, and open the composer for it. The
    /// spot is normalized against the displayed size, so it stays correct at
    /// any zoom and for anyone opening the review later.
    fn place_spot(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return;
        }
        let local = position - bounds.origin;
        self.pending_spot = Some(Spot {
            x: (f32::from(local.x) / f32::from(bounds.size.width)).clamp(0., 1.),
            y: (f32::from(local.y) / f32::from(bounds.size.height)).clamp(0., 1.),
        });
        self.noting_media = true;
        self.noting_time = true;
        self.anchor = None;
        self.reply_to = None;
        self.status = None;
        self.draft.update(cx, |draft, cx| {
            draft.set_placeholder("Write a review comment…", cx);
        });
        self.draft.read(cx).focus(window);
        cx.notify();
    }

    /// Take the player to the moment a note was left at, selecting its file
    /// first if the note is on a different one.
    ///
    /// Without this a video review is one-way: notes go in at a moment and
    /// nothing brings you back to it.
    fn jump_to(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(comment) = self.review.find(&id) else {
            return;
        };
        let (path, seconds, spot) = (
            comment.path.clone(),
            comment.anchor.seconds(),
            comment.anchor.spot(),
        );

        if self
            .diff
            .files
            .get(self.selected_file)
            .map(|file| &file.path)
            != Some(&path)
        {
            let Some(index) = self.diff.files.iter().position(|file| file.path == path) else {
                return;
            };
            self.select_file(index, cx);
        }

        if let Some(seconds) = seconds {
            self.seek(seconds, cx);
        }
        // Show which note is being looked at while the picture catches up.
        self.status = spot.map(|spot| format!("{id} · {}", spot.label()));
        cx.notify();
    }

    /// A divider between panels. Dragging it resizes the panel beside it.
    fn render_divider(&self, which: Dragging, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = match which {
            Dragging::Sidebar => "divide-sidebar",
            Dragging::Notes => "divide-notes",
        };
        holds_the_mouse(div().id(id))
            .flex_none()
            .w(px(6.))
            .h_full()
            .cursor(CursorStyle::ResizeLeftRight)
            .hover(|style| style.bg(tint(ACCENT, 0.35)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.dragging = Some(which);
                    cx.notify();
                }),
            )
    }

    /// Follow the pointer while a divider is held.
    fn drag_divider(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let Some(dragging) = self.dragging else {
            return;
        };
        let x = f32::from(x);
        match dragging {
            Dragging::Sidebar => {
                self.sidebar_width = (x - OUTER_PADDING).clamp(190., 520.);
            }
            Dragging::Notes => {
                let width = f32::from(window.viewport_size().width);
                self.notes_width = (width - x - OUTER_PADDING).clamp(220., 620.);
            }
        }
        cx.notify();
    }

    /// Start a note about the media without pointing at anything in it.
    fn start_media_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.noting_media = true;
        self.noting_time = true;
        self.pending_spot = None;
        self.anchor = None;
        self.reply_to = None;
        self.status = None;
        self.draft.update(cx, |draft, cx| {
            draft.set_placeholder("Write a review comment…", cx);
        });
        self.draft.read(cx).focus(window);
        cx.notify();
    }

    /// Drop the pin from the note being written, leaving the note itself.
    fn clear_spot(&mut self, cx: &mut Context<Self>) {
        self.pending_spot = None;
        cx.notify();
    }

    /// Comments already on the thing being looked at, in review order so the
    /// pins carry the same numbers as the exported brief.
    fn spots_here(&self, path: &str) -> Vec<Marker> {
        self.review
            .comments
            .iter()
            .enumerate()
            .filter(|(_, comment)| comment.path == path)
            .filter_map(|(index, comment)| {
                let spot = comment.anchor.spot()?;
                // On a video, only markers for the moment on screen.
                if let Some(seconds) = comment.anchor.seconds()
                    && (seconds - self.time).abs() > 0.25
                {
                    return None;
                }
                Some(Marker {
                    number: index + 1,
                    id: comment.id.clone(),
                    spot,
                    seconds: comment.anchor.seconds(),
                    frame: match comment.anchor {
                        Anchor::Time { frame, .. } => frame,
                        _ => None,
                    },
                })
            })
            .collect()
    }

    /// A chip marking a place on the picture.
    ///
    /// It carries the frame it sits on rather than the note's number, because
    /// on a video that is the useful fact: a composition is edited by frame, so
    /// the marker says both where and when.
    /// One attachment on a note: a moment, or a place. Each is its own chip and
    /// each comes off on its own — a note can be pinned to both, either, or
    /// neither.
    fn render_tag(
        id: impl Into<gpui::ElementId>,
        label: String,
        remove: Option<Detach>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        holds_the_mouse(div().id(id))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.))
            .px(px(7.))
            .py(px(2.))
            .rounded_full()
            .bg(hex(ACCENT_FILL))
            .border_1()
            .border_color(tint(ACCENT, 0.3))
            .font_family(MONO)
            .text_size(px(10.))
            .text_color(tint(ACCENT, 0.95))
            .child(label)
            .when_some(remove, |element, remove| {
                element.child(
                    div()
                        .id("remove-tag")
                        .text_color(fg(0.5))
                        .cursor_pointer()
                        .hover(|style| style.text_color(hex(DEL_MARK)))
                        .on_click(cx.listener(move |this, _, window, cx| remove(this, window, cx)))
                        .child("×"),
                )
            })
    }

    /// The moment chip: the timecode a person reads and the frame a composition
    /// is edited by, in one tag.
    fn time_label(&self) -> Option<String> {
        (self.medium == Medium::Video).then(|| match self.probe {
            Some(probe) => format!(
                "{} f{}",
                media::timecode(self.time),
                probe.frame_at(self.time)
            ),
            None => media::timecode(self.time),
        })
    }

    /// The place chip, in the media's own pixels rather than a fraction.
    fn spot_label(&self, spot: Spot) -> String {
        match self.media_size {
            Some((width, height)) => format!(
                "{}px:{}px",
                (spot.x * width as f32).round() as u32,
                (spot.y * height as f32).round() as u32
            ),
            None => spot.label(),
        }
    }

    fn render_chip(
        &self,
        marker: &Marker,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let target = marker.id.clone();
        let label = match (marker.seconds, marker.frame) {
            (Some(seconds), Some(frame)) => format!("{} f{frame}", media::timecode(seconds)),
            (Some(seconds), None) => media::timecode(seconds),
            _ => marker.number.to_string(),
        };

        holds_the_mouse(div().id(("chip", marker.number)))
            .absolute()
            .left(relative(marker.spot.x))
            .top(relative(marker.spot.y))
            // Anchored at its left edge on the point, sitting just above it.
            .ml(px(-6.))
            .mt(px(-13.))
            .flex()
            .items_center()
            .px(px(7.))
            .py(px(2.))
            .rounded_full()
            .bg(rgb(ACCENT))
            .border_2()
            .border_color(scrim(0.55))
            .font_family(MONO)
            .text_size(px(10.))
            .font_weight(FontWeight::BOLD)
            .text_color(scrim(0.82))
            .when(pending, |element| element.opacity(0.85))
            .when(!pending, |element| {
                element
                    .cursor_pointer()
                    .hover(|style| style.border_color(fg(0.9)))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(target.clone(), cx)))
            })
            .child(label)
    }

    /// The pane for something that is looked at rather than read: an image, or
    /// a frame of a video with a timeline under it.
    fn render_media(&self, file: &FileDiff, cx: &mut Context<Self>) -> gpui::Div {
        let path = file.path.clone();
        let still = (self.medium != Medium::Video).then(|| self.repository.root.join(&path));
        let frame = self.frame.clone();
        let spots = self.spots_here(&path);
        let pending = self.pending_spot;

        // A surface has no intrinsic size — unlike an image it contributes
        // nothing to layout — so the video is given an explicit rect, fitted
        // inside the stage. Fitting it ourselves rather than letting the
        // element letterbox also keeps the pins on the picture: a spot is a
        // fraction of the video, not of the empty space around it.
        let video_rect = match (frame.as_ref(), self.stage_bounds) {
            (Some(frame), Some(stage)) => Some(fit(
                stage,
                frame.get_width() as f32,
                frame.get_height() as f32,
            )),
            _ => None,
        };

        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("media-stage")
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p(px(18.))
                            // Stretched over the stage: a canvas sizes to its content,
                            // which is nothing, and a zero-size rect would scale the
                            // video down to nothing with it.
                            .child(
                                canvas(
                                    {
                                        let view = cx.entity();
                                        move |bounds, _, cx| {
                                            view.update(cx, |view, _| {
                                                view.stage_bounds = Some(bounds)
                                            });
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .map(|element| {
                                let Some(surface_element) = self
                                    .render_surface(still, frame, video_rect, spots, pending, cx)
                                else {
                                    return element.child(Self::empty_state(
                                "Opening the clip",
                                "AVFoundation is loading it. If this never finishes, the file \
                                 may not be a format macOS can decode.",
                            ));
                                };
                                element.child(surface_element)
                            }),
                    )
                    .when(self.medium == Medium::Video, |element| {
                        element.child(self.render_timeline(cx))
                    }),
            )
            // Notes beside the picture rather than under it: a video pane is
            // wide and short, so the room is at the side.
            .when(self.has_notes(&path), |element| {
                element.child(self.render_divider(Dragging::Notes, cx))
            })
            .children(self.render_media_threads(&path, cx))
    }

    /// The picture: a decoded frame at its fitted rect, or a still image that
    /// sizes itself. Pins and clicks land on whichever it is, so a spot always
    /// means a fraction of the picture.
    #[allow(clippy::type_complexity)]
    fn render_surface(
        &self,
        still: Option<PathBuf>,
        frame: Option<CVPixelBuffer>,
        video_rect: Option<Bounds<Pixels>>,
        spots: Vec<Marker>,
        pending: Option<Spot>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let stage = self.stage_bounds;

        // The picture goes down first. Primitives are ordered by the sequence
        // they are painted in, so anything added before it ends up underneath —
        // which is where the pins were.
        let mut body = div().id("media-surface").relative().cursor_crosshair();
        body = match (&frame, &still) {
            (Some(frame), _) => body.child(surface(frame.clone()).size_full()),
            (_, Some(still)) => body.child(img(still.clone()).max_w_full().max_h_full()),
            _ => return None,
        };

        let body = body
            .children({
                // Built eagerly: two closures cannot both borrow the context.
                let mut chips: Vec<_> = spots
                    .iter()
                    .map(|marker| self.render_chip(marker, false, cx))
                    .collect();
                if let Some(spot) = pending {
                    // The chip about to be committed shows the frame it will
                    // carry, so the label does not change on saving.
                    chips.push(
                        self.render_chip(
                            &Marker {
                                number: 0,
                                id: String::new(),
                                spot,
                                seconds: (self.medium == Medium::Video).then_some(self.time),
                                frame: (self.medium == Medium::Video)
                                    .then(|| self.probe.map(|probe| probe.frame_at(self.time)))
                                    .flatten(),
                            },
                            true,
                            cx,
                        ),
                    );
                }
                chips
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if let Some(bounds) = this.media_bounds {
                        this.place_spot(event.position, bounds, window, cx);
                    }
                }),
            )
            .child(
                canvas(
                    {
                        let view = cx.entity();
                        move |bounds, _, cx| {
                            view.update(cx, |view, _| view.media_bounds = Some(bounds));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );

        match (frame, still) {
            // A decoded frame has no intrinsic size, so it is placed at the
            // rect computed for it; an image sizes itself.
            (Some(_), _) => {
                let (rect, stage) = (video_rect?, stage?);
                Some(
                    body.absolute()
                        .left(rect.origin.x - stage.origin.x)
                        .top(rect.origin.y - stage.origin.y)
                        .w(rect.size.width)
                        .h(rect.size.height),
                )
            }
            (_, Some(_)) => Some(body.max_w_full().max_h_full()),
            _ => None,
        }
    }

    fn has_notes(&self, path: &str) -> bool {
        self.review.comments.iter().any(|c| c.path == path) || self.medium != Medium::Text
    }

    /// The notes left on this render, newest last, each one a way back to the
    /// moment or place it was left at.
    fn render_media_threads(
        &self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let threads: Vec<(usize, ReviewComment)> = self
            .review
            .comments
            .iter()
            .enumerate()
            .filter(|(_, comment)| comment.path == path)
            .map(|(index, comment)| (index, comment.clone()))
            .collect();

        let heading = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .px(px(12.))
            .py(px(10.))
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ink())
                    .child(match threads.len() {
                        0 => "No notes yet".to_string(),
                        1 => "1 note".to_string(),
                        count => format!("{count} notes"),
                    }),
            )
            .child(
                Self::button("add-media-note", "Add note", false, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.start_media_note(window, cx))),
            );

        Some(
            div()
                .id("media-threads")
                .flex_none()
                .w(px(self.notes_width))
                .h_full()
                .overflow_y_scroll()
                .border_l_1()
                .border_color(fg(0.08))
                .child(heading)
                .when(threads.is_empty(), |element| {
                    element.child(
                        div()
                            .px(px(12.))
                            .text_size(px(11.))
                            .line_height(px(17.))
                            .text_color(fg(0.45))
                            .child(
                                "Click the picture to mark a place in it, or add a note \
                                 about the moment or the render as a whole.",
                            ),
                    )
                })
                .children(threads.into_iter().map(|(index, comment)| {
                    // The anchor rides on the message as an attachment, so
                    // nothing sits above the card.
                    self.render_inline_comment(index, comment, px(12.), cx)
                })),
        )
    }

    /// The scrubber. Clicking it seeks; the ticks are the moments already
    /// carrying a comment, so a review reads back as marks on the timeline.
    fn render_timeline(&self, cx: &mut Context<Self>) -> gpui::Div {
        let duration = self.probe.map(|probe| probe.duration).unwrap_or(0.);
        let progress = if duration > 0. {
            (self.time / duration) as f32
        } else {
            0.
        };
        let path = self
            .diff
            .files
            .get(self.selected_file)
            .map(|file| file.path.clone())
            .unwrap_or_default();
        let marks: Vec<f32> = self
            .review
            .comments
            .iter()
            .filter(|comment| comment.path == path)
            .filter_map(|comment| comment.anchor.seconds())
            .filter(|_| duration > 0.)
            .map(|seconds| (seconds / duration) as f32)
            .collect();

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(18.))
            .py(px(12.))
            .border_t_1()
            .border_color(fg(0.08))
            .child(
                div()
                    .id("timeline")
                    .relative()
                    .h(px(22.))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                            let Some(bounds) = this.timeline_bounds else {
                                return;
                            };
                            let width = f32::from(bounds.size.width);
                            if width <= 0. {
                                return;
                            }
                            let ratio = (f32::from(event.position.x - bounds.origin.x) / width)
                                .clamp(0., 1.);
                            let duration = this.probe.map(|probe| probe.duration).unwrap_or(0.);
                            this.seek(f64::from(ratio) * duration, cx);
                        }),
                    )
                    .child(
                        canvas(
                            {
                                let view = cx.entity();
                                move |bounds, _, cx| {
                                    view.update(cx, |view, _| view.timeline_bounds = Some(bounds));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    // Track.
                    .child(
                        div()
                            .absolute()
                            .top(px(9.))
                            .left_0()
                            .right_0()
                            .h(px(4.))
                            .rounded_full()
                            .bg(fg(0.1)),
                    )
                    // Played.
                    .child(
                        div()
                            .absolute()
                            .top(px(9.))
                            .left_0()
                            .w(relative(progress))
                            .h(px(4.))
                            .rounded_full()
                            .bg(tint(ACCENT, 0.75)),
                    )
                    // Moments already commented on.
                    .children(marks.into_iter().map(|at| {
                        div()
                            .absolute()
                            .top(px(4.))
                            .left(relative(at))
                            .w(px(2.))
                            .h(px(14.))
                            .rounded_full()
                            .bg(rgb(ACCENT))
                    }))
                    // Playhead.
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .left(relative(progress))
                            .ml(px(-1.))
                            .w(px(2.))
                            .h(px(18.))
                            .rounded_full()
                            .bg(fg(0.95)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(ink())
                    .child(
                        holds_the_mouse(div().id("play"))
                            .flex_none()
                            .size(px(24.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(fg(0.08))
                            .text_size(px(10.))
                            .text_color(ink())
                            .cursor_pointer()
                            .hover(|style| style.bg(fg(0.14)))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)))
                            .child(if self.is_playing() { "❚❚" } else { "▶" }),
                    )
                    .child(match self.probe {
                        Some(probe) => format!(
                            "{}  ·  frame {}",
                            media::timecode(self.time),
                            probe.frame_at(self.time)
                        ),
                        None => media::timecode(self.time),
                    })
                    .child(div().flex_1())
                    .child(
                        self.probe
                            .map(|probe| media::timecode(probe.duration))
                            .unwrap_or_else(|| "—".into()),
                    ),
            )
    }

    fn empty_state(title: impl Into<SharedString>, message: impl Into<SharedString>) -> gpui::Div {
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
                    .child(title.into()),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(fg(0.5))
                    .child(message.into()),
            )
    }
}

/// Width for a line-number column that fits the file's widest number.
///
/// The columns are fixed width, and a number too wide for one wraps inside it —
/// which doubles the row's height and reads as a blank line that isn't in the
/// diff at all. Four digits only just fitted the old fixed 44px, and the second
/// column lost another pixel to its border, so five-figure files wrapped every
/// row.
fn gutter_width(file: &FileDiff) -> Pixels {
    let widest = file
        .lines
        .iter()
        .filter_map(|line| line.new_line.max(line.old_line))
        .max()
        .unwrap_or(1);
    // ~8px per digit at 13px monospace, plus the 6px padding on each side and a
    // pixel for the divider.
    let digits = widest.to_string().len() as f32;
    px((digits * 8. + 13.).max(GUTTER))
}

/// Where a thread hangs in a diff: level with the code, past both gutters and
/// the marker column.
fn code_indent(gutter: Pixels) -> Pixels {
    gutter + gutter + px(MARKER)
}

/// The largest rect of a given aspect that fits inside `stage`, centred.
fn fit(stage: Bounds<Pixels>, width: f32, height: f32) -> Bounds<Pixels> {
    if width <= 0. || height <= 0. {
        return stage;
    }
    let available = (f32::from(stage.size.width), f32::from(stage.size.height));
    let scale = (available.0 / width).min(available.1 / height);
    let (drawn_width, drawn_height) = (width * scale, height * scale);
    Bounds {
        origin: point(
            stage.origin.x + px((available.0 - drawn_width) / 2.),
            stage.origin.y + px((available.1 - drawn_height) / 2.),
        ),
        size: size(px(drawn_width), px(drawn_height)),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            "{}{file_count} changed file{} · {note_count} review note{}",
            // A branch review is worth naming; uncommitted work is the default
            // and reads as noise in the header.
            if self.base.is_working_tree() {
                String::new()
            } else {
                format!("{} · ", self.base.label())
            },
            if file_count == 1 { "" } else { "s" },
            if note_count == 1 { "" } else { "s" }
        );

        // Sidebar — part of the window itself, painted straight onto the glass
        // with no panel fill or divider, exactly like tgip's.
        let sidebar = div()
            .w(px(self.sidebar_width))
            .flex_none()
            .flex()
            .flex_col()
            // Room for the traffic lights, which sit at the sidebar's own inset.
            .child(draggable(div().h(px(TITLEBAR_INSET)).flex_none()))
            .child(
                draggable(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .px(px(12.))
                        .pb(px(10.)),
                )
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
                                .text_color(ink())
                                .child(root_name),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(10.))
                        .text_color(ink())
                        .child(root_path),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(ink())
                        .child(change_summary),
                ),
            )
            .child(
                draggable(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(14.))
                        .py(px(8.)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ink())
                        .child("Changed Files"),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(11.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(ink())
                        .child(file_count.to_string()),
                ),
            )
            .child(
                // Chrome, so the gaps between and below the rows drag the
                // window. The rows themselves opt out.
                draggable(div())
                    .flex_1()
                    .min_h_0()
                    .px(px(4.))
                    .pb(px(6.))
                    .child(
                        uniform_list(
                            "file-list",
                            self.sidebar_rows.len(),
                            cx.processor(|this, range: Range<usize>, _window, cx| {
                                range
                                    .map(|index| this.render_sidebar_row(index, cx))
                                    .collect()
                            }),
                        )
                        .size_full(),
                    ),
            )
            .when_some(self.update.clone(), |element, version| {
                let command = update::Install::detect().upgrade_hint();
                element.child(
                    holds_the_mouse(div().id("update-banner"))
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
                                .text_color(ink())
                                .child(command),
                        ),
                )
            })
            .child(
                draggable(div())
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
                            .text_color(ink())
                            .child(format!("{note_count} ⌸")),
                    )
                    .child(div().flex_1())
                    .child(
                        Self::button("copy", "Copy Markdown", false, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_markdown(cx))),
                    )
                    .child(
                        Self::button("finish", "Finish", true, cx)
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
            // A render is read by looking at it, not by scrolling a patch.
            .map(|element| match (self.medium, selected_file) {
                (Medium::Text, _) | (_, None) => element.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .when(self.diff.files.is_empty(), |element| {
                            element.child(if self.base.is_working_tree() {
                                Self::empty_state(
                                    "Working tree is clean",
                                    "No tracked, staged, or untracked changes are waiting for review.",
                                )
                            } else {
                                Self::empty_state(
                                    "Nothing to review",
                                    format!("{} has no changes.", self.base.label()),
                                )
                            })
                        })
                        .when(!self.diff.files.is_empty(), |element| {
                            element.child(
                                list(
                                    self.diff_list.clone(),
                                    cx.processor(|this, index: usize, window, cx| {
                                        this.render_diff_row(index, window, cx)
                                    }),
                                )
                                .size_full(),
                            )
                        }),
                ),
                (_, Some(file)) => element
                    .child(self.render_media(file, cx))
                    .when(self.noting_media, |element| {
                        element.child(self.render_inline_composer(None, px(12.), window, cx))
                    }),
            });

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key))
            // Tracked at the root so the pointer can leave the divider without
            // dropping the drag.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.dragging.is_some() {
                    this.drag_divider(event.position.x, window, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.take().is_some() {
                        cx.notify();
                    }
                }),
            )
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
            .child(self.render_divider(Dragging::Sidebar, cx))
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

    /// The gutter has to fit the file's widest line number. When it did not,
    /// the number wrapped inside the column, doubling the row height so the
    /// diff appeared to contain blank lines that were never in it.
    #[test]
    fn the_gutter_grows_with_the_line_numbers() {
        let narrow = gutter_width(&file_ending_at(42));
        let wide = gutter_width(&file_ending_at(10_046));
        assert!(wide > narrow, "{wide:?} should exceed {narrow:?}");

        // Five digits at ~8px plus padding, and never below the minimum.
        assert!(wide >= px(53.));
        assert_eq!(narrow, px(GUTTER));
        assert_eq!(gutter_width(&file_ending_at(1)), px(GUTTER));
    }

    /// A file with no numbered lines at all must still lay out.
    #[test]
    fn an_empty_file_still_gets_a_gutter() {
        let file = FileDiff {
            path: "empty.rs".into(),
            additions: 0,
            deletions: 0,
            lines: Vec::new(),
        };
        assert_eq!(gutter_width(&file), px(GUTTER));
    }

    fn file_ending_at(last: u32) -> FileDiff {
        FileDiff {
            path: "src/lib.rs".into(),
            additions: 1,
            deletions: 0,
            lines: vec![DiffLine {
                kind: LineKind::Addition,
                old_line: None,
                new_line: Some(last),
                text: "+value".into(),
            }],
        }
    }

    /// A surface has no intrinsic size, so the video's rect is computed rather
    /// than laid out. Getting it wrong once meant a black pane with audio.
    #[test]
    fn video_is_fitted_inside_the_stage() {
        let stage = Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(800.), px(600.)),
        };

        // Wider than the stage: letterboxed, pinned to full width.
        let wide = fit(stage, 1920., 1080.);
        assert_eq!(wide.size.width, px(800.));
        assert_eq!(wide.size.height, px(450.));
        assert_eq!(wide.origin.x, px(10.));
        assert_eq!(wide.origin.y, px(20. + 75.)); // centred vertically

        // Taller than the stage — a 9:16 render — pillarboxed.
        let tall = fit(stage, 1080., 1920.);
        assert_eq!(tall.size.height, px(600.));
        assert_eq!(tall.size.width, px(337.5));
        assert_eq!(tall.origin.y, px(20.));
    }

    #[test]
    fn a_degenerate_frame_does_not_divide_by_zero() {
        let stage = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(100.), px(100.)),
        };
        assert_eq!(fit(stage, 0., 0.).size, stage.size);
    }

    #[test]
    fn diff_plumbing_is_treated_as_noise() {
        assert!(is_noise("index 1a2b3c..4d5e6f 100644"));
        assert!(is_noise("+++ b/src/app.rs"));
        assert!(!is_noise("new file mode 100644"));
    }
}
