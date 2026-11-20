use editor::Editor;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyContext, KeyDownEvent, ListSizingBehavior,
    MouseButton, Pixels, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, div, px, relative, uniform_list,
};
use language::{Capability, LanguageServerId};
use project::{
    Project,
    java_dependencies::{
        JavaDependencies, JavaDependenciesEvent, JavaDependencyRow, JavaDependencyRowKind,
    },
    lsp_store::LspStoreEvent,
};
use settings::{ProjectPanelEntrySpacing, Settings};
use ui::{ListItem, ListItemSpacing, LoadingLabel, Scrollbars, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt;
use workspace::Workspace;

use crate::project_panel_settings::{ProjectPanelScrollbarProxy, ProjectPanelSettings};

pub(super) struct ExternalLibraries {
    store: Entity<JavaDependencies>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    enabled: bool,
    panel_visible: bool,
    expanded: bool,
    height: Pixels,
    selection: Option<(u64, JavaDependencyRowKind)>,
    opening: Option<u64>,
    open_task: Option<Task<()>>,
    open_error: Option<(u64, SharedString)>,
    _subscriptions: Vec<Subscription>,
}

impl ExternalLibraries {
    pub(super) fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let lsp_store = project.read(cx).lsp_store();
        let lsp_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| {
            if let LspStoreEvent::JavaDependencyDocumentOpened { server_id, jdt_uri } = event {
                this.reveal_document(*server_id, jdt_uri.clone(), cx);
            }
        });
        let store = cx.new(|cx| JavaDependencies::new(project, cx));
        let reveal_subscription = cx.subscribe(&store, |this, _, event, cx| {
            let JavaDependenciesEvent::Revealed { id, kind } = event;
            this.selection = Some((*id, *kind));
            if let Some(index) = this
                .store
                .read(cx)
                .rows()
                .iter()
                .position(|row| row.id == *id && row.kind == *kind)
            {
                this.scroll_handle
                    .scroll_to_item(index, ScrollStrategy::Center);
            }
            cx.notify();
        });
        cx.observe(&store, |this, store, cx| {
            if let Some((id, kind)) = this.selection {
                let rows = store.read(cx).rows();
                if !rows.iter().any(|row| row.id == id && row.kind == kind) {
                    this.selection = rows
                        .iter()
                        .find(|row| row.id == id && Self::is_actionable(row.kind))
                        .map(|row| (row.id, row.kind));
                }
            }
            cx.notify();
        })
        .detach();

        Self {
            store,
            workspace,
            focus_handle: cx.focus_handle().tab_stop(true),
            scroll_handle: UniformListScrollHandle::new(),
            enabled: ProjectPanelSettings::get_global(cx).show_external_libraries,
            panel_visible: false,
            expanded: false,
            height: px(240.),
            selection: None,
            opening: None,
            open_task: None,
            open_error: None,
            _subscriptions: vec![lsp_subscription, reveal_subscription],
        }
    }

    fn reveal_document(
        &mut self,
        server_id: LanguageServerId,
        jdt_uri: String,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled || !self.panel_visible {
            return;
        }
        self.expanded = true;
        self.sync_visibility(cx);
        self.store
            .update(cx, |store, cx| store.reveal(server_id, jdt_uri, cx));
    }

    pub(super) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.sync_visibility(cx);
            cx.notify();
        }
    }

    pub(super) fn set_panel_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.panel_visible != visible {
            self.panel_visible = visible;
            self.sync_visibility(cx);
            cx.notify();
        }
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.enabled && self.panel_visible {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    pub(super) fn resize(&mut self, height: Pixels, cx: &mut Context<Self>) {
        if self.height != height {
            self.height = height;
            cx.notify();
        }
    }

    fn query_visible(&self) -> bool {
        self.enabled && self.panel_visible && self.expanded
    }

    fn cancel_open(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.opening.take() {
            self.store.update(cx, |store, cx| store.cancel_open(id, cx));
        }
        self.open_task = None;
    }

    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        let visible = self.query_visible();
        if !visible {
            self.cancel_open(cx);
        }
        self.store
            .update(cx, |store, cx| store.set_visible(visible, cx));
    }

    fn toggle_section(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        self.selection = None;
        self.sync_visibility(cx);
        cx.notify();
    }

    fn is_container(kind: JavaDependencyRowKind) -> bool {
        matches!(
            kind,
            JavaDependencyRowKind::Project
                | JavaDependencyRowKind::Jar
                | JavaDependencyRowKind::Package
        )
    }

    fn is_actionable(kind: JavaDependencyRowKind) -> bool {
        !matches!(
            kind,
            JavaDependencyRowKind::Loading | JavaDependencyRowKind::Info
        )
    }

    fn selected_index(&self, cx: &App) -> Option<usize> {
        let (id, kind) = self.selection?;
        self.store
            .read(cx)
            .rows()
            .iter()
            .position(|row| row.id == id && row.kind == kind)
    }

    fn select_index(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        self.selection = index
            .and_then(|index| self.store.read(cx).rows().get(index))
            .map(|row| (row.id, row.kind));
        if let Some(index) = index {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.expanded {
            return;
        }
        let rows = self.store.read(cx).rows();
        let selected_index = self.selected_index(cx);
        let index = if forward {
            let start = selected_index.map_or(0, |index| index + 1);
            rows.iter()
                .enumerate()
                .skip(start)
                .find(|(_, row)| Self::is_actionable(row.kind))
                .map(|(index, _)| index)
                .or(selected_index)
        } else {
            selected_index.and_then(|index| {
                rows.get(..index)?
                    .iter()
                    .rposition(|row| Self::is_actionable(row.kind))
            })
        };
        self.select_index(index, cx);
    }

    fn activate(
        &mut self,
        id: u64,
        kind: JavaDependencyRowKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.query_visible() {
            return;
        }
        match kind {
            JavaDependencyRowKind::Project
            | JavaDependencyRowKind::Jar
            | JavaDependencyRowKind::Package => {
                self.store.update(cx, |store, cx| store.toggle(id, cx));
            }
            JavaDependencyRowKind::Error => {
                self.store.update(cx, |store, cx| store.retry(id, cx));
            }
            JavaDependencyRowKind::LoadMore => {
                self.store.update(cx, |store, cx| store.load_more(id, cx));
            }
            JavaDependencyRowKind::Class => self.open_class(id, window, cx),
            JavaDependencyRowKind::Loading | JavaDependencyRowKind::Info => {}
        }
    }

    fn open_class(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query_visible() || self.opening == Some(id) {
            return;
        }
        self.cancel_open(cx);
        self.opening = Some(id);
        self.open_error = None;
        let task = self.store.update(cx, |store, cx| store.open(id, cx));
        let workspace = self.workspace.clone();
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let buffer = task.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_capability(Capability::ReadOnly, cx);
                    });
                    let editor = workspace.open_project_item::<Editor>(
                        None, buffer, true, true, false, true, window, cx,
                    );
                    editor.update(cx, |editor, cx| {
                        editor.set_read_only(true);
                        cx.notify();
                    });
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.opening = None;
                this.open_task = None;
                if let Err(error) = result {
                    log::error!("Failed to open Java dependency: {error:#}");
                    this.open_error = Some((id, format!("Failed to open class: {error:#}").into()));
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn activate_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, kind)) = self.selection {
            self.activate(id, kind, window, cx);
        } else {
            self.toggle_section(cx);
        }
    }

    fn select_child(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index(cx) {
            let rows = self.store.read(cx).rows();
            if let Some(row) = rows.get(index) {
                if Self::is_container(row.kind) && !row.expanded {
                    let id = row.id;
                    self.store.update(cx, |store, cx| store.toggle(id, cx));
                } else if Self::is_container(row.kind) {
                    let child_index = rows
                        .get(index + 1)
                        .filter(|child| child.depth > row.depth && Self::is_actionable(child.kind))
                        .map(|_| index + 1);
                    if child_index.is_some() {
                        self.select_index(child_index, cx);
                    }
                }
            }
        } else if !self.expanded {
            self.toggle_section(cx);
        } else {
            self.move_selection(true, cx);
        }
    }

    fn select_parent(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index(cx) {
            let rows = self.store.read(cx).rows();
            if let Some(row) = rows.get(index) {
                if Self::is_container(row.kind) && row.expanded {
                    let id = row.id;
                    self.store.update(cx, |store, cx| store.toggle(id, cx));
                } else {
                    let parent_index = rows.get(..index).and_then(|rows| {
                        rows.iter().rposition(|parent| {
                            parent.depth < row.depth && Self::is_container(parent.kind)
                        })
                    });
                    self.select_index(parent_index, cx);
                }
            }
        } else if self.expanded {
            self.toggle_section(cx);
        }
    }

    fn select_last(&mut self, cx: &mut Context<Self>) {
        if self.expanded {
            let index = self
                .store
                .read(cx)
                .rows()
                .iter()
                .rposition(|row| Self::is_actionable(row.kind));
            self.select_index(index, cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.move_selection(true, cx),
            "up" => self.move_selection(false, cx),
            "enter" | "space" => self.activate_selection(window, cx),
            "right" => self.select_child(cx),
            "left" => self.select_parent(cx),
            "home" => self.select_index(None, cx),
            "end" => self.select_last(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_row(
        &self,
        index: usize,
        row: JavaDependencyRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = row.id;
        let kind = row.kind;
        let actionable = Self::is_actionable(kind);
        let loading = row.loading || self.opening == Some(id);
        let icon = match kind {
            JavaDependencyRowKind::Project => IconName::FileTree,
            JavaDependencyRowKind::Jar => IconName::Archive,
            JavaDependencyRowKind::Package => IconName::Box,
            JavaDependencyRowKind::Class => IconName::Code,
            JavaDependencyRowKind::Loading => IconName::Ellipsis,
            JavaDependencyRowKind::Error => IconName::Warning,
            JavaDependencyRowKind::LoadMore => IconName::ArrowDown,
            JavaDependencyRowKind::Info => IconName::Info,
        };
        let tooltip = row.detail.as_ref().map_or_else(
            || row.name.clone(),
            |detail| format!("{}\n{}", row.name, detail).into(),
        );
        let settings = ProjectPanelSettings::get_global(cx);
        ListItem::new(("external-library-row", index))
            .inset(false)
            .height(px(28.))
            .spacing(match settings.entry_spacing {
                ProjectPanelEntrySpacing::Comfortable => ListItemSpacing::Dense,
                ProjectPanelEntrySpacing::Standard => ListItemSpacing::ExtraDense,
            })
            .indent_level(row.depth + 1)
            .indent_step_size(px(settings.indent_size))
            .always_show_disclosure_icon(true)
            .selectable(actionable)
            .toggle_state(self.selection == Some((id, kind)))
            .focused(
                self.selection == Some((id, kind))
                    && self.focus_handle.contains_focused(window, cx),
            )
            .when(Self::is_container(kind), |item| item.toggle(row.expanded))
            .start_slot(Icon::new(icon).size(IconSize::Small).color(
                if kind == JavaDependencyRowKind::Error {
                    Color::Error
                } else {
                    Color::Muted
                },
            ))
            .child(div().flex_1().min_w_0().overflow_hidden().map(|element| {
                if kind == JavaDependencyRowKind::Loading {
                    element.child(LoadingLabel::new(row.name).size(LabelSize::Small))
                } else {
                    element.child(Label::new(row.name).size(LabelSize::Small).truncate())
                }
            }))
            .when(loading && kind != JavaDependencyRowKind::Loading, |item| {
                item.end_slot(LoadingLabel::new("Loading").size(LabelSize::Small))
            })
            .when(kind == JavaDependencyRowKind::Error, |item| {
                item.end_slot(
                    Label::new("Retry")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(kind == JavaDependencyRowKind::LoadMore, |item| {
                item.end_slot(Icon::new(IconName::ArrowDown).size(IconSize::Small))
            })
            .tooltip(Tooltip::text(tooltip))
            .when(actionable, |item| {
                item.on_click(cx.listener(move |this, _, window, cx| {
                    this.selection = Some((id, kind));
                    this.focus_handle.focus(window, cx);
                    this.activate(id, kind, window, cx);
                    cx.notify();
                }))
            })
    }
}

impl Focusable for ExternalLibraries {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub(super) struct ResizeExternalLibraries;

impl Render for ResizeExternalLibraries {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Render for ExternalLibraries {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.store.read(cx).rows().len();
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("ExternalLibraries");
        key_context.add("menu");
        v_flex()
            .id("external-libraries")
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                this.move_selection(true, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.move_selection(false, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectFirst, _, cx| {
                this.select_index(None, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectLast, _, cx| {
                this.select_last(cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectChild, _, cx| {
                this.select_child(cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectParent, _, cx| {
                this.select_parent(cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.activate_selection(window, cx);
            }))
            .on_key_down(cx.listener(Self::key_down))
            .w_full()
            .flex_none()
            .max_h(relative(0.65))
            .overflow_hidden()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .when(self.expanded, |section| {
                section.h(self.height).child(
                    div()
                        .id("resize-external-libraries")
                        .w_full()
                        .h(px(4.))
                        .flex_none()
                        .cursor_row_resize()
                        .hover(|style| style.bg(cx.theme().colors().panel_focused_border))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_drag(ResizeExternalLibraries, |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| ResizeExternalLibraries)
                        }),
                )
            })
            .child(
                h_flex()
                    .h_7()
                    .flex_none()
                    .px_1()
                    .child(
                        h_flex()
                            .id("toggle-external-libraries")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .gap_1()
                            .px_1()
                            .cursor_pointer()
                            .when(
                                self.selection.is_none() && self.focus_handle.is_focused(window),
                                |title| title.bg(cx.theme().colors().element_selected),
                            )
                            .child(
                                Icon::new(if self.expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            )
                            .child(
                                Label::new("External Libraries")
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .tooltip(|_, cx| {
                                Tooltip::with_meta(
                                    "External Libraries",
                                    Some(&super::FocusExternalLibraries),
                                    "Command Palette: Project Panel: Focus External Libraries\nReturn to files: Project Panel: Focus File Tree",
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.focus_handle.focus(window, cx);
                                this.toggle_section(cx);
                            })),
                    )
                    .child(
                        IconButton::new("refresh-external-libraries", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(!self.query_visible())
                            .tooltip(Tooltip::text("Refresh External Libraries"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.query_visible() {
                                    this.cancel_open(cx);
                                    this.open_error = None;
                                    this.selection = None;
                                    this.store.update(cx, |store, cx| store.refresh(cx));
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .when(self.expanded, |section| {
                section
                    .when_some(self.open_error.clone(), |section, (id, message)| {
                        section.child(
                            h_flex()
                                .px_2()
                                .gap_1()
                                .flex_none()
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(message.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Error)
                                            .truncate(),
                                    ),
                                )
                                .child(
                                    Button::new("retry-open-class", "Retry")
                                        .style(ButtonStyle::Subtle)
                                        .tooltip(Tooltip::text(message))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_class(id, window, cx)
                                        })),
                                ),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .child(
                                uniform_list(
                                    "external-library-rows",
                                    row_count,
                                    cx.processor(
                                        |this, range: std::ops::Range<usize>, window, cx| {
                                            range
                                                .filter_map(|index| {
                                                    let row = this
                                                        .store
                                                        .read(cx)
                                                        .rows()
                                                        .get(index)?
                                                        .clone();
                                                    Some(
                                                        this.render_row(index, row, window, cx)
                                                            .into_any_element(),
                                                    )
                                                })
                                                .collect()
                                        },
                                    ),
                                )
                                .with_sizing_behavior(ListSizingBehavior::Auto)
                                .track_scroll(&self.scroll_handle)
                                .size_full(),
                            )
                            .custom_scrollbars(
                                Scrollbars::for_settings::<ProjectPanelScrollbarProxy>()
                                    .tracked_scroll_handle(&self.scroll_handle)
                                    .notify_content(),
                                window,
                                cx,
                            ),
                    )
            })
    }
}
