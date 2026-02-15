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
use workspace::{Pane, SplitDirection, Workspace};
use zed_actions::call_hierarchy::{ShowCallHierarchy, ShowIncomingCalls, ShowOutgoingCalls};

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
    navigation_pane: Option<WeakEntity<Pane>>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,

    root: Option<CallHierarchyItem>,
    root_buffer: Option<Entity<language::Buffer>>,
    flat_entries: Vec<FlatEntry>,
    next_entry_id: usize,
    expanded: HashSet<usize>,
    selected_index: Option<usize>,
    direction: CallDirection,

    pending_loads: Vec<Task<Result<()>>>,
}

#[derive(Debug, Clone)]
struct FlatEntry {
    entry_id: usize,
    item: CallHierarchyItem,
    from_ranges: Vec<lsp::Range>,
    depth: usize,
    has_children: bool,
    node_index: usize,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowCallHierarchy, window, cx| {
            let navigation_pane = workspace.active_pane().downgrade();
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view_in_right_pane(
                workspace,
                navigation_pane,
                window,
                cx,
            );
            if let Some(active_editor) = active_editor {
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ShowIncomingCalls, window, cx| {
            let navigation_pane = workspace.active_pane().downgrade();
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view_in_right_pane(
                workspace,
                navigation_pane,
                window,
                cx,
            );
            if let Some(active_editor) = active_editor {
                view.update(cx, |view, _cx| {
                    view.direction = CallDirection::Incoming;
                });
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ShowOutgoingCalls, window, cx| {
            let navigation_pane = workspace.active_pane().downgrade();
            let active_editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            let view = open_or_focus_call_hierarchy_view_in_right_pane(
                workspace,
                navigation_pane,
                window,
                cx,
            );
            if let Some(active_editor) = active_editor {
                view.update(cx, |view, _cx| {
                    view.direction = CallDirection::Outgoing;
                });
                trigger_call_hierarchy(workspace, active_editor, view, window, cx);
            }
        });
    })
    .detach();
}

fn open_or_focus_call_hierarchy_view_in_right_pane(
    workspace: &mut Workspace,
    navigation_pane: WeakEntity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<CallHierarchyView> {
    if let Some(right_pane) = workspace.find_pane_in_direction(SplitDirection::Right, cx) {
        if let Some(existing) = right_pane
            .read(cx)
            .items_of_type::<CallHierarchyView>()
            .last()
        {
            existing.update(cx, |view, _cx| {
                view.navigation_pane = Some(navigation_pane.clone());
            });
            let is_active = workspace
                .active_item(cx)
                .is_some_and(|item| item.item_id() == existing.entity_id());
            workspace.activate_item(&existing, true, !is_active, window, cx);
            return existing;
        }

        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        let view = cx
            .new(|cx| CallHierarchyView::new(project, workspace_handle, Some(navigation_pane), cx));
        workspace.add_item(right_pane, view.boxed_clone(), None, true, true, window, cx);
        return view;
    }

    let workspace_handle = workspace.weak_handle();
    let project = workspace.project().clone();
    let view =
        cx.new(|cx| CallHierarchyView::new(project, workspace_handle, Some(navigation_pane), cx));
    workspace.split_item(SplitDirection::Right, view.boxed_clone(), window, cx);
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
    let source_file_path = buffer
        .read(cx)
        .file()
        .and_then(|file| file.as_local())
        .map(|file| file.abs_path(cx).to_path_buf());

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
        let Some(root_item) = select_call_hierarchy_item(items, position, source_file_path) else {
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
        navigation_pane: Option<WeakEntity<Pane>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            project,
            workspace,
            navigation_pane,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            root: None,
            root_buffer: None,
            flat_entries: Vec::new(),
            next_entry_id: 0,
            expanded: HashSet::default(),
            selected_index: None,
            direction: CallDirection::Incoming,
            pending_loads: Vec::new(),
        }
    }

    fn clear_results(&mut self, cx: &mut Context<Self>) {
        self.root = None;
        self.root_buffer = None;
        self.flat_entries.clear();
        self.next_entry_id = 0;
        self.expanded.clear();
        self.selected_index = None;
        self.pending_loads.clear();
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
        let root_entry_id = self.new_entry_id();
        self.root = Some(item.clone());
        self.root_buffer = Some(buffer);
        self.direction = direction;
        self.expanded.clear();
        self.expanded.insert(0);
        self.selected_index = Some(0);

        self.flat_entries = vec![FlatEntry {
            entry_id: root_entry_id,
            item,
            from_ranges: Vec::new(),
            depth: 0,
            has_children: true,
            node_index: 0,
        }];

        self.load_children(root_entry_id, window, cx);
    }

    fn new_entry_id(&mut self) -> usize {
        let entry_id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.saturating_add(1);
        entry_id
    }

    fn entry_index_by_id(&self, entry_id: usize) -> Option<usize> {
        self.flat_entries
            .iter()
            .position(|entry| entry.entry_id == entry_id)
    }

    fn load_children(
        &mut self,
        parent_entry_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry_index) = self.entry_index_by_id(parent_entry_id) else {
            return;
        };
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
                panel.insert_children(parent_entry_id, calls);
                cx.notify();
            })?;

            anyhow::Ok(())
        });

        self.pending_loads.push(task);
        cx.notify();
    }

    fn insert_children(
        &mut self,
        parent_entry_id: usize,
        children: Vec<(CallHierarchyItem, Vec<lsp::Range>)>,
    ) -> Vec<usize> {
        let Some(parent_index) = self.entry_index_by_id(parent_entry_id) else {
            return Vec::new();
        };
        if !self.expanded.contains(&parent_index) {
            return Vec::new();
        }

        if children.is_empty() {
            if let Some(parent_entry) = self.flat_entries.get_mut(parent_index) {
                parent_entry.has_children = false;
            }
            self.expanded.remove(&parent_index);
            return Vec::new();
        }

        let parent_depth = self
            .flat_entries
            .get(parent_index)
            .map(|e| e.depth)
            .unwrap_or(0);

        let ancestor_keys: HashSet<(String, lsp::Range)> = self
            .ancestor_items(parent_index)
            .map(|item| (item.uri.to_string(), item.selection_range))
            .collect();

        let insert_at = parent_index + 1;
        let old_child_count = self.count_visible_descendants(parent_index);
        self.remove_entry_range(insert_at, old_child_count, parent_index);

        let mut inserted_entry_ids = Vec::with_capacity(children.len());
        let mut entries_to_insert = Vec::with_capacity(children.len());
        for (index, (item, from_ranges)) in children.into_iter().enumerate() {
            let is_cycle = ancestor_keys.contains(&(item.uri.to_string(), item.selection_range));
            let entry_id = self.new_entry_id();
            inserted_entry_ids.push(entry_id);
            entries_to_insert.push(FlatEntry {
                entry_id,
                item,
                from_ranges,
                depth: parent_depth + 1,
                has_children: !is_cycle,
                node_index: insert_at + index,
            });
        }

        let inserted_entry_count = entries_to_insert.len();
        self.flat_entries
            .splice(insert_at..insert_at, entries_to_insert);

        self.remap_indices_for_insert(insert_at, inserted_entry_count);
        self.reindex_entries();
        inserted_entry_ids
    }

    fn ancestor_items(&self, index: usize) -> impl Iterator<Item = &CallHierarchyItem> {
        let target_depth = self.flat_entries.get(index).map(|e| e.depth).unwrap_or(0);
        let mut current_depth = target_depth;
        self.flat_entries[..=index]
            .iter()
            .rev()
            .filter(move |entry| {
                if entry.depth < current_depth {
                    current_depth = entry.depth;
                    true
                } else {
                    entry.depth == target_depth && entry.node_index == index
                }
            })
            .map(|entry| &entry.item)
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
            let entry_id = self.flat_entries[index].entry_id;
            self.load_children(entry_id, window, cx);
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

        let start = entry
            .from_ranges
            .first()
            .map(|range| range.start)
            .unwrap_or(item.selection_range.start);
        let row = start.line;
        let column = start.character;

        workspace.update(cx, |workspace, cx| {
            let path = uri.to_file_path().ok();
            if let Some(path) = path {
                let project = workspace.project().clone();
                let target_pane = self.navigation_pane.clone();
                let project_path_task = Workspace::project_path_for_path(project, &path, true, cx);

                cx.spawn_in(window, async move |workspace, cx| {
                    let (_, project_path) = project_path_task.await?;
                    let open_task = workspace.update_in(cx, |workspace, window, cx| {
                        workspace.open_path(project_path, target_pane, true, window, cx)
                    })?;

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
                    if event.click_count() >= 2 {
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
                            if let (Some(root), Some(buffer)) =
                                (this.root.clone(), this.root_buffer.clone())
                            {
                                this.direction = CallDirection::Incoming;
                                this.set_root(root, buffer, CallDirection::Incoming, window, cx);
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
                            if let (Some(root), Some(buffer)) =
                                (this.root.clone(), this.root_buffer.clone())
                            {
                                this.direction = CallDirection::Outgoing;
                                this.set_root(root, buffer, CallDirection::Outgoing, window, cx);
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
        key_context.add("CallHierarchy");

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

fn select_call_hierarchy_item(
    items: Vec<lsp::CallHierarchyItem>,
    requested_position: language::PointUtf16,
    source_file_path: Option<std::path::PathBuf>,
) -> Option<lsp::CallHierarchyItem> {
    if items.is_empty() {
        return None;
    }

    let requested_position = lsp::Position {
        line: requested_position.row,
        character: requested_position.column,
    };

    if let Some(best) = items
        .iter()
        .filter(|item| {
            source_file_path
                .as_ref()
                .is_none_or(|source_file_path| lsp_item_is_in_file(item, source_file_path))
        })
        .filter(|item| lsp_range_contains_position(&item.selection_range, &requested_position))
        .min_by_key(|item| {
            let range = &item.selection_range;
            (
                range.end.line.saturating_sub(range.start.line),
                if range.end.line == range.start.line {
                    range.end.character.saturating_sub(range.start.character)
                } else {
                    range.end.character
                },
                range.start.line.abs_diff(requested_position.line),
                range.start.character.abs_diff(requested_position.character),
            )
        })
    {
        return Some(best.clone());
    }

    items.into_iter().next()
}

fn lsp_range_contains_position(range: &lsp::Range, position: &lsp::Position) -> bool {
    range.start <= *position && *position < range.end
}

fn lsp_item_is_in_file(
    item: &lsp::CallHierarchyItem,
    source_file_path: &std::path::PathBuf,
) -> bool {
    item.uri
        .to_file_path()
        .is_ok_and(|item_path| item_path == *source_file_path)
}
