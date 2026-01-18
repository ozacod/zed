mod image_info;
mod image_viewer_settings;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use editor::items::entry_git_aware_label_color;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyContext, MouseDownEvent, ObjectFit, ParentElement, Pixels,
    Render, RenderImage, ScrollHandle, ScrollWheelEvent, Styled, Task, WeakEntity, Window, actions,
    canvas, div, fill, img, opaque_grey, point, px, size,
};
use language::File as _;
use persistence::IMAGE_VIEWER;
use project::{ImageItem, Project, ProjectPath, image_store::ImageItemEvent};
use settings::Settings;
use ui::{ScrollAxes, Scrollbars, Tooltip, WithScrollbar, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{Item, ProjectItem, SerializableItem, TabContentParams},
};

pub use crate::image_info::*;
pub use crate::image_viewer_settings::*;

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 100.0;
const ZOOM_STEP: f32 = 0.1;

actions!(
    image_viewer,
    [
        ZoomIn,
        ZoomOut,
        ResetZoom,
        ZoomToActualSize,
        ToggleEyedropper,
        CopyColorToClipboard
    ]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ZoomMode {
    Fit,
    Manual,
}

pub struct ImageView {
    image_item: Entity<ImageItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    zoom_level: f32,
    zoom_mode: ZoomMode,
    scroll_handle: ScrollHandle,
    eyedropper_mode: bool,
    picked_color: Option<PickedColor>,
}

#[derive(Clone, Copy)]
struct PickedColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl PickedColor {
    fn to_hex(&self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    fn to_gpui_color(&self) -> gpui::Rgba {
        gpui::rgba(
            ((self.r as u32) << 24 | (self.g as u32) << 16 | (self.b as u32) << 8 | (self.a as u32))
                as u32,
        )
    }
}

impl ImageView {
    pub fn new(
        image_item: Entity<ImageItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&image_item, Self::on_image_event).detach();
        cx.on_release_in(window, |this, window, cx| {
            let image_data = this.image_item.read(cx).image.clone();
            if let Some(image) = image_data.clone().get_render_image(window, cx) {
                cx.drop_image(image, None);
            }
            image_data.remove_asset(cx);
        })
        .detach();

        Self {
            image_item,
            project,
            focus_handle: cx.focus_handle(),
            zoom_level: 1.0,
            zoom_mode: ZoomMode::Fit,
            scroll_handle: ScrollHandle::new(),
            eyedropper_mode: false,
            picked_color: None,
        }
    }

    fn on_image_event(
        &mut self,
        _: Entity<ImageItem>,
        event: &ImageItemEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ImageItemEvent::MetadataUpdated
            | ImageItemEvent::FileHandleChanged
            | ImageItemEvent::Reloaded => {
                cx.emit(ImageViewEvent::TitleChanged);
                cx.notify();
            }
            ImageItemEvent::ReloadNeeded => {}
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoom_mode == ZoomMode::Fit {
            self.zoom_level = self.calculate_fit_zoom(window, cx);
            self.zoom_mode = ZoomMode::Manual;
        }

        let old_zoom = self.zoom_level;
        let new_zoom = (self.zoom_level + ZOOM_STEP).min(MAX_ZOOM);

        if old_zoom != new_zoom {
            self.adjust_zoom_center(old_zoom, new_zoom, window, cx);
            self.zoom_level = new_zoom;
            cx.notify();
        }
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoom_mode == ZoomMode::Fit {
            self.zoom_level = self.calculate_fit_zoom(window, cx);
            self.zoom_mode = ZoomMode::Manual;
        }

        let old_zoom = self.zoom_level;
        let new_zoom = (self.zoom_level - ZOOM_STEP).max(MIN_ZOOM);

        if old_zoom != new_zoom {
            self.adjust_zoom_center(old_zoom, new_zoom, window, cx);
            self.zoom_level = new_zoom;
            cx.notify();
        }
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_mode = ZoomMode::Fit;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn zoom_to_actual_size(
        &mut self,
        _: &ZoomToActualSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.zoom_mode == ZoomMode::Fit {
            self.zoom_level = self.calculate_fit_zoom(window, cx);
            self.zoom_mode = ZoomMode::Manual;
        }

        let old_zoom = self.zoom_level;
        let new_zoom = 1.0;

        if old_zoom != new_zoom {
            self.adjust_zoom_center(old_zoom, new_zoom, window, cx);
            self.zoom_level = new_zoom;
            cx.notify();
        }
    }

    fn toggle_eyedropper(
        &mut self,
        _: &ToggleEyedropper,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.eyedropper_mode = !self.eyedropper_mode;
        cx.notify();
    }

    fn copy_color_to_clipboard(
        &mut self,
        _: &CopyColorToClipboard,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(color) = self.picked_color {
            let hex = color.to_hex();
            cx.write_to_clipboard(ClipboardItem::new_string(hex));
        }
    }

    fn pick_color_at_position(
        &mut self,
        position: gpui::Point<Pixels>,
        render_image: &Arc<RenderImage>,
        image_bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let image_size = render_image.size(0);
        let image_width = image_size.width.0 as f32;
        let image_height = image_size.height.0 as f32;

        let rel_x: f32 = (position.x - image_bounds.origin.x).into();
        let rel_y: f32 = (position.y - image_bounds.origin.y).into();
        let bounds_width: f32 = image_bounds.size.width.into();
        let bounds_height: f32 = image_bounds.size.height.into();

        let pixel_x = (rel_x / bounds_width * image_width) as i32;
        let pixel_y = (rel_y / bounds_height * image_height) as i32;

        if pixel_x >= 0
            && pixel_x < image_width as i32
            && pixel_y >= 0
            && pixel_y < image_height as i32
        {
            if let Some(bytes) = render_image.as_bytes(0) {
                let stride = image_width as usize * 4;
                let offset = (pixel_y as usize * stride) + (pixel_x as usize * 4);

                if offset + 3 < bytes.len() {
                    self.picked_color = Some(PickedColor {
                        r: bytes[offset],
                        g: bytes[offset + 1],
                        b: bytes[offset + 2],
                        a: bytes[offset + 3],
                    });
                    cx.notify();
                }
            }
        }
    }

    fn adjust_zoom_center(
        &mut self,
        old_zoom: f32,
        new_zoom: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let image = self.image_item.read(cx).image.clone();
        let image_size = image
            .use_render_image(window, cx)
            .map(|data| {
                let size = data.size(0);
                gpui::size(px(size.width.0 as f32), px(size.height.0 as f32))
            })
            .unwrap_or(size(px(0.), px(0.)));

        let scaled_size = size(image_size.width * old_zoom, image_size.height * old_zoom);

        let max_offset = self.scroll_handle.max_offset();
        let viewport_w = scaled_size.width - max_offset.width;
        let viewport_h = scaled_size.height - max_offset.height;

        let viewport_center = point(viewport_w / 2.0, viewport_h / 2.0);

        let zoom_factor = new_zoom / old_zoom;
        let scroll_offset = self.scroll_handle.offset();

        let new_offset = scroll_offset * zoom_factor + viewport_center * (zoom_factor - 1.0);
        self.scroll_handle.set_offset(new_offset);
    }

    fn calculate_fit_zoom(&self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let viewport_size = self.scroll_handle.bounds().size;
        let image = self.image_item.read(cx).image.clone();
        let image_size = image
            .use_render_image(window, cx)
            .map(|data| {
                let size = data.size(0);
                gpui::size(px(size.width.0 as f32), px(size.height.0 as f32))
            })
            .unwrap_or(size(px(0.), px(0.)));

        if viewport_size.width > px(0.)
            && viewport_size.height > px(0.)
            && image_size.width > px(0.)
            && image_size.height > px(0.)
        {
            let width_ratio = viewport_size.width / image_size.width;
            let height_ratio = viewport_size.height / image_size.height;
            width_ratio
                .min(height_ratio)
                .min(1.0)
                .clamp(MIN_ZOOM, MAX_ZOOM)
        } else {
            1.0
        }
    }
}

pub enum ImageViewEvent {
    TitleChanged,
}

impl EventEmitter<ImageViewEvent> for ImageView {}

impl Item for ImageView {
    type Event = ImageViewEvent;

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(workspace::item::ItemEvent)) {
        match event {
            ImageViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.image_item.entity_id(), self.image_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.image_item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(file_path.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.image_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.image_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.image_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            image_item: self.image_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            zoom_level: 1.0,
            zoom_mode: ZoomMode::Fit,
            scroll_handle: ScrollHandle::new(),
            eyedropper_mode: false,
            picked_color: None,
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.image_item.read(cx).file.disk_state().is_deleted()
    }
    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl SerializableItem for ImageView {
    fn serialized_item_kind() -> &'static str {
        "ImageView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let image_path = IMAGE_VIEWER
                .get_image_path(item_id, workspace_id)?
                .context("No image path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(image_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let image_item = project
                .update(cx, |project, cx| project.open_image(project_path, cx))
                .await?;

            cx.update(
                |window, cx| Ok(cx.new(|cx| ImageView::new(image_item, project, window, cx))),
            )?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        delete_unloaded_items(
            alive_items,
            workspace_id,
            "image_viewers",
            &IMAGE_VIEWER,
            cx,
        )
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let image_path = self.image_item.read(cx).abs_path(cx)?;

        Some(cx.background_spawn({
            async move {
                log::debug!("Saving image at path {image_path:?}");
                IMAGE_VIEWER
                    .save_image_path(item_id, workspace_id, image_path)
                    .await
            }
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

impl EventEmitter<()> for ImageView {}
impl Focusable for ImageView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.image_item.read(cx).image.clone();
        let image_size = image
            .clone()
            .use_render_image(window, cx)
            .map(|data| {
                let size = data.size(0);
                gpui::size(px(size.width.0 as f32), px(size.height.0 as f32))
            })
            .unwrap_or(size(px(0.), px(0.)));

        let zoom_level = if self.zoom_mode == ZoomMode::Fit {
            self.calculate_fit_zoom(window, cx)
        } else {
            self.zoom_level
        };

        let scaled_width = image_size.width * zoom_level;
        let scaled_height = image_size.height * zoom_level;

        let checkered_background =
            |bounds: Bounds<Pixels>, _, window: &mut Window, _cx: &mut App| {
                let square_size: f32 = 32.0;

                let start_y = bounds.origin.y.into();
                let height: f32 = bounds.size.height.into();
                let start_x = bounds.origin.x.into();
                let width: f32 = bounds.size.width.into();

                let mut y = start_y;
                let mut x = start_x;
                let mut color_swapper = true;
                // draw checkerboard pattern
                while y < start_y + height {
                    // Keeping track of the grid in order to be resilient to resizing
                    let start_swap = color_swapper;
                    while x < start_x + width {
                        // Clamp square dimensions to not exceed bounds
                        let square_width = square_size.min(start_x + width - x);
                        let square_height = square_size.min(start_y + height - y);

                        let rect = Bounds::new(
                            point(px(x), px(y)),
                            size(px(square_width), px(square_height)),
                        );

                        let color = if color_swapper {
                            opaque_grey(0.6, 0.4)
                        } else {
                            opaque_grey(0.7, 0.4)
                        };

                        window.paint_quad(fill(rect, color));
                        color_swapper = !color_swapper;
                        x += square_size;
                    }
                    x = start_x;
                    color_swapper = !start_swap;
                    y += square_size;
                }
            };

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("ImageView");

        div()
            .size_full()
            .flex()
            .flex_col()
            .relative() // Ensure custom scrollbars anchor to the pane edges
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::zoom_to_actual_size))
            .on_action(cx.listener(Self::toggle_eyedropper))
            .on_action(cx.listener(Self::copy_color_to_clipboard))
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().styles.colors.border)
                    .bg(cx.theme().styles.colors.toolbar_background)
                    .child(
                        IconButton::new("zoom_in", IconName::Plus)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.zoom_in(&ZoomIn, window, cx)
                                }),
                            )
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Zoom In", &ZoomIn, cx)
                            }),
                    )
                    .child(
                        IconButton::new("zoom_out", IconName::Dash)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.zoom_out(&ZoomOut, window, cx)
                            }))
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Zoom Out", &ZoomOut, cx)
                            }),
                    )
                    .child(
                        IconButton::new("reset_zoom", IconName::Maximize)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_zoom(&ResetZoom, window, cx)
                            }))
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Reset Zoom", &ResetZoom, cx)
                            }),
                    )
                    .child(
                        IconButton::new("zoom_to_actual_size", IconName::GenericRestore)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.zoom_to_actual_size(&ZoomToActualSize, window, cx)
                            }))
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Zoom to Actual Size", &ZoomToActualSize, cx)
                            }),
                    )
                    .child(div().w_px().h_4().bg(cx.theme().styles.colors.border))
                    .child({
                        let eyedropper_mode = self.eyedropper_mode;
                        IconButton::new("eyedropper", IconName::Crosshair)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_eyedropper(&ToggleEyedropper, window, cx)
                            }))
                            .toggle_state(eyedropper_mode)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Color Picker", &ToggleEyedropper, cx)
                            })
                    })
                    .when_some(self.picked_color, |el, color| {
                        el.child(
                            div()
                                .id("color-swatch")
                                .w_6()
                                .h_6()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().styles.colors.border)
                                .bg(color.to_gpui_color()),
                        )
                        .child(
                            Label::new(color.to_hex())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            IconButton::new("copy_color", IconName::Copy)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.copy_color_to_clipboard(&CopyColorToClipboard, window, cx)
                                }))
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Copy Color", &CopyColorToClipboard, cx)
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .id("image-viewer-scroll-container")
                    .w_full()
                    .flex_1()
                    .overflow_scroll()
                    .track_scroll(&self.scroll_handle)
                    .on_scroll_wheel(cx.listener(
                        move |this, event: &ScrollWheelEvent, window, cx| {
                            if event.modifiers.secondary() || event.modifiers.control {
                                let delta = event.delta.pixel_delta(px(1.)).y;
                                let zoom_delta = if delta > px(0.) {
                                    ZOOM_STEP
                                } else if delta < px(0.) {
                                    -ZOOM_STEP
                                } else {
                                    return;
                                };

                                if this.zoom_mode == ZoomMode::Fit {
                                    this.zoom_level = this.calculate_fit_zoom(window, cx);
                                    this.zoom_mode = ZoomMode::Manual;
                                }

                                let old_zoom = this.zoom_level;
                                let new_zoom = (old_zoom + zoom_delta).clamp(MIN_ZOOM, MAX_ZOOM);

                                if new_zoom != old_zoom {
                                    let zoom_factor = new_zoom / old_zoom;
                                    let mouse_pos = event.position;
                                    let scroll_offset = this.scroll_handle.offset();

                                    let new_offset = scroll_offset * zoom_factor
                                        + mouse_pos * (zoom_factor - 1.0);

                                    this.zoom_level = new_zoom;
                                    this.scroll_handle.set_offset(new_offset);
                                    cx.notify();
                                }
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_center()
                            .items_center()
                            .size_full()
                            .min_w(scaled_width)
                            .min_h(scaled_height)
                            .child({
                                let eyedropper_mode = self.eyedropper_mode;
                                let image_item = self.image_item.clone();
                                div()
                                    .id("image-container")
                                    .flex_shrink_0()
                                    .relative()
                                    .w(scaled_width)
                                    .h(scaled_height)
                                    .when(eyedropper_mode, |el| el.cursor_crosshair())
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(
                                            move |this, event: &MouseDownEvent, window, cx| {
                                                if this.eyedropper_mode {
                                                    let image_data =
                                                        image_item.read(cx).image.clone();
                                                    if let Some(render_image) =
                                                        image_data.use_render_image(window, cx)
                                                    {
                                                        let image_size = render_image.size(0);
                                                        let img_width =
                                                            px(image_size.width.0 as f32);
                                                        let img_height =
                                                            px(image_size.height.0 as f32);

                                                        let zoom =
                                                            if this.zoom_mode == ZoomMode::Fit {
                                                                this.calculate_fit_zoom(window, cx)
                                                            } else {
                                                                this.zoom_level
                                                            };

                                                        let scaled_w = img_width * zoom;
                                                        let scaled_h = img_height * zoom;

                                                        let scroll_bounds =
                                                            this.scroll_handle.bounds();
                                                        let scroll_offset =
                                                            this.scroll_handle.offset();

                                                        let content_center_x =
                                                            (scroll_bounds.size.width - scaled_w)
                                                                .max(px(0.))
                                                                / 2.0;
                                                        let content_center_y =
                                                            (scroll_bounds.size.height - scaled_h)
                                                                .max(px(0.))
                                                                / 2.0;

                                                        let image_origin_x = scroll_bounds.origin.x
                                                            + content_center_x
                                                            - scroll_offset.x;
                                                        let image_origin_y = scroll_bounds.origin.y
                                                            + content_center_y
                                                            - scroll_offset.y;

                                                        let image_bounds = Bounds::new(
                                                            point(image_origin_x, image_origin_y),
                                                            size(scaled_w, scaled_h),
                                                        );

                                                        this.pick_color_at_position(
                                                            event.position,
                                                            &render_image,
                                                            image_bounds,
                                                            cx,
                                                        );
                                                    }
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ),
                                    )
                                    .child(
                                        canvas(|_, _, _| (), checkered_background)
                                            .border_2()
                                            .border_color(cx.theme().styles.colors.border)
                                            .size_full()
                                            .absolute()
                                            .top_0()
                                            .left_0(),
                                    )
                                    .child(
                                        img(image)
                                            .object_fit(ObjectFit::Fill)
                                            .size_full()
                                            .id("img"),
                                    )
                            }),
                    ),
            )
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Both).tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
    }
}

impl ProjectItem for ImageView {
    type Item = ImageItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<ImageView>(cx);
    workspace::register_serializable_item::<ImageView>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct ImageViewerDb(ThreadSafeConnection);

    impl Domain for ImageViewerDb {
        const NAME: &str = stringify!(ImageViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE image_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    image_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(IMAGE_VIEWER, ImageViewerDb, [WorkspaceDb]);

    impl ImageViewerDb {
        query! {
            pub async fn save_image_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                image_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO image_viewers(item_id, workspace_id, image_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_image_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT image_path
                FROM image_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
