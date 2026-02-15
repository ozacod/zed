use anyhow::Result;
use collections::HashSet;
use editor::Editor;
use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyContext, ListHorizontalSizingBehavior, ListSizingBehavior,
    ParentElement, Render, ScrollStrategy, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled, Task, UniformListScrollHandle, WeakEntity, Window, actions, div, px, uniform_list,
};
use language::ToPointUtf16 as _;
use lsp::CallHierarchyItem;
use menu::{SelectNext, SelectPrevious};
use project::Project;
use ui::{
    FluentBuilder, Icon, IconButton, IconButtonShape, IconName, Label, ListItem, Tooltip,
    prelude::*,
};
use workspace::item::{Item, ItemHandle};
use workspace::{OpenOptions, SplitDirection, Workspace};
use zed_actions::call_hierarchy::{
    ShowCallHierarchy, ShowIncomingCalls, ShowOutgoingCalls, ToggleFocus,
};

actions!(
    call_hierarchy,
    [
        /// Expands the currently selected entry.
        ExpandSelectedEntry,
        /// Collapses the currently selected entry.
        CollapseSelectedEntry,
        /// Opens the currently selected entry in the editor.
        OpenSelectedEntry,
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallDirection {
    Incoming,
    Outgoing,
}

pub struct CallHierarchyView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,

    root: Option<CallHierarchyItem>,
    root_buffer: Option<Entity<language::Buffer>>,
    flat_entries: Vec<FlatEntry>,
    expanded: HashSet<usize>,
    selected_index: Option<usize>,
    direction: CallDirection,

    pending_load: Option<Task<Result<()>>>,
}

#[derive(Debug, Clone)]
struct FlatEntry {
    item: CallHierarchyItem,
    from_ranges: Vec<lsp::Range>,
    depth: usize,
    has_children: bool,
    node_index: usize,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            open_or_focus_call_hierarchy_view(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowCallHierarchy, window, cx| {
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view(workspace, window, cx);
            if let Some(active_editor) = active_editor {
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ShowIncomingCalls, window, cx| {
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view(workspace, window, cx);
            view.update(cx, |view, _cx| {
                view.direction = CallDirection::Incoming;
            });
            if let Some(active_editor) = active_editor {
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ShowOutgoingCalls, window, cx| {
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view(workspace, window, cx);
            view.update(cx, |view, _cx| {
                view.direction = CallDirection::Outgoing;
            });
            if let Some(active_editor) = active_editor {
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
    })
    .detach();
}

fn open_or_focus_call_hierarchy_view(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<CallHierarchyView> {
    if let Some(existing) = workspace.item_of_type::<CallHierarchyView>(cx) {
        let is_active = workspace
            .active_item(cx)
            .is_some_and(|item| item.item_id() == existing.entity_id());
        workspace.activate_item(&existing, true, !is_active, window, cx);
        return existing;
    }

    let workspace_handle = workspace.weak_handle();
    let project = workspace.project().clone();
    let view = cx.new(|cx| CallHierarchyView::new(project, workspace_handle, cx));
    match workspace.find_pane_in_direction(SplitDirection::Right, cx) {
        Some(right_pane) => {
            workspace.add_item(right_pane, view.boxed_clone(), None, true, true, window, cx);
        }
        None => {
            workspace.split_item(SplitDirection::Right, view.boxed_clone(), window, cx);
        }
    }
    view
}

fn trigger_call_hierarchy(
    workspace: &mut Workspace,
    editor: Entity<Editor>,
    view: Entity<CallHierarchyView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let result = editor.update(cx, |editor, cx| {
        let (_, buffer, range) = editor.active_excerpt(cx)?;
        let snapshot = buffer.read(cx).snapshot();
        let head = editor.selections.newest_anchor().head();
        let position = head.text_anchor.to_point_utf16(&snapshot);
        Some((buffer, position, range))
    });

    let Some((buffer, position, _range)) = result else {
        return;
    };

    let project = workspace.project().clone();
    let direction = view.read(cx).direction;

    let prepare_task = project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            lsp_store.prepare_call_hierarchy(&buffer, position, cx)
        })
    });

    let view_clone = view.clone();
    cx.spawn_in(window, async move |_workspace, cx| {
        let items = prepare_task.await?;
        let Some(root_item) = items.into_iter().next() else {
            view_clone.update_in(cx, |view, _window, cx| {
                view.clear_results(cx);
            })?;
            return anyhow::Ok(());
        };

        view_clone.update_in(cx, |view, window, cx| {
            view.set_root(root_item, buffer, direction, window, cx);
        })?;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

impl CallHierarchyView {
    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            root: None,
            root_buffer: None,
            flat_entries: Vec::new(),
            expanded: HashSet::default(),
            selected_index: None,
            direction: CallDirection::Incoming,
            pending_load: None,
        }
    }

    fn clear_results(&mut self, cx: &mut Context<Self>) {
        self.root = None;
        self.root_buffer = None;
        self.flat_entries.clear();
        self.expanded.clear();
        self.selected_index = None;
        self.pending_load = None;
        cx.notify();
    }

    fn set_root(
        &mut self,
        item: CallHierarchyItem,
        buffer: Entity<language::Buffer>,
        direction: CallDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.root = Some(item.clone());
        self.root_buffer = Some(buffer);
        self.direction = direction;
        self.expanded.clear();
        self.expanded.insert(0);
        self.selected_index = Some(0);

        self.flat_entries = vec![FlatEntry {
            item,
            from_ranges: Vec::new(),
            depth: 0,
            has_children: true,
            node_index: 0,
        }];

        self.load_children(0, window, cx);
    }

    fn load_children(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.flat_entries.get(entry_index).cloned() else {
            return;
        };
        let Some(buffer) = self.root_buffer.clone() else {
            return;
        };

        let item = entry.item.clone();
        let direction = self.direction;
        let project = self.project.clone();

        let calls_task: Task<Result<Vec<(CallHierarchyItem, Vec<lsp::Range>)>>> = match direction {
            CallDirection::Incoming => {
                let lsp_task = project.update(cx, |project, cx| {
                    project.lsp_store().update(cx, |lsp_store, cx| {
                        lsp_store.incoming_calls(item.clone(), &buffer, cx)
                    })
                });
                cx.background_spawn(async move {
                    let results = lsp_task.await?;
                    Ok(results
                        .into_iter()
                        .map(|call| (call.from, call.from_ranges))
                        .collect())
                })
            }
            CallDirection::Outgoing => {
                let lsp_task = project.update(cx, |project, cx| {
                    project.lsp_store().update(cx, |lsp_store, cx| {
                        lsp_store.outgoing_calls(item.clone(), &buffer, cx)
                    })
                });
                cx.background_spawn(async move {
                    let results = lsp_task.await?;
                    Ok(results
                        .into_iter()
                        .map(|call| (call.to, call.from_ranges))
                        .collect())
                })
            }
        };

        let task = cx.spawn_in(window, async move |this, cx| {
            let calls = calls_task.await?;

            this.update_in(cx, |panel, _window, cx| {
                panel.insert_children(entry_index, calls);
                cx.notify();
            })?;

            anyhow::Ok(())
        });

        self.pending_load = Some(task);
        cx.notify();
    }

    fn insert_children(
        &mut self,
        parent_index: usize,
        children: Vec<(CallHierarchyItem, Vec<lsp::Range>)>,
    ) {
        let parent_depth = self
            .flat_entries
            .get(parent_index)
            .map(|e| e.depth)
            .unwrap_or(0);

        let insert_at = parent_index + 1;
        let old_child_count = self.count_visible_descendants(parent_index);
        self.remove_entry_range(insert_at, old_child_count, parent_index);

        let entries_to_insert: Vec<FlatEntry> = children
            .into_iter()
            .enumerate()
            .map(|(index, (item, from_ranges))| FlatEntry {
                item,
                from_ranges,
                depth: parent_depth + 1,
                has_children: true,
                node_index: insert_at + index,
            })
            .collect();

        let inserted_entry_count = entries_to_insert.len();
        self.flat_entries
            .splice(insert_at..insert_at, entries_to_insert);

        self.remap_indices_for_insert(insert_at, inserted_entry_count);
        self.reindex_entries();
    }

    fn reindex_entries(&mut self) {
        for (index, entry) in self.flat_entries.iter_mut().enumerate() {
            entry.node_index = index;
        }
    }

    fn count_visible_descendants(&self, parent_index: usize) -> usize {
        let parent_depth = self
            .flat_entries
            .get(parent_index)
            .map(|e| e.depth)
            .unwrap_or(0);
        self.flat_entries
            .iter()
            .skip(parent_index + 1)
            .take_while(|e| e.depth > parent_depth)
            .count()
    }

    fn remove_entry_range(
        &mut self,
        remove_start: usize,
        remove_count: usize,
        fallback_selected_index: usize,
    ) {
        if remove_count == 0 {
            return;
        }

        let remove_end = remove_start + remove_count;
        self.flat_entries.drain(remove_start..remove_end);

        self.expanded = self
            .expanded
            .iter()
            .filter_map(|index| {
                if *index < remove_start {
                    Some(*index)
                } else if *index < remove_end {
                    None
                } else {
                    Some(*index - remove_count)
                }
            })
            .collect();

        self.selected_index = self.selected_index.and_then(|selected_index| {
            if selected_index < remove_start {
                Some(selected_index)
            } else if selected_index < remove_end {
                if self.flat_entries.is_empty() {
                    None
                } else {
                    Some(fallback_selected_index.min(self.flat_entries.len() - 1))
                }
            } else {
                Some(selected_index - remove_count)
            }
        });
    }

    fn remap_indices_for_insert(&mut self, insert_at: usize, inserted_count: usize) {
        if inserted_count == 0 {
            return;
        }

        self.expanded = self
            .expanded
            .iter()
            .map(|index| {
                if *index >= insert_at {
                    *index + inserted_count
                } else {
                    *index
                }
            })
            .collect();

        self.selected_index = self.selected_index.map(|selected_index| {
            if selected_index >= insert_at {
                selected_index + inserted_count
            } else {
                selected_index
            }
        });
    }

    fn toggle_expanded(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.expanded.contains(&index) {
            self.expanded.remove(&index);
            let count = self.count_visible_descendants(index);
            let remove_start = index + 1;
            self.remove_entry_range(remove_start, count, index);
            self.reindex_entries();
            cx.notify();
        } else {
            self.expanded.insert(index);
            self.load_children(index, window, cx);
        }
    }

    fn select_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.flat_entries.len() {
            self.selected_index = Some(index);
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let next = self
            .selected_index
            .map(|i| (i + 1).min(self.flat_entries.len().saturating_sub(1)))
            .unwrap_or(0);
        self.select_entry(next, cx);
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = self
            .selected_index
            .map(|i| i.saturating_sub(1))
            .unwrap_or(0);
        self.select_entry(prev, cx);
    }

    fn open_selected_entry(
        &mut self,
        _: &OpenSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.selected_index else {
            return;
        };
        self.navigate_to_entry(index, window, cx);
    }

    fn navigate_to_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.flat_entries.get(index) else {
            return;
        };
        let item = entry.item.clone();
        let uri = item.uri.clone();

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let start = item.selection_range.start;
        let row = start.line;
        let column = start.character;

        workspace.update(cx, |workspace, cx| {
            let path = uri.to_file_path().ok();
            if let Some(path) = path {
                let open_task = workspace.open_abs_path(path, OpenOptions::default(), window, cx);
                cx.spawn_in(window, async move |_workspace, cx| {
                    let item = open_task.await?;
                    if let Some(editor) = item.downcast::<Editor>() {
                        editor.update_in(cx, |editor, window, cx| {
                            let point = language::Point::new(row, column);
                            editor.go_to_singleton_buffer_point(point, window, cx);
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
        });
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.selected_index {
            if !self.expanded.contains(&index) {
                self.toggle_expanded(index, window, cx);
            }
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.selected_index {
            if self.expanded.contains(&index) {
                self.expanded.remove(&index);
                let count = self.count_visible_descendants(index);
                let remove_start = index + 1;
                self.remove_entry_range(remove_start, count, index);
                self.reindex_entries();
                cx.notify();
            }
        }
    }

    fn render_entry(
        &self,
        entry: &FlatEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let index = entry.node_index;
        let is_selected = self.selected_index == Some(index);
        let is_expanded = self.expanded.contains(&index);
        let item_name: SharedString = entry.item.name.clone().into();
        let detail: SharedString = entry.item.detail.clone().unwrap_or_default().into();
        let call_count = entry.from_ranges.len();

        let icon = symbol_icon(entry.item.kind);
        let has_children = entry.has_children;

        div()
            .id(ElementId::NamedInteger(
                "call-hierarchy-entry".into(),
                index as u64,
            ))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.select_entry(index, cx);
                    if event.click_count() > 1 {
                        this.navigate_to_entry(index, window, cx);
                    }
                }),
            )
            .cursor_pointer()
            .child(
                ListItem::new(ElementId::NamedInteger(
                    "call-hierarchy-list-item".into(),
                    index as u64,
                ))
                .indent_level(entry.depth)
                .indent_step_size(px(20.))
                .toggle_state(is_selected)
                .when(has_children, |item| {
                    item.on_toggle(cx.listener(move |this, _event, window, cx| {
                        this.toggle_expanded(index, window, cx);
                    }))
                    .toggle(is_expanded)
                })
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(Label::new(item_name))
                        .when(!detail.is_empty(), |flex| {
                            flex.child(
                                Label::new(detail)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                        })
                        .when(call_count > 0, |flex| {
                            flex.child(
                                Label::new(format!("({call_count})"))
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                        }),
                ),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let direction = self.direction;

        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("incoming", IconName::ArrowDown)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .toggle_state(direction == CallDirection::Incoming)
                    .tooltip(Tooltip::text("Incoming Calls"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.direction != CallDirection::Incoming {
                            this.direction = CallDirection::Incoming;
                            if let Some(root) = this.root.clone() {
                                if let Some(buffer) = this.root_buffer.clone() {
                                    this.set_root(
                                        root,
                                        buffer,
                                        CallDirection::Incoming,
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }
                    })),
            )
            .child(
                IconButton::new("outgoing", IconName::ArrowUp)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .toggle_state(direction == CallDirection::Outgoing)
                    .tooltip(Tooltip::text("Outgoing Calls"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.direction != CallDirection::Outgoing {
                            this.direction = CallDirection::Outgoing;
                            if let Some(root) = this.root.clone() {
                                if let Some(buffer) = this.root_buffer.clone() {
                                    this.set_root(
                                        root,
                                        buffer,
                                        CallDirection::Outgoing,
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }
                    })),
            )
            .when(self.root.is_some(), |header| {
                let name: SharedString = self
                    .root
                    .as_ref()
                    .map(|r| r.name.clone())
                    .unwrap_or_default()
                    .into();
                header.child(Label::new(name).size(LabelSize::Small).color(Color::Muted))
            })
    }

    fn render_entries(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.flat_entries.is_empty() {
            return v_flex()
                .size_full()
                .p_2()
                .gap_1()
                .items_start()
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(Icon::new(IconName::ListTree).size(IconSize::XSmall))
                        .child(
                            Label::new("No call hierarchy results")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
                .into_any_element();
        }

        let entries_len = self.flat_entries.len();
        uniform_list(
            "call-hierarchy-entries",
            entries_len,
            cx.processor(move |panel, range: std::ops::Range<usize>, window, cx| {
                let entries = panel.flat_entries[range.clone()].to_vec();
                entries
                    .iter()
                    .map(|entry| panel.render_entry(entry, window, cx))
                    .collect()
            }),
        )
        .size_full()
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.scroll_handle)
        .into_any_element()
    }
}

impl Focusable for CallHierarchyView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for CallHierarchyView {}

impl Item for CallHierarchyView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Call Hierarchy".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("call hierarchy: open")
    }
}

impl Render for CallHierarchyView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("CallHierarchyView");
        key_context.add("CallHierarchyPanel");

        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_selected_entry))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_entries(window, cx))
    }
}

fn symbol_icon(kind: lsp::SymbolKind) -> IconName {
    match kind {
        lsp::SymbolKind::FUNCTION | lsp::SymbolKind::METHOD => IconName::Code,
        lsp::SymbolKind::CLASS | lsp::SymbolKind::STRUCT => IconName::Blocks,
        lsp::SymbolKind::INTERFACE => IconName::Blocks,
        lsp::SymbolKind::MODULE | lsp::SymbolKind::NAMESPACE => IconName::Folder,
        lsp::SymbolKind::CONSTRUCTOR => IconName::Code,
        lsp::SymbolKind::ENUM => IconName::Blocks,
        lsp::SymbolKind::PROPERTY | lsp::SymbolKind::FIELD => IconName::Dash,
        lsp::SymbolKind::VARIABLE | lsp::SymbolKind::CONSTANT => IconName::Dash,
        _ => IconName::Code,
    }
}
