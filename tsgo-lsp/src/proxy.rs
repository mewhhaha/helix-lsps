use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, select, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;

use crate::{
    discovery::{Discovery, ProjectContext, SessionKey},
    file_watcher::{WatchedFileChange, WorkspaceWatcher},
    session::{Session, SessionEvent},
};

const FILE_WATCH_INTERVAL: Duration = Duration::from_secs(1);

pub fn run() -> Result<()> {
    init_tracing();

    let (connection, io_threads) = Connection::stdio();
    match Proxy::new(connection)?.run() {
        Ok(()) => {
            io_threads.join().context("failed to join stdio threads")?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn init_tracing() {
    let Ok(filter) =
        EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("tsgo_lsp=info"))
    else {
        return;
    };

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

struct Proxy {
    connection: Connection,
    discovery: Discovery,
    sessions: HashMap<SessionKey, Session>,
    documents: HashMap<String, SessionKey>,
    client_request_routes: HashMap<RequestId, SessionKey>,
    child_request_routes: HashMap<RequestId, ChildRequestRoute>,
    initialize_routes: HashMap<RequestId, InitializeRoute>,
    client_initialize: Option<ClientInitializeState>,
    default_session: Option<SessionKey>,
    internal_request_counter: usize,
    events_tx: Sender<SessionEvent>,
    events_rx: Receiver<SessionEvent>,
    watch_rx: Receiver<Vec<WatchedFileChange>>,
    workspace_watcher: WorkspaceWatcher,
    shutdown_requested: bool,
}

#[derive(Clone)]
struct ClientInitializeState {
    params: Value,
    initialized: Option<Notification>,
}

struct ChildRequestRoute {
    session_key: SessionKey,
    child_request_id: RequestId,
}

struct InitializeRoute {
    session_key: SessionKey,
    forward_to_client: bool,
}

impl Proxy {
    fn new(connection: Connection) -> Result<Self> {
        let (events_tx, events_rx) = unbounded();
        let (workspace_watcher, watch_rx) = WorkspaceWatcher::spawn(FILE_WATCH_INTERVAL)?;

        Ok(Self {
            connection,
            discovery: Discovery,
            sessions: HashMap::new(),
            documents: HashMap::new(),
            client_request_routes: HashMap::new(),
            child_request_routes: HashMap::new(),
            initialize_routes: HashMap::new(),
            client_initialize: None,
            default_session: None,
            internal_request_counter: 0,
            events_tx,
            events_rx,
            watch_rx,
            workspace_watcher,
            shutdown_requested: false,
        })
    }

    fn run(mut self) -> Result<()> {
        loop {
            select! {
                recv(self.connection.receiver) -> message => {
                    let Ok(message) = message else {
                        break;
                    };

                    if self.handle_client_message(message)? {
                        break;
                    }
                }
                recv(self.events_rx) -> event => {
                    let Ok(event) = event else {
                        break;
                    };

                    self.handle_session_event(event)?;
                }
                recv(self.watch_rx) -> changes => {
                    let Ok(changes) = changes else {
                        break;
                    };

                    self.handle_watched_file_changes(changes)?;
                }
            }
        }

        for session in self.sessions.values_mut() {
            session.terminate();
        }

        Ok(())
    }

    fn handle_client_message(&mut self, message: Message) -> Result<bool> {
        match message {
            Message::Request(request) => {
                if self.shutdown_requested {
                    self.connection.sender.send(
                        Response::new_err(
                            request.id,
                            ErrorCode::InvalidRequest as i32,
                            format!("received {} after shutdown", request.method),
                        )
                        .into(),
                    )?;
                    return Ok(false);
                }

                if request.method == "initialize" {
                    self.handle_initialize(request)?;
                    return Ok(false);
                }

                if request.method == "shutdown" {
                    self.shutdown_requested = true;
                    self.connection
                        .sender
                        .send(Response::new_ok(request.id, Value::Null).into())?;
                    return Ok(false);
                }

                let target = self.resolve_target_for_request(&request)?;

                if let Some(target) = target {
                    self.client_request_routes
                        .insert(request.id.clone(), target.clone());
                    self.dispatch_to_session(target, Message::Request(request))?;
                } else {
                    self.respond_missing_target(request)?;
                }
            }
            Message::Notification(notification) => {
                if notification.method == "initialized" {
                    self.store_initialized(notification.clone());
                    self.broadcast_or_dispatch_initialized(notification)?;
                    self.workspace_watcher.set_active(true);
                    return Ok(false);
                }

                if notification.method == "exit" {
                    self.broadcast_notification(notification)?;
                    return Ok(true);
                }

                if notification.method == "$/cancelRequest" {
                    self.handle_cancel_notification(&notification)?;
                }

                match self.resolve_target_for_notification(&notification)? {
                    Dispatch::Single(target) => {
                        self.dispatch_to_session(target, notification.into())?
                    }
                    Dispatch::Broadcast => self.broadcast_notification(notification)?,
                    Dispatch::None => {}
                }
            }
            Message::Response(response) => {
                if let Some(route) = self.child_request_routes.remove(&response.id) {
                    let Some(session) = self.sessions.get(&route.session_key) else {
                        return Ok(false);
                    };
                    session.send(Message::Response(Response {
                        id: route.child_request_id,
                        result: response.result,
                        error: response.error,
                    }))?;
                }
            }
        }

        Ok(false)
    }

    fn handle_watched_file_changes(&mut self, changes: Vec<WatchedFileChange>) -> Result<()> {
        let mut changes_by_session = HashMap::<SessionKey, Vec<Value>>::new();
        for change in changes {
            if self.documents.contains_key(&change.uri) {
                continue;
            }

            changes_by_session
                .entry(change.session_key)
                .or_default()
                .push(json!({
                    "uri": change.uri,
                    "type": change.kind.lsp_file_change_type(),
                }));
        }

        for (session_key, changes) in changes_by_session {
            if !self.sessions.contains_key(&session_key) {
                continue;
            }

            self.dispatch_watched_file_changes(session_key, changes)?;
        }

        Ok(())
    }

    fn dispatch_watched_file_changes(
        &mut self,
        session_key: SessionKey,
        changes: Vec<Value>,
    ) -> Result<()> {
        let notification = Notification::new(
            "workspace/didChangeWatchedFiles".into(),
            json!({
                "changes": changes
            }),
        );
        self.dispatch_to_session(session_key, notification.into())
    }

    fn handle_session_event(&mut self, event: SessionEvent) -> Result<()> {
        match event {
            SessionEvent::Message(session_key, generation, message) => {
                if !self.is_current_session(&session_key, generation) {
                    return Ok(());
                }

                match message {
                    Message::Request(request) => self.handle_child_request(session_key, request)?,
                    Message::Response(response) => {
                        self.handle_child_response(session_key, response)?
                    }
                    Message::Notification(notification) => {
                        self.connection.sender.send(notification.into())?;
                    }
                }
            }
            SessionEvent::Closed(session_key, generation) => {
                if !self.is_current_session(&session_key, generation) {
                    return Ok(());
                }

                self.drop_session(&session_key, "tsgo child exited unexpectedly")?;
            }
        }

        Ok(())
    }

    fn is_current_session(&self, session_key: &SessionKey, generation: u64) -> bool {
        self.sessions
            .get(session_key)
            .is_some_and(|session| session.generation == generation)
    }

    fn handle_initialize(&mut self, request: Request) -> Result<()> {
        let request_id = request.id.clone();
        if let Err(error) = self.try_handle_initialize(request) {
            let message = error.to_string();
            self.connection.sender.send(
                Response::new_err(request_id, ErrorCode::RequestFailed as i32, message.clone())
                    .into(),
            )?;
            self.log_warning(message)?;
        }

        Ok(())
    }

    fn try_handle_initialize(&mut self, request: Request) -> Result<()> {
        let params = request.params.clone();
        self.client_initialize = Some(ClientInitializeState {
            params: params.clone(),
            initialized: None,
        });

        let initial_path = match find_initialize_root(&params) {
            Some(path) => path,
            None => std::env::current_dir()
                .context("failed to determine current directory for initialize fallback")?,
        };
        let context = self.discovery.context_for_uri_path(&initial_path)?;
        let key = context.key.clone();
        self.ensure_session(context, false)?;
        self.default_session = Some(key.clone());

        self.initialize_routes.insert(
            request.id.clone(),
            InitializeRoute {
                session_key: key.clone(),
                forward_to_client: true,
            },
        );

        let initialize = Request {
            id: request.id.clone(),
            method: request.method,
            params: rewrite_initialize_params(&params, self.root_for_session(&key))?,
        };

        self.sessions
            .get(&key)
            .ok_or_else(|| anyhow!("default session disappeared during initialize"))?
            .send(initialize.into())?;

        Ok(())
    }

    fn handle_child_request(&mut self, session_key: SessionKey, request: Request) -> Result<()> {
        let client_id = self.next_internal_request_id("client");
        self.child_request_routes.insert(
            client_id.clone(),
            ChildRequestRoute {
                session_key,
                child_request_id: request.id,
            },
        );

        self.connection
            .sender
            .send(Request::new(client_id, request.method, request.params).into())?;
        Ok(())
    }

    fn handle_child_response(
        &mut self,
        _session_key: SessionKey,
        response: Response,
    ) -> Result<()> {
        let response = normalize_child_response(response);

        if let Some(route) = self.initialize_routes.remove(&response.id) {
            let initialized = response.error.is_none();
            let Some(session) = self.sessions.get_mut(&route.session_key) else {
                return Ok(());
            };

            session.initialized = initialized;
            if route.forward_to_client {
                self.connection.sender.send(response.clone().into())?;
            }

            if initialized {
                if let Some(initialized) = self
                    .client_initialize
                    .as_ref()
                    .and_then(|state| state.initialized.clone())
                {
                    session.send(initialized.into())?;
                }
                session.drain_queue()?;
            } else {
                let message = response
                    .error
                    .as_ref()
                    .map(|error| format!("tsgo initialize failed: {}", error.message))
                    .unwrap_or_else(|| "tsgo initialize failed".to_owned());
                self.drop_session(&route.session_key, &message)?;
            }

            return Ok(());
        }

        self.client_request_routes.remove(&response.id);
        self.connection.sender.send(response.into())?;
        Ok(())
    }

    fn resolve_target_for_request(&mut self, request: &Request) -> Result<Option<SessionKey>> {
        if let Some(uri) = find_uri(&request.params) {
            return self.resolve_target_for_uri(&uri);
        }

        Ok(self.default_session.clone())
    }

    fn resolve_target_for_notification(&mut self, notification: &Notification) -> Result<Dispatch> {
        if notification.method.starts_with("workspace/") {
            return Ok(Dispatch::Broadcast);
        }

        if notification.method == "$/cancelRequest" {
            return Ok(Dispatch::None);
        }

        if let Some(uri) = find_uri(&notification.params) {
            if notification.method == "textDocument/didClose" {
                let target = self
                    .documents
                    .get(uri.as_str())
                    .cloned()
                    .or_else(|| self.resolve_target_for_uri(&uri).ok().flatten());
                self.documents.remove(uri.as_str());
                return Ok(target.map_or(Dispatch::None, Dispatch::Single));
            }

            let target = self.resolve_target_for_uri(&uri)?;
            if notification.method == "textDocument/didOpen" {
                let target = target.as_ref();
                if let Some(target) = target {
                    self.documents.insert(uri.to_string(), target.clone());
                }
            }

            return Ok(target.map_or(Dispatch::None, Dispatch::Single));
        }

        Ok(self
            .default_session
            .clone()
            .map_or(Dispatch::None, Dispatch::Single))
    }

    fn resolve_target_for_uri(&mut self, uri: &Url) -> Result<Option<SessionKey>> {
        if let Some(existing) = self.documents.get(uri.as_str()).cloned() {
            return Ok(Some(existing));
        }

        let Ok(file_path) = uri.to_file_path() else {
            return Ok(self.default_session.clone());
        };
        // A file we cannot resolve or spawn a session for must not take the
        // whole proxy down with it; other projects keep working.
        let context = match self.discovery.context_for_uri_path(&file_path) {
            Ok(context) => context,
            Err(error) => {
                self.log_warning(format!("failed to resolve a tsgo session for {uri}: {error:#}"))?;
                return Ok(None);
            }
        };
        let key = context.key.clone();
        if let Err(error) = self.ensure_session(context, true) {
            self.log_warning(format!("failed to spawn tsgo for {uri}: {error:#}"))?;
            return Ok(None);
        }
        Ok(Some(key))
    }

    fn ensure_session(&mut self, context: ProjectContext, send_initialize: bool) -> Result<()> {
        if self.sessions.contains_key(&context.key) {
            return Ok(());
        }

        let key = context.key.clone();
        let session_root = context.root.clone();
        let watch_root = watch_root_for_context(&context);
        let session = Session::spawn(context, self.events_tx.clone())?;

        if send_initialize {
            let initialize_params = self
                .client_initialize
                .as_ref()
                .map(|state| state.params.clone());
            if let Some(params) = initialize_params {
                let init_id = self.next_internal_request_id("init");
                self.initialize_routes.insert(
                    init_id.clone(),
                    InitializeRoute {
                        session_key: key.clone(),
                        forward_to_client: false,
                    },
                );

                let rewritten = rewrite_initialize_params(&params, session_root.as_deref())?;
                session.send(Request::new(init_id, "initialize".into(), rewritten).into())?;
            }
        }

        if let Some(root) = watch_root {
            self.workspace_watcher.watch(key.clone(), root);
        }
        self.sessions.insert(key, session);
        Ok(())
    }

    fn dispatch_to_session(&mut self, key: SessionKey, message: Message) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&key) else {
            if let Message::Request(request) = message {
                return self.respond_missing_target(request);
            }
            return self.log_warning(format!("dropped a message for a missing session: {key:?}"));
        };

        let sent = if session.initialized {
            session.send(message)
        } else {
            session.queued_messages.push(message);
            Ok(())
        };

        if sent.is_err() {
            // The writer thread is gone, so the child is dead or dying; fail
            // its pending requests instead of erroring out of the event loop.
            self.drop_session(&key, "tsgo child stopped accepting messages")?;
        }

        Ok(())
    }

    fn respond_missing_target(&mut self, request: Request) -> Result<()> {
        let message = format!("no tsgo session is available for {}", request.method);
        self.connection.sender.send(
            Response::new_err(
                request.id,
                ErrorCode::ServerErrorStart as i32,
                message.clone(),
            )
            .into(),
        )?;
        self.log_warning(message)?;
        Ok(())
    }

    fn broadcast_notification(&mut self, notification: Notification) -> Result<()> {
        // Route through dispatch_to_session so sessions still waiting on
        // their initialize response receive the notification once ready.
        let keys = self.sessions.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.dispatch_to_session(key, notification.clone().into())?;
        }

        Ok(())
    }

    fn broadcast_or_dispatch_initialized(&mut self, notification: Notification) -> Result<()> {
        if let Some(state) = self.client_initialize.as_mut() {
            state.initialized = Some(notification.clone());
        }

        if let Some(default_session) = self.default_session.clone() {
            self.dispatch_to_session(default_session, notification.into())?;
        }

        Ok(())
    }

    fn store_initialized(&mut self, notification: Notification) {
        if let Some(state) = self.client_initialize.as_mut() {
            state.initialized = Some(notification);
        }
    }

    fn handle_cancel_notification(&self, notification: &Notification) -> Result<()> {
        let Some(cancelled_id) = notification
            .params
            .get("id")
            .and_then(request_id_from_value)
        else {
            return Ok(());
        };

        let Some(session_key) = self.client_request_routes.get(&cancelled_id) else {
            return Ok(());
        };
        let Some(session) = self.sessions.get(session_key) else {
            return Ok(());
        };

        session.send(notification.clone().into())?;

        Ok(())
    }

    fn log_warning(&self, message: String) -> Result<()> {
        self.connection.sender.send(
            Notification::new(
                "window/logMessage".into(),
                json!({
                    "type": 2,
                    "message": message,
                }),
            )
            .into(),
        )?;
        Ok(())
    }

    fn next_internal_request_id(&mut self, prefix: &str) -> RequestId {
        self.internal_request_counter += 1;
        RequestId::from(format!(
            "tsgo-lsp/{prefix}/{}",
            self.internal_request_counter
        ))
    }

    fn drop_session(&mut self, session_key: &SessionKey, message: &str) -> Result<()> {
        self.documents.retain(|_, key| key != session_key);
        self.workspace_watcher.unwatch(session_key);

        let response_ids = self
            .client_request_routes
            .iter()
            .filter_map(|(request_id, key)| {
                if key == session_key {
                    Some(request_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for request_id in response_ids {
            self.client_request_routes.remove(&request_id);
            self.connection.sender.send(
                Response::new_err(
                    request_id,
                    ErrorCode::RequestFailed as i32,
                    message.to_owned(),
                )
                .into(),
            )?;
        }

        let initialize_ids = self
            .initialize_routes
            .iter()
            .filter_map(|(request_id, route)| {
                if &route.session_key == session_key && route.forward_to_client {
                    Some(request_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for request_id in initialize_ids {
            self.initialize_routes.remove(&request_id);
            self.connection.sender.send(
                Response::new_err(
                    request_id,
                    ErrorCode::RequestFailed as i32,
                    message.to_owned(),
                )
                .into(),
            )?;
        }

        self.initialize_routes
            .retain(|_, route| &route.session_key != session_key);
        self.child_request_routes
            .retain(|_, route| &route.session_key != session_key);

        if let Some(mut session) = self.sessions.remove(session_key) {
            // Terminate even when the child exited on its own: the wait()
            // inside reaps it, otherwise it lingers as a zombie.
            session.terminate();
        }

        if self.default_session.as_ref() == Some(session_key) {
            self.default_session = None;
        }

        self.log_warning(format!("{message}: {session_key:?}"))?;
        Ok(())
    }

    fn root_for_session(&self, key: &SessionKey) -> Option<&Path> {
        self.sessions
            .get(key)
            .and_then(|session| session.context.root.as_deref())
    }
}

enum Dispatch {
    Single(SessionKey),
    Broadcast,
    None,
}

fn find_initialize_root(params: &Value) -> Option<PathBuf> {
    params
        .get("rootUri")
        .and_then(Value::as_str)
        .and_then(|value| Url::parse(value).ok())
        .and_then(|value| value.to_file_path().ok())
        .or_else(|| {
            params
                .get("rootPath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .or_else(|| {
            params
                .get("workspaceFolders")
                .and_then(Value::as_array)
                .and_then(|folders| folders.first())
                .and_then(|folder| folder.get("uri"))
                .and_then(Value::as_str)
                .and_then(|value| Url::parse(value).ok())
                .and_then(|value| value.to_file_path().ok())
        })
}

fn rewrite_initialize_params(params: &Value, root: Option<&Path>) -> Result<Value> {
    let Some(root) = root else {
        return Ok(params.clone());
    };

    let mut params = params.clone();
    let Some(object) = params.as_object_mut() else {
        return Ok(params);
    };

    let root_uri = Url::from_directory_path(root)
        .or_else(|_| Url::from_file_path(root))
        .map_err(|_| anyhow!("failed to convert {} to a file URI", root.display()))?;

    object.insert("rootUri".into(), Value::String(root_uri.to_string()));
    object.insert(
        "rootPath".into(),
        Value::String(root.to_string_lossy().into_owned()),
    );
    object.insert(
        "workspaceFolders".into(),
        Value::Array(vec![json!({
            "uri": root_uri.to_string(),
            "name": root
                .file_name()
                .and_then(|segment| segment.to_str())
                .unwrap_or("workspace"),
        })]),
    );

    Ok(params)
}

fn watch_root_for_context(context: &ProjectContext) -> Option<PathBuf> {
    if let Some(root) = context.root.clone() {
        return Some(root);
    }

    context.command.cwd.clone()
}

fn find_uri(params: &Value) -> Option<Url> {
    match params {
        Value::Object(map) => map
            .get("uri")
            .and_then(Value::as_str)
            .and_then(|value| Url::parse(value).ok())
            .or_else(|| map.values().find_map(find_uri)),
        Value::Array(values) => values.iter().find_map(find_uri),
        _ => None,
    }
}

fn normalize_child_response(mut response: Response) -> Response {
    if response.result.is_none() && response.error.is_none() {
        response.result = Some(Value::Null);
    }

    response
}

fn request_id_from_value(value: &Value) -> Option<RequestId> {
    if let Some(number) = value.as_i64() {
        let number = i32::try_from(number).ok()?;
        return Some(RequestId::from(number));
    }

    value
        .as_str()
        .map(|value| RequestId::from(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use lsp_server::{Message, RequestId, Response};

    use super::normalize_child_response;

    #[test]
    fn serializes_resultless_child_response_with_null_result() {
        let response = normalize_child_response(Response {
            id: RequestId::from(7),
            result: None,
            error: None,
        });

        let mut bytes = Vec::new();
        Message::Response(response).write(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":null}"));
    }
}
