use std::{any::TypeId, cmp, sync::Arc};

use anyhow::{Context as _, Result};
use editor::{
    Editor, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer, SplittableEditor,
    multibuffer_context_lines,
};
use git::{Oid, repository::LineHistoryTarget};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, SharedString, Task, WeakEntity, Window,
};
use language::{Capability, Point};
use multi_buffer::PathKey;
use project::{
    Project,
    git_store::{LineHistoryDiffFile, Repository, RepositoryId},
};
use settings::{DiffViewStyle, Settings};
use ui::prelude::*;
use workspace::{Item, Workspace, item::ItemEvent, searchable::SearchableItemHandle};

use crate::commit_view::{GitBlob, build_buffer, build_buffer_diff, worktree_id_for_repo_path};

pub(crate) struct LineHistoryView {
    pub(crate) repository_id: RepositoryId,
    pub(crate) target: LineHistoryTarget,
    pub(crate) sha: Oid,
    editor: Entity<SplittableEditor>,
    title: SharedString,
    loaded: bool,
    error: Option<SharedString>,
    _load_task: Task<()>,
}

impl LineHistoryView {
    pub(crate) fn new(
        repository: Entity<Repository>,
        target: LineHistoryTarget,
        sha: Oid,
        files: Task<Result<Vec<LineHistoryDiffFile>>>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let file_name = target
            .path
            .file_name()
            .unwrap_or_else(|| target.path.as_unix_str());
        let title = format!("{file_name}:L{} @ {}", target.line, sha.display_short()).into();
        let languages = project.read(cx).languages().clone();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project.clone(),
                workspace,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_read_only(true);
                editor.set_should_serialize(false, cx);
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
            });
            editor
        });
        let repository_id = repository.read(cx).id;
        let load_task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let files = files.await?;
                anyhow::ensure!(
                    !files.is_empty(),
                    "No line changes are available for this commit"
                );
                for file in files {
                    Self::append_file(&this, file, sha, &repository, &project, &languages, cx)
                        .await?;
                }
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.loaded = true;
                this.error = result.err().map(|error| error.to_string().into());
                cx.notify();
            })
            .ok();
        });
        Self {
            repository_id,
            target,
            sha,
            editor,
            title,
            loaded: false,
            error: None,
            _load_task: load_task,
        }
    }

    async fn append_file(
        view: &WeakEntity<Self>,
        scoped_file: LineHistoryDiffFile,
        sha: Oid,
        repository: &Entity<Repository>,
        project: &Entity<Project>,
        languages: &Arc<language::LanguageRegistry>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let LineHistoryDiffFile { file, ranges } = scoped_file;
        anyhow::ensure!(
            !file.is_binary,
            "This historical file is binary and cannot be displayed as a text diff"
        );
        anyhow::ensure!(
            !ranges.is_empty(),
            "No historical line ranges are available for this file"
        );
        // Git counts LF boundaries, while the native editor also splits standalone CR.
        // Reject those historical snapshots rather than showing a different line range.
        anyhow::ensure!(
            [file.old_text.as_deref(), file.new_text.as_deref()]
                .into_iter()
                .flatten()
                .all(|text| !text.bytes().enumerate().any(|(index, byte)| {
                    byte == b'\r' && text.as_bytes().get(index + 1) != Some(&b'\n')
                })),
            "This historical file contains standalone CR line endings, which cannot be displayed with accurate Git line numbers"
        );
        let worktree_id = repository
            .read_with(cx, |repository, cx| {
                worktree_id_for_repo_path(repository, project.read(cx), &file.path, cx)
            })
            .context("Project has no worktrees")?;
        let blob = Arc::new(GitBlob {
            display_name: format!("{} - {}", sha.display_short(), file.path.as_unix_str()),
            path: file.path,
            worktree_id,
            is_deleted: file.new_text.is_none(),
            is_binary: false,
        });
        let buffer = build_buffer(file.new_text.unwrap_or_default(), blob, languages, cx).await?;
        buffer.update(cx, |buffer, cx| {
            buffer.set_capability(Capability::ReadOnly, cx)
        });
        let diff = build_buffer_diff(file.old_text, &buffer, languages, cx).await?;
        view.update_in(cx, |view, window, cx| {
            let snapshot = buffer.read(cx).snapshot();
            let path = PathKey::with_sort_prefix(1, snapshot.file().unwrap().path().clone());
            let max_point = snapshot.max_point();
            // Scope excerpts to the Git -L ranges, while keeping full historical buffers
            // so the native diff can display and expand the surrounding context.
            let ranges: Vec<_> = ranges
                .into_iter()
                .map(|range| {
                    cmp::min(Point::new(range.start, 0), max_point)
                        ..cmp::min(Point::new(range.end, 0), max_point)
                })
                .collect();
            let context_lines = multibuffer_context_lines(cx).max(3);
            view.editor.update(cx, |editor, cx| {
                editor.update_excerpts_for_path(path, buffer, ranges, context_lines, diff, cx);
                if editor.diff_view_style() == DiffViewStyle::Split {
                    editor.split(window, cx);
                }
            });
            cx.notify();
        })?;
        Ok(())
    }

    pub(crate) fn open(
        repository: Entity<Repository>,
        target: LineHistoryTarget,
        sha: Oid,
        patch: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let repository_id = repository.read(cx).id;
        let existing = workspace.items_of_type::<Self>(cx).find(|view| {
            let view = view.read(cx);
            view.repository_id == repository_id && view.target == target && view.sha == sha
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let files = repository.update(cx, |repository, cx| {
            repository.line_history_diff(sha, patch.to_owned(), cx)
        });
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let view = cx.new(|cx| {
            Self::new(
                repository,
                target,
                sha,
                files,
                project,
                workspace_entity,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }
}

impl EventEmitter<ItemEvent> for LineHistoryView {}

impl Focusable for LineHistoryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for LineHistoryView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .when(!self.loaded, |view| {
                view.child(
                    div()
                        .p_3()
                        .child(Label::new("Loading line changes…").color(Color::Muted)),
                )
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(div().p_3().child(
                    Label::new(format!("Unable to load line changes: {error}")).color(Color::Error),
                ))
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Item for LineHistoryView {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(
            format!(
                "{}:L{} at {}\nLine history from {}",
                self.target.path.as_unix_str(),
                self.target.line,
                self.sha,
                self.target.revision
            )
            .into(),
        )
    }

    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::RepoPath;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::{FakeFs, git_store::CommitFile};
    use serde_json::json;
    use settings::SettingsStore;
    use std::{path::Path, str::FromStr};

    async fn open_test_view(
        style: DiffViewStyle,
        created: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<LineHistoryView>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(style);
                })
            });
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({".git": {}, "sample.txt": "current\n"}),
        )
        .await;
        let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();
        let repository =
            project.read_with(cx, |project, cx| project.active_repository(cx).unwrap());
        let (multi, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let old_text = (1..=20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let new_text = old_text
            .replace("line 10\n", "changed target\n")
            .replace("line 18\n", "unrelated change\n");
        let files = vec![LineHistoryDiffFile {
            file: CommitFile {
                path: RepoPath::new("sample.txt").unwrap(),
                old_text: (!created).then_some(old_text),
                new_text: Some(new_text),
                is_binary: false,
            },
            ranges: vec![9..10],
        }];
        let sha = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        let view = workspace.update_in(cx, |workspace, window, cx| {
            let workspace_entity = cx.entity();
            let view = cx.new(|cx| {
                LineHistoryView::new(
                    repository,
                    LineHistoryTarget {
                        path: RepoPath::new("sample.txt").unwrap(),
                        revision: sha,
                        line: 10,
                    },
                    sha,
                    Task::ready(Ok(files)),
                    project.clone(),
                    workspace_entity,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn assert_context(view: &LineHistoryView, cx: &App) {
        assert!(view.loaded);
        assert!(view.error.is_none(), "{:?}", view.error);
        let editor = view.editor.read(cx).rhs_editor().read(cx);
        assert!(editor.read_only(cx));
        let text = editor.buffer().read(cx).snapshot(cx).text();
        for context in [
            "line 7\n",
            "line 9\n",
            "changed target\n",
            "line 11\n",
            "line 13\n",
        ] {
            assert!(text.contains(context), "missing {context:?}: {text:?}");
        }
        assert!(!text.contains("unrelated change"), "{text:?}");
        assert!(!text.contains("line 1\n"), "{text:?}");
    }

    #[gpui::test]
    async fn line_history_native_unified_diff_keeps_context_and_scope(cx: &mut TestAppContext) {
        let (view, cx) = open_test_view(DiffViewStyle::Unified, false, cx).await;
        view.read_with(cx, |view, cx| {
            assert_context(view, cx);
            assert!(view.editor.read(cx).lhs_editor().is_none());
        });
    }

    #[gpui::test]
    async fn line_history_native_split_diff_keeps_context_and_scope(cx: &mut TestAppContext) {
        let (view, cx) = open_test_view(DiffViewStyle::Split, false, cx).await;
        view.read_with(cx, |view, cx| {
            assert_context(view, cx);
            let lhs = view.editor.read(cx).lhs_editor().unwrap().read(cx);
            assert!(lhs.read_only(cx));
            assert!(
                lhs.buffer()
                    .read(cx)
                    .snapshot(cx)
                    .text()
                    .contains("line 10\n")
            );
        });
    }

    #[gpui::test]
    async fn line_history_native_root_diff_scopes_large_created_file(cx: &mut TestAppContext) {
        let (view, cx) = open_test_view(DiffViewStyle::Unified, true, cx).await;
        view.read_with(cx, assert_context);
    }
}
