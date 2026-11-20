use crate::{Project, lsp_store::LspStoreEvent};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
use gpui::{AppContext as _, Context, Entity, EventEmitter, SharedString, Subscription, Task};
use language::Buffer;
use lsp::LanguageServerId;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use url::Url;

pub(crate) struct JavaDependencyEvent;

impl lsp::notification::Notification for JavaDependencyEvent {
    type Params = serde_json::Value;
    const METHOD: &'static str = "language/eventNotification";
}

pub(crate) struct JavaDependencyDocument;

impl lsp::notification::Notification for JavaDependencyDocument {
    type Params = serde_json::Value;
    const METHOD: &'static str = "java/dependencyDocument";
}

const PAGE_SIZE: usize = 200;
const CACHED_NODE_LIMIT: usize = 20_000;
const CHILDREN_COMMAND: &str = "java.dependencies.children";
const OPEN_COMMAND: &str = "java.dependencies.open";
const LOCATE_COMMAND: &str = "java.dependencies.locate";

pub type JavaDependencyId = u64;

pub enum JavaDependenciesEvent {
    Revealed {
        id: JavaDependencyId,
        kind: JavaDependencyRowKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JavaDependencyRowKind {
    Project,
    Jar,
    Package,
    Class,
    Loading,
    Error,
    LoadMore,
    Info,
}

#[derive(Clone)]
pub struct JavaDependencyRow {
    pub id: JavaDependencyId,
    pub name: SharedString,
    pub kind: JavaDependencyRowKind,
    pub depth: usize,
    pub expanded: bool,
    pub loading: bool,
    pub detail: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum NodeKind {
    Root,
    Project,
    Jar,
    Package,
    Class,
}

impl NodeKind {
    fn query_kind(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Project => "project",
            Self::Jar => "jar",
            Self::Package => "package",
            Self::Class => "class",
        }
    }

    fn row_kind(self) -> JavaDependencyRowKind {
        match self {
            Self::Root => JavaDependencyRowKind::Info,
            Self::Project => JavaDependencyRowKind::Project,
            Self::Jar => JavaDependencyRowKind::Jar,
            Self::Package => JavaDependencyRowKind::Package,
            Self::Class => JavaDependencyRowKind::Class,
        }
    }
}

/// Recover the library path from the `jdt://` URI stored beside a cached document.
///
/// JDT puts the project name and the real JAR path in the URI query, but the
/// cache file itself lives in a temporary decompilation directory. Displaying the
/// library path keeps dependency documents anchored to the JAR the user is reading.
pub fn dependency_display_path(jdt_uri: &str) -> Option<DependencyDisplayPath> {
    let uri = Url::parse(jdt_uri).ok()?;
    if uri.scheme() != "jdt" || uri.host_str() != Some("contents") {
        return None;
    }

    let mut segments = uri.path_segments()?.filter(|segment| !segment.is_empty());
    let jar_name = decode_uri_segment(segments.next()?);
    let path_within = segments
        .map(decode_uri_segment)
        .collect::<Vec<_>>()
        .join("/");
    if jar_name.is_empty() || path_within.is_empty() {
        return None;
    }

    let compact = format!("{jar_name}!/{path_within}");
    let full = uri
        .query()
        .and_then(jar_path_from_query)
        .map(|jar_path| format!("{jar_path}!/{path_within}"))
        .unwrap_or_else(|| compact.clone());
    Some(DependencyDisplayPath { compact, full })
}

fn decode_uri_segment(segment: &str) -> String {
    percent_decode_str(segment).decode_utf8_lossy().into_owned()
}

fn jar_path_from_query(query: &str) -> Option<String> {
    query.split('=').find_map(|part| {
        if part.is_empty() {
            return None;
        }
        let decoded = percent_decode_str(part).decode_utf8().ok()?.into_owned();
        // In project-relative classpath entries JDT appends the class handle
        // directly after the JAR path (`library.jar<package(Class.class`).
        let decoded = decoded.split('<').next().unwrap_or(&decoded).to_string();
        // JDT prefixes the JAR path with the project name. The path itself may
        // start with an escaped separator, so remember that before normalizing.
        let (_, remainder) = decoded.split_once('/').unwrap_or(("", decoded.as_str()));
        let absolute = remainder.starts_with('\\') || remainder.starts_with('/');
        let normalized = remainder
            .replace('\\', "/")
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        if normalized.is_empty() {
            return None;
        }
        let candidate = if absolute && !normalized.starts_with('/') {
            format!("/{normalized}")
        } else {
            normalized
        };
        candidate
            .to_ascii_lowercase()
            .ends_with(".jar")
            .then_some(candidate)
    })
}

#[derive(Deserialize)]
struct WireNode {
    kind: NodeKind,
    handle: String,
    name: String,
    uri: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChildrenPage {
    nodes: Vec<WireNode>,
    next_offset: Option<usize>,
    total: usize,
}

#[derive(Deserialize)]
struct LocateResponse {
    nodes: Vec<WireNode>,
}

/// A library path suitable for display outside the proxy's temporary cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyDisplayPath {
    /// Compact form used in breadcrumbs, e.g. `guava-33.jar!/com/google/Foo.java`.
    pub compact: String,
    /// Absolute JAR path used in tooltips when it can be recovered from the URI.
    pub full: String,
}

struct RevealState {
    token: u64,
    server_id: LanguageServerId,
    chain: Option<Vec<(NodeKind, String)>>,
    next: usize,
}

enum RevealAction {
    Wait,
    Load(usize),
    Failed,
    Ready,
}

enum LoadState {
    Unloaded,
    Loading,
    Loaded,
    Failed(SharedString),
}

struct Node {
    server_id: LanguageServerId,
    kind: NodeKind,
    handle: String,
    name: SharedString,
    detail: Option<SharedString>,
    uri: Option<String>,
    parent: Option<JavaDependencyId>,
    children: Vec<JavaDependencyId>,
    next_offset: Option<usize>,
    state: LoadState,
    expanded: bool,
    request_token: Option<u64>,
    touched: u64,
}

enum PendingWork {
    Children {
        id: JavaDependencyId,
        server_id: LanguageServerId,
        offset: usize,
        token: u64,
    },
    Open {
        id: JavaDependencyId,
        server_id: LanguageServerId,
    },
}

impl PendingWork {
    fn server_id(&self) -> LanguageServerId {
        match self {
            Self::Children { server_id, .. } | Self::Open { server_id, .. } => *server_id,
        }
    }

    fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }
}

struct ActiveWork {
    id: JavaDependencyId,
    token: u64,
    is_open: bool,
    _task: Task<()>,
}

struct OpeningDocument {
    handle: String,
    server_id: LanguageServerId,
    waiters: Vec<oneshot::Sender<Result<Entity<Buffer>>>>,
}

impl EventEmitter<JavaDependenciesEvent> for JavaDependencies {}

pub struct JavaDependencies {
    project: Entity<Project>,
    visible: bool,
    nodes: HashMap<JavaDependencyId, Node>,
    roots: Vec<JavaDependencyId>,
    rows: Vec<JavaDependencyRow>,
    next_id: u64,
    clock: u64,
    pending: VecDeque<PendingWork>,
    active: HashMap<LanguageServerId, ActiveWork>,
    opening: HashMap<JavaDependencyId, OpeningDocument>,
    reveal: Option<RevealState>,
    revealed: Option<(JavaDependencyId, JavaDependencyRowKind)>,
    invalidation_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl JavaDependencies {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let lsp_store = project.read(cx).lsp_store();
        let subscription = cx.subscribe(&lsp_store, |this, _, event, cx| match event {
            LspStoreEvent::JavaDependenciesChanged {
                server_id,
                project_uri,
            } => {
                this.invalidate(*server_id, project_uri.as_deref(), cx);
            }
            LspStoreEvent::LanguageServerAdded(..) | LspStoreEvent::LanguageServerRemoved(..) => {
                this.sync_servers(cx);
                this.rebuild_rows(cx);
            }
            _ => {}
        });
        Self {
            project,
            visible: false,
            nodes: HashMap::default(),
            roots: Vec::new(),
            rows: Vec::new(),
            next_id: 1,
            clock: 0,
            pending: VecDeque::new(),
            active: HashMap::default(),
            opening: HashMap::default(),
            reveal: None,
            revealed: None,
            invalidation_task: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn rows(&self) -> &[JavaDependencyRow] {
        &self.rows
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.visible = visible;
        if visible {
            self.sync_servers(cx);
            self.resume_visible_queries(cx);
            self.pump(cx);
        } else {
            self.cancel_reveal();
            self.cancel_queries(None);
            for id in self.opening.keys().copied().collect::<Vec<_>>() {
                self.cancel_open_work(id);
            }
        }
        self.rebuild_rows(cx);
    }

    /// Expand the tree to the class referenced by a `jdt://` URI and select it.
    ///
    /// Navigation can open a dependency document before its JAR branch has ever
    /// been expanded. The locate response gives us the exact handles, and this
    /// state machine loads each level lazily, reusing the normal paging and
    /// cancellation rules instead of fabricating a partial tree.
    pub fn reveal(&mut self, server_id: LanguageServerId, jdt_uri: String, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.clock += 1;
        let token = self.clock;
        self.reveal = Some(RevealState {
            token,
            server_id,
            chain: None,
            next: 0,
        });
        self.revealed = None;

        let lsp_store = self.project.read(cx).lsp_store();
        let request = lsp_store.update(cx, |store, cx| {
            store.execute_lsp_command(
                server_id,
                LOCATE_COMMAND.into(),
                vec![json!({"uri": jdt_uri})],
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = async {
                let value = request
                    .await?
                    .context("empty Java dependency locate response")?;
                let response: LocateResponse = serde_json::from_value(value)
                    .context("invalid Java dependency locate response")?;
                if response.nodes.len() != 4 {
                    bail!("Java dependency locate returned an incomplete path");
                }
                let expected = [
                    NodeKind::Project,
                    NodeKind::Jar,
                    NodeKind::Package,
                    NodeKind::Class,
                ];
                let mut chain = Vec::with_capacity(4);
                for (node, expected_kind) in response.nodes.into_iter().zip(expected) {
                    if node.kind != expected_kind || node.handle.is_empty() {
                        bail!("Java dependency locate returned an invalid path");
                    }
                    chain.push((node.kind, node.handle));
                }
                Ok::<_, anyhow::Error>(chain)
            }
            .await;

            if let Err(error) = this.update(cx, |this, cx| {
                if this
                    .reveal
                    .as_ref()
                    .is_none_or(|reveal| reveal.token != token)
                {
                    return;
                }
                match result {
                    Ok(chain) => {
                        if let Some(reveal) = &mut this.reveal {
                            reveal.chain = Some(chain);
                        }
                        this.advance_reveal(cx);
                    }
                    Err(error) => {
                        log::debug!("Java dependency locate failed: {error:#}");
                        this.cancel_reveal();
                    }
                }
                this.pump(cx);
                this.rebuild_rows(cx);
            }) {
                log::debug!("dependency view closed: {error}");
            }
        })
        .detach();
    }

    fn cancel_reveal(&mut self) {
        self.reveal = None;
        self.revealed = None;
    }

    fn advance_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(mut reveal) = self.reveal.take() else {
            return;
        };
        if !self.visible {
            return;
        }
        let Some(chain) = reveal.chain.clone() else {
            self.reveal = Some(reveal);
            return;
        };
        let server_id = reveal.server_id;
        loop {
            let Some((kind, handle)) = chain.get(reveal.next).cloned() else {
                self.cancel_reveal();
                return;
            };
            if let Some(id) = self.find_node(server_id, kind, &handle) {
                if reveal.next + 1 == chain.len() {
                    self.revealed = Some((id, kind.row_kind()));
                    self.reveal = None;
                    return;
                }
                match self.reveal_expand_action(id) {
                    RevealAction::Wait => {
                        self.reveal = Some(reveal);
                        return;
                    }
                    RevealAction::Load(offset) => {
                        self.enqueue_children(id, offset, cx);
                        self.reveal = Some(reveal);
                        return;
                    }
                    RevealAction::Failed => {
                        self.cancel_reveal();
                        return;
                    }
                    RevealAction::Ready => {
                        reveal.next += 1;
                    }
                }
            } else {
                let parent_id = if reveal.next == 0 {
                    self.roots.iter().copied().find(|id| {
                        self.nodes
                            .get(id)
                            .is_some_and(|node| node.server_id == server_id)
                    })
                } else {
                    chain
                        .get(reveal.next - 1)
                        .and_then(|(kind, handle)| self.find_node(server_id, *kind, handle))
                };
                let Some(parent_id) = parent_id else {
                    self.cancel_reveal();
                    return;
                };
                match self.reveal_children_action(parent_id) {
                    RevealAction::Wait => {
                        self.reveal = Some(reveal);
                        return;
                    }
                    RevealAction::Load(offset) => {
                        self.enqueue_children(parent_id, offset, cx);
                        self.reveal = Some(reveal);
                        return;
                    }
                    RevealAction::Failed => {
                        self.cancel_reveal();
                        return;
                    }
                    RevealAction::Ready => {}
                }
            }
        }
    }

    fn find_node(
        &self,
        server_id: LanguageServerId,
        kind: NodeKind,
        handle: &str,
    ) -> Option<JavaDependencyId> {
        self.nodes.iter().find_map(|(id, node)| {
            (node.server_id == server_id && node.kind == kind && node.handle.as_str() == handle)
                .then_some(*id)
        })
    }

    fn reveal_expand_action(&mut self, id: JavaDependencyId) -> RevealAction {
        self.clock += 1;
        let clock = self.clock;
        let Some(node) = self.nodes.get_mut(&id) else {
            return RevealAction::Failed;
        };
        node.expanded = true;
        node.touched = clock;
        match &node.state {
            LoadState::Unloaded => RevealAction::Load(0),
            LoadState::Loading => RevealAction::Wait,
            LoadState::Failed(_) => RevealAction::Failed,
            LoadState::Loaded => RevealAction::Ready,
        }
    }

    fn reveal_children_action(&mut self, id: JavaDependencyId) -> RevealAction {
        self.clock += 1;
        let clock = self.clock;
        let Some(node) = self.nodes.get_mut(&id) else {
            return RevealAction::Failed;
        };
        node.expanded = true;
        node.touched = clock;
        match &node.state {
            LoadState::Unloaded => RevealAction::Load(0),
            LoadState::Loading => RevealAction::Wait,
            LoadState::Failed(_) => RevealAction::Failed,
            LoadState::Loaded => match node.next_offset {
                Some(offset) => RevealAction::Load(offset),
                None => RevealAction::Failed,
            },
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.cancel_reveal();
        self.cancel_queries(None);
        for id in self.roots.clone() {
            self.clear_children(id);
            if let Some(node) = self.nodes.get_mut(&id) {
                node.state = LoadState::Unloaded;
            }
        }
        self.set_visible(self.visible, cx);
    }

    pub fn toggle(&mut self, id: JavaDependencyId, cx: &mut Context<Self>) {
        self.clock += 1;
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if node.kind == NodeKind::Class {
            return;
        }
        node.expanded = !node.expanded;
        node.touched = self.clock;
        if node.expanded {
            if matches!(node.state, LoadState::Unloaded) {
                self.enqueue_children(id, 0, cx);
            }
        } else {
            self.cancel_queries(Some(id));
            self.pump(cx);
        }
        self.resume_visible_queries(cx);
        self.evict_collapsed(0);
        self.rebuild_rows(cx);
    }

    pub fn load_more(&mut self, id: JavaDependencyId, cx: &mut Context<Self>) {
        if let Some(offset) = self.nodes.get(&id).and_then(|node| node.next_offset) {
            self.enqueue_children(id, offset, cx);
            self.rebuild_rows(cx);
        }
    }

    pub fn retry(&mut self, id: JavaDependencyId, cx: &mut Context<Self>) {
        let offset = self
            .nodes
            .get(&id)
            .and_then(|node| node.next_offset)
            .unwrap_or(0);
        self.enqueue_children(id, offset, cx);
        self.rebuild_rows(cx);
    }

    pub fn open(
        &mut self,
        id: JavaDependencyId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        if !self.visible {
            return Task::ready(Err(anyhow!("Java dependency browser is hidden")));
        }
        let (sender, receiver) = oneshot::channel();
        if let Some(opening) = self.opening.get_mut(&id) {
            opening.waiters.push(sender);
        } else {
            let Some(node) = self
                .nodes
                .get(&id)
                .filter(|node| node.kind == NodeKind::Class)
            else {
                return Task::ready(Err(anyhow!(
                    "dependency class no longer exists; refresh the tree"
                )));
            };
            let server_id = node.server_id;
            self.opening.insert(
                id,
                OpeningDocument {
                    handle: node.handle.clone(),
                    server_id,
                    waiters: vec![sender],
                },
            );
            // Opening a class takes precedence over tree expansions still in the queue.
            self.pending.push_front(PendingWork::Open { id, server_id });
            self.pump(cx);
            self.rebuild_rows(cx);
        }
        cx.background_spawn(async move {
            receiver
                .await
                .context("dependency document request was cancelled")?
        })
    }

    pub fn cancel_open(&mut self, id: JavaDependencyId, cx: &mut Context<Self>) {
        self.cancel_open_work(id);
        self.pump(cx);
        self.rebuild_rows(cx);
    }

    fn cancel_open_work(&mut self, id: JavaDependencyId) {
        self.pending.retain(
            |work| !matches!(work, PendingWork::Open { id: pending, .. } if *pending == id),
        );
        if let Some(opening) = self.opening.get(&id) {
            let server_id = opening.server_id;
            if self
                .active
                .get(&server_id)
                .is_some_and(|work| work.is_open && work.id == id)
            {
                self.active.remove(&server_id);
            }
        }
        self.finish_open(id, Err(anyhow!("Java dependency open cancelled")));
    }

    fn sync_servers(&mut self, cx: &mut Context<Self>) {
        let lsp_store = self.project.read(cx).lsp_store();
        let supported = {
            let store = lsp_store.read(cx);
            store
                .language_server_statuses()
                .filter_map(|(id, status)| {
                    let server = store.as_local()?.running_language_server_for_id(id)?;
                    server
                        .capabilities()
                        .execute_command_provider
                        .as_ref()?
                        .commands
                        .iter()
                        .any(|command| command == CHILDREN_COMMAND)
                        .then(|| (id, SharedString::from(status.name.to_string())))
                })
                .collect::<Vec<_>>()
        };
        let live = supported.iter().map(|(id, _)| *id).collect::<HashSet<_>>();
        if self
            .reveal
            .as_ref()
            .is_some_and(|reveal| !live.contains(&reveal.server_id))
        {
            self.cancel_reveal();
        }
        for id in self.roots.clone() {
            if self
                .nodes
                .get(&id)
                .is_some_and(|node| !live.contains(&node.server_id))
            {
                if let Some(node) = self.nodes.get(&id) {
                    let server_id = node.server_id;
                    self.active.remove(&server_id);
                    self.pending.retain(|work| work.server_id() != server_id);
                    let opening = self
                        .opening
                        .iter()
                        .filter_map(|(id, document)| {
                            (document.server_id == server_id).then_some(*id)
                        })
                        .collect::<Vec<_>>();
                    for id in opening {
                        self.finish_open(id, Err(anyhow!("Java language server stopped")));
                    }
                }
                self.clear_children(id);
                self.nodes.remove(&id);
                self.roots.retain(|root| *root != id);
            }
        }
        if !self.visible {
            return;
        }
        for (server_id, name) in supported {
            if self.roots.iter().any(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.server_id == server_id)
            }) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.nodes.insert(
                id,
                Node {
                    server_id,
                    kind: NodeKind::Root,
                    handle: String::new(),
                    name,
                    detail: None,
                    uri: None,
                    parent: None,
                    children: Vec::new(),
                    next_offset: None,
                    state: LoadState::Unloaded,
                    expanded: true,
                    request_token: None,
                    touched: self.clock,
                },
            );
            self.roots.push(id);
            self.enqueue_children(id, 0, cx);
        }
    }

    fn invalidate(
        &mut self,
        server_id: LanguageServerId,
        project_uri: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let project_path = project_uri
            .and_then(|uri| uri.parse::<lsp::Uri>().ok())
            .and_then(|uri| uri.to_file_path().ok());
        let affected = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                if node.server_id != server_id {
                    return None;
                }
                let affected = match &project_path {
                    Some(path) => {
                        node.kind == NodeKind::Project
                            && node
                                .uri
                                .as_deref()
                                .and_then(|uri| uri.parse::<lsp::Uri>().ok())
                                .and_then(|uri| uri.to_file_path().ok())
                                .as_ref()
                                == Some(path)
                    }
                    None => node.kind == NodeKind::Root,
                };
                affected.then_some(*id)
            })
            .collect::<Vec<_>>();
        if affected.is_empty() {
            return;
        }
        if self
            .reveal
            .as_ref()
            .is_some_and(|reveal| reveal.server_id == server_id)
        {
            self.cancel_reveal();
        }
        for id in affected {
            self.cancel_queries(Some(id));
            self.clear_children(id);
            if let Some(node) = self.nodes.get_mut(&id) {
                node.state = LoadState::Unloaded;
            }
        }
        // Imports can emit many classpath events. Coalesce them before querying again.
        if self.visible {
            self.invalidation_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                if let Err(error) = this.update(cx, |this, cx| {
                    this.resume_visible_queries(cx);
                    this.rebuild_rows(cx);
                }) {
                    log::debug!("dependency view closed: {error}");
                }
            }));
        }
        self.pump(cx);
        self.rebuild_rows(cx);
    }

    fn resume_visible_queries(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let pending = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                if !node.expanded || !matches!(node.state, LoadState::Unloaded) {
                    return None;
                }
                let mut parent = node.parent;
                while let Some(parent_id) = parent {
                    let ancestor = self.nodes.get(&parent_id)?;
                    if !ancestor.expanded {
                        return None;
                    }
                    parent = ancestor.parent;
                }
                Some(*id)
            })
            .collect::<Vec<_>>();
        for id in pending {
            self.enqueue_children(id, 0, cx);
        }
    }

    fn enqueue_children(&mut self, id: JavaDependencyId, offset: usize, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if matches!(node.state, LoadState::Loading) {
            return;
        }
        self.clock += 1;
        let token = self.clock;
        node.request_token = Some(token);
        node.state = LoadState::Loading;
        node.touched = token;
        self.pending.push_back(PendingWork::Children {
            id,
            server_id: node.server_id,
            offset,
            token,
        });
        self.pump(cx);
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        while let Some(index) = self.pending.iter().position(|work| {
            (self.visible || work.is_open()) && !self.active.contains_key(&work.server_id())
        }) {
            let Some(work) = self.pending.remove(index) else {
                break;
            };
            let server_id = work.server_id();
            let lsp_store = self.project.read(cx).lsp_store();
            match work {
                PendingWork::Children {
                    id, offset, token, ..
                } => {
                    let Some(node) = self.nodes.get(&id) else {
                        continue;
                    };
                    let argument = json!({"kind": node.kind.query_kind(), "handle": node.handle,
                        "offset": offset, "limit": PAGE_SIZE});
                    let request = lsp_store.update(cx, |store, cx| {
                        store.execute_lsp_command(
                            server_id,
                            CHILDREN_COMMAND.into(),
                            vec![argument],
                            cx,
                        )
                    });
                    let response = cx.background_spawn(async move {
                        let value = request.await?.context("empty Java dependency response")?;
                        let page: ChildrenPage = serde_json::from_value(value)
                            .context("invalid Java dependency response")?;
                        if page.nodes.len() > PAGE_SIZE
                            || page.next_offset.is_some_and(|next| next <= offset)
                        {
                            bail!("invalid Java dependency pagination");
                        }
                        Ok(page)
                    });
                    let started = Instant::now();
                    let task = cx.spawn(async move |this, cx| {
                        let result = response.await;
                        if let Err(error) = this.update(cx, |this, cx| {
                            if this
                                .active
                                .get(&server_id)
                                .is_none_or(|active| active.token != token)
                            {
                                return;
                            }
                            this.active.remove(&server_id);
                            this.finish_children(id, token, offset, result);
                            this.advance_reveal(cx);
                            log::debug!(
                                "Java dependency query finished in {:?}",
                                started.elapsed()
                            );
                            this.evict_collapsed(0);
                            this.pump(cx);
                            this.rebuild_rows(cx);
                        }) {
                            log::debug!("dependency view closed: {error}");
                        }
                    });
                    self.active.insert(
                        server_id,
                        ActiveWork {
                            id,
                            token,
                            is_open: false,
                            _task: task,
                        },
                    );
                }
                PendingWork::Open { id, .. } => {
                    let Some(opening) = self.opening.get(&id) else {
                        continue;
                    };
                    let argument = json!({"handle": opening.handle});
                    let request = lsp_store.update(cx, |store, cx| {
                        store.execute_lsp_command(
                            server_id,
                            OPEN_COMMAND.into(),
                            vec![argument],
                            cx,
                        )
                    });
                    let project = self.project.downgrade();
                    self.clock += 1;
                    let token = self.clock;
                    let task = cx.spawn(async move |this, cx| {
                        let result = async {
                            let value = request.await?.context("empty Java class location")?;
                            let locations: Vec<lsp::Location> = serde_json::from_value(value)
                                .context("invalid Java class location")?;
                            let location = locations.into_iter().next().context("Java class is no longer available")?;
                            if location.uri.scheme() != "file" {
                                bail!("Java proxy could not load this class; retry after checking the proxy installation");
                            }
                            project.update(cx, |project, cx|
                                project.open_local_buffer_via_lsp(location.uri, server_id, cx))?.await
                        }.await;
                        if let Err(error) = this.update(cx, |this, cx| {
                            if this.active.get(&server_id).is_none_or(|active| active.token != token) { return; }
                            this.active.remove(&server_id);
                            this.finish_open(id, result);
                            this.pump(cx);
                            this.rebuild_rows(cx);
                        }) { log::debug!("dependency view closed: {error}"); }
                    });
                    self.active.insert(
                        server_id,
                        ActiveWork {
                            id,
                            token,
                            is_open: true,
                            _task: task,
                        },
                    );
                }
            }
        }
    }

    fn finish_children(
        &mut self,
        id: JavaDependencyId,
        token: u64,
        offset: usize,
        result: Result<ChildrenPage>,
    ) {
        if self
            .nodes
            .get(&id)
            .is_none_or(|node| node.request_token != Some(token))
        {
            return;
        }
        match result {
            Ok(page) => {
                let Some(parent) = self.nodes.get(&id) else {
                    return;
                };
                let server_id = parent.server_id;
                let expected = match parent.kind {
                    NodeKind::Root => NodeKind::Project,
                    NodeKind::Project => NodeKind::Jar,
                    NodeKind::Jar => NodeKind::Package,
                    NodeKind::Package | NodeKind::Class => NodeKind::Class,
                };
                if page
                    .nodes
                    .iter()
                    .any(|node| node.kind != expected || node.handle.is_empty())
                {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.state = LoadState::Failed("Unexpected Java dependency node".into());
                        node.request_token = None;
                    }
                    return;
                }
                if offset == 0 {
                    self.clear_children(id);
                }
                self.evict_collapsed(page.nodes.len());
                if self.nodes.len() + page.nodes.len() > CACHED_NODE_LIMIT {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.state = LoadState::Failed(
                            "Too many entries are open. Collapse another branch and retry.".into(),
                        );
                        node.request_token = None;
                    }
                    return;
                }
                let mut children = Vec::with_capacity(page.nodes.len());
                for wire in page.nodes {
                    let child_id = self.next_id;
                    self.next_id += 1;
                    self.nodes.insert(
                        child_id,
                        Node {
                            server_id,
                            kind: wire.kind,
                            handle: wire.handle,
                            name: wire.name.into(),
                            detail: wire.path.or_else(|| wire.uri.clone()).map(Into::into),
                            uri: wire.uri,
                            parent: Some(id),
                            children: Vec::new(),
                            next_offset: None,
                            state: LoadState::Unloaded,
                            expanded: false,
                            request_token: None,
                            touched: self.clock,
                        },
                    );
                    children.push(child_id);
                }
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.children.extend(children);
                    node.next_offset = page.next_offset;
                    node.state = LoadState::Loaded;
                    node.request_token = None;
                }
                log::trace!("Loaded Java dependency page: {offset}/{}", page.total);
            }
            Err(error) => {
                log::warn!("Java dependency query failed: {error:#}");
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.state = LoadState::Failed(format!("{error:#}").into());
                    node.request_token = None;
                }
            }
        }
    }

    fn finish_open(&mut self, id: JavaDependencyId, result: Result<Entity<Buffer>>) {
        if let Some(opening) = self.opening.remove(&id) {
            for waiter in opening.waiters {
                let result = match &result {
                    Ok(buffer) => Ok(buffer.clone()),
                    Err(error) => Err(anyhow!("{error:#}")),
                };
                if waiter.send(result).is_err() {
                    log::debug!("dependency document caller closed");
                }
            }
        }
    }

    fn belongs_to(&self, mut id: JavaDependencyId, ancestor: JavaDependencyId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            let Some(parent) = self.nodes.get(&id).and_then(|node| node.parent) else {
                return false;
            };
            id = parent;
        }
    }

    fn cancel_queries(&mut self, ancestor: Option<JavaDependencyId>) {
        let affected = self
            .nodes
            .keys()
            .copied()
            .filter(|id| ancestor.is_none_or(|ancestor| self.belongs_to(*id, ancestor)))
            .collect::<HashSet<_>>();
        self.pending.retain(|work| match work {
            PendingWork::Children { id, .. } => !affected.contains(id),
            PendingWork::Open { .. } => true,
        });
        self.active
            .retain(|_, work| work.is_open || !affected.contains(&work.id));
        for id in affected {
            if let Some(node) = self.nodes.get_mut(&id) {
                if matches!(node.state, LoadState::Loading) {
                    node.state = if node.children.is_empty() {
                        LoadState::Unloaded
                    } else {
                        LoadState::Loaded
                    };
                }
                node.request_token = None;
            }
        }
    }

    fn clear_children(&mut self, id: JavaDependencyId) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        let children = std::mem::take(&mut node.children);
        node.next_offset = None;
        for child in children {
            self.clear_children(child);
            self.nodes.remove(&child);
        }
    }

    fn evict_collapsed(&mut self, additional_nodes: usize) {
        if self.nodes.len() + additional_nodes <= CACHED_NODE_LIMIT {
            return;
        }
        let mut candidates = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                (!node.expanded && !node.children.is_empty()).then_some((node.touched, *id))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        for (_, id) in candidates {
            if self.nodes.len() + additional_nodes <= CACHED_NODE_LIMIT {
                break;
            }
            self.clear_children(id);
            if let Some(node) = self.nodes.get_mut(&id) {
                node.state = LoadState::Unloaded;
            }
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        if self.visible {
            for id in &self.roots {
                self.append_rows(*id, 0, &mut rows);
            }
            if rows.is_empty() {
                rows.push(JavaDependencyRow {
                    id: 0,
                    name: "No Java projects available. Import a project and refresh.".into(),
                    kind: JavaDependencyRowKind::Info,
                    depth: 0,
                    expanded: false,
                    loading: false,
                    detail: None,
                });
            }
        }
        self.rows = rows;
        cx.notify();
        if let Some((id, kind)) = self.revealed.take() {
            cx.emit(JavaDependenciesEvent::Revealed { id, kind });
        }
    }

    fn append_rows(&self, id: JavaDependencyId, depth: usize, rows: &mut Vec<JavaDependencyRow>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let is_root = node.kind == NodeKind::Root;
        if !is_root {
            rows.push(JavaDependencyRow {
                id,
                name: node.name.clone(),
                kind: node.kind.row_kind(),
                depth,
                expanded: node.expanded,
                loading: matches!(node.state, LoadState::Loading) || self.opening.contains_key(&id),
                detail: node.detail.clone(),
            });
        }
        if !node.expanded || node.kind == NodeKind::Class {
            return;
        }
        let child_depth = depth + usize::from(!is_root);
        for child in &node.children {
            self.append_rows(*child, child_depth, rows);
        }
        let auxiliary = match &node.state {
            LoadState::Unloaded | LoadState::Loading => {
                Some((JavaDependencyRowKind::Loading, "Loading…".into(), None))
            }
            LoadState::Failed(error) => Some((
                JavaDependencyRowKind::Error,
                "Unable to load. Click to retry.".into(),
                Some(error.clone()),
            )),
            LoadState::Loaded if node.next_offset.is_some() => {
                Some((JavaDependencyRowKind::LoadMore, "Load more…".into(), None))
            }
            LoadState::Loaded if node.children.is_empty() => {
                Some((JavaDependencyRowKind::Info, "No entries".into(), None))
            }
            _ => None,
        };
        if let Some((kind, name, detail)) = auxiliary {
            rows.push(JavaDependencyRow {
                id,
                name,
                kind,
                depth: child_depth,
                expanded: false,
                loading: false,
                detail,
            });
        }
    }
}
