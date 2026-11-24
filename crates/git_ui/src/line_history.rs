use editor::Editor;
use git::{Oid, repository::LineHistoryTarget};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    Render, SharedString, Task, Window,
};
use language::Buffer;
use project::{Project, git_store::RepositoryId};
use ui::{Color, Icon, IconName};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent, searchable::SearchableItemHandle};

pub(crate) struct LineHistoryView {
    pub(crate) repository_id: RepositoryId,
    pub(crate) target: LineHistoryTarget,
    pub(crate) sha: Oid,
    editor: Entity<Editor>,
    title: SharedString,
    _language_task: Task<()>,
}

impl LineHistoryView {
    pub(crate) fn new(
        repository_id: RepositoryId,
        target: LineHistoryTarget,
        sha: Oid,
        patch: &str,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let file_name = target
            .path
            .file_name()
            .unwrap_or_else(|| target.path.as_unix_str());
        let title = format!("{file_name}:L{} @ {}", target.line, sha.display_short()).into();
        let buffer = cx.new(|cx| Buffer::local(changed_lines_only(patch), cx));
        let languages = project.read(cx).languages().clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), Some(project), window, cx);
            editor.set_read_only(true);
            editor.set_should_serialize(false, cx);
            editor
        });
        let language_task = cx.spawn(async move |_, cx| {
            if let Some(language) = languages.language_for_name("Diff").await.log_err() {
                buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            }
        });

        Self {
            repository_id,
            target,
            sha,
            editor,
            title,
            _language_task: language_task,
        }
    }

    pub(crate) fn open(
        repository_id: RepositoryId,
        target: LineHistoryTarget,
        sha: Oid,
        patch: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).find(|view| {
            let view = view.read(cx);
            view.repository_id == repository_id && view.target == target && view.sha == sha
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let project = workspace.project().clone();
        let view = cx.new(|cx| Self::new(repository_id, target, sha, patch, project, window, cx));
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
        self.editor.clone()
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
                self.target.revision,
            )
            .into(),
        )
    }

    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
}

fn changed_lines_only(patch: &str) -> String {
    let mut in_hunk = false;
    let mut text = String::with_capacity(patch.len());
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        }
        // Keep the original patch in the history cache; hiding context is only a presentation choice.
        if in_hunk && line.starts_with(' ') {
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::changed_lines_only;

    #[test]
    fn line_history_preview_hides_context_without_losing_header_like_code() {
        let patch = "diff --git a/sample b/sample\n--- a/sample\n+++ b/sample\n@@ -2,3 +2,3 @@\n context\n--- old_expression\n+++ new_expression\n trailing context\n\\ No newline at end of file\n";
        assert_eq!(
            changed_lines_only(patch),
            "diff --git a/sample b/sample\n--- a/sample\n+++ b/sample\n@@ -2,3 +2,3 @@\n--- old_expression\n+++ new_expression\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn line_history_preview_preserves_root_and_multiple_hunks() {
        let patch = "diff --git a/sample b/sample\nnew file mode 100644\n--- /dev/null\n+++ b/sample\n@@ -0,0 +1 @@\n+first line\n@@ -0,0 +9 @@\n+another line\n";
        assert_eq!(changed_lines_only(patch), patch);
    }
}
