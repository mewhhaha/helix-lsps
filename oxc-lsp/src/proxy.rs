use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, select, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;

use crate::{
    discovery::{CommandSpec, Discovery, ProjectContext, SessionKey},
    session::{ChildKind, Session, SessionEvent, SessionId},
};

pub fn run() -> Result<()> {
    init_tracing();

    let (connection, io_threads) = Connection::stdio();
    match Proxy::new(connection).run() {
        Ok(()) => {
            io_threads.join().context("failed to join stdio threads")?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("oxc_lsp=info"))
        .unwrap();

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

struct Proxy {
    connection: Connection,
    discovery: Discovery,
    sessions: HashMap<SessionId, Session>,
    documents: HashMap<String, SessionKey>,
    client_request_routes: HashMap<RequestId, SessionId>,
    child_request_routes: HashMap<RequestId, ChildRequestRoute>,
    initialize_routes: HashMap<RequestId, InitializeRoute>,
    client_initialize: Option<ClientInitializeState>,
    default_project: Option<SessionKey>,
    internal_request_counter: usize,
    events_tx: Sender<SessionEvent>,
    events_rx: Receiver<SessionEvent>,
    shutdown_requested: bool,
}

#[derive(Clone)]
struct ClientInitializeState {
    params: Value,
    initialized: Option<Notification>,
}

struct ChildRequestRoute {
    session_id: SessionId,
    child_request_id: RequestId,
}

struct InitializeRoute {
    session_id: SessionId,
    forward_to_client: bool,
}

impl Proxy {
    fn new(connection: Connection) -> Self {
        let (events_tx, events_rx) = unbounded();

        Self {
            connection,
            discovery: Discovery,
            sessions: HashMap::new(),
            documents: HashMap::new(),
            client_request_routes: HashMap::new(),
            child_request_routes: HashMap::new(),
            initialize_routes: HashMap::new(),
            client_initialize: None,
            default_project: None,
            internal_request_counter: 0,
            events_tx,
            events_rx,
            shutdown_requested: false,
        }
    }

    fn run(mut self) -> Result<()> {
        loop {
            select! {
                recv(self.connection.receiver) -> message => {
                    let Ok(message) = message else {
                        break;
                    };

                    let should_exit = self.handle_client_message(message)?;
                    if should_exit {
                        break;
                    }
                }
                recv(self.events_rx) -> event => {
                    let Ok(event) = event else {
                        break;
                    };
                    self.handle_session_event(event)?;
                }
            }
        }

        Ok(())
    }

    fn handle_client_message(&mut self, message: Message) -> Result<bool> {
        match message {
            Message::Request(request) => {
                if request.method == "initialize" {
                    self.handle_initialize(request)?;
                    return Ok(false);
                }

                if request.method == "shutdown" {
                    self.shutdown_requested = true;
                }

                let target = self.resolve_target_for_request(&request)?;
                let Some(target) = target else {
                    self.respond_missing_target(request)?;
                    return Ok(false);
                };

                self.client_request_routes
                    .insert(request.id.clone(), target.clone());
                self.dispatch_to_session(target, request.into())?;
            }
            Message::Notification(notification) => {
                if notification.method == "initialized" {
                    self.store_initialized(notification.clone());
                    self.broadcast_or_dispatch_initialized(notification)?;
                    return Ok(false);
                }

                if notification.method == "exit" {
                    self.broadcast_notification(notification);
                    return Ok(true);
                }

                if notification.method == "$/cancelRequest" {
                    self.handle_cancel_notification(&notification)?;
                }

                match self.resolve_target_for_notification(&notification)? {
                    Dispatch::Single(project_key) => {
                        self.dispatch_project_notification(project_key, notification)?
                    }
                    Dispatch::Broadcast => self.broadcast_notification(notification),
                    Dispatch::None => {}
                }
            }
            Message::Response(response) => {
                if let Some(route) = self.child_request_routes.remove(&response.id) {
                    if let Some(session) = self.sessions.get(&route.session_id) {
                        session.send(Message::Response(Response {
                            id: route.child_request_id,
                            result: response.result,
                            error: response.error,
                        }))?;
                    }
                }
            }
        }

        Ok(false)
    }

    fn handle_session_event(&mut self, event: SessionEvent) -> Result<()> {
        match event {
            SessionEvent::Message(session_id, message) => match message {
                Message::Request(request) => self.handle_child_request(session_id, request)?,
                Message::Response(response) => self.handle_child_response(session_id, response)?,
                Message::Notification(notification) => {
                    self.connection.sender.send(notification.into())?;
                }
            },
            SessionEvent::Closed(session_id) => {
                let message = match session_id.kind {
                    ChildKind::Lint => "oxlint child exited unexpectedly",
                    ChildKind::Format => "oxfmt child exited unexpectedly",
                };
                self.drop_session(&session_id, message, false)?;
            }
        }

        Ok(())
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

        let initial_path = find_initialize_root(&params)
            .unwrap_or_else(|| std::env::current_dir().expect("cwd should be available"));
        let Some(context) = self.discovery.maybe_context_for_uri_path(&initial_path)? else {
            self.connection
                .sender
                .send(Response::new_ok(request.id, silent_initialize_result()).into())?;
            return Ok(());
        };
        let project_key = context.key.clone();

        self.ensure_project_sessions(&context, false)?;
        self.default_project = Some(project_key.clone());

        let lint_session_id = SessionId {
            project: project_key.clone(),
            kind: ChildKind::Lint,
        };
        self.initialize_routes.insert(
            request.id.clone(),
            InitializeRoute {
                session_id: lint_session_id.clone(),
                forward_to_client: true,
            },
        );

        let initialize = Request {
            id: request.id.clone(),
            method: request.method,
            params: rewrite_initialize_params(&params, self.root_for_session(&lint_session_id))?,
        };

        self.sessions
            .get(&lint_session_id)
            .ok_or_else(|| anyhow!("default lint session disappeared during initialize"))?
            .send(initialize.into())?;

        if context.format_command.is_some() {
            self.ensure_formatter_initialized(&context)?;
        }

        Ok(())
    }

    fn handle_child_request(&mut self, session_id: SessionId, request: Request) -> Result<()> {
        let client_id = self.next_internal_request_id("client");
        self.child_request_routes.insert(
            client_id.clone(),
            ChildRequestRoute {
                session_id,
                child_request_id: request.id,
            },
        );

        self.connection
            .sender
            .send(Request::new(client_id, request.method, request.params).into())?;
        Ok(())
    }

    fn handle_child_response(&mut self, _session_id: SessionId, response: Response) -> Result<()> {
        let response = normalize_child_response(response);

        if let Some(route) = self.initialize_routes.remove(&response.id) {
            let initialized = response.error.is_none();
            let initialize_error_message = response.error.as_ref().map(|error| error.message.clone());
            let has_formatter = self.sessions.contains_key(&SessionId {
                project: route.session_id.project.clone(),
                kind: ChildKind::Format,
            });
            let Some(session) = self.sessions.get_mut(&route.session_id) else {
                return Ok(());
            };

            session.initialized = initialized;
            if route.forward_to_client {
                let forwarded = if route.session_id.kind == ChildKind::Lint {
                    normalize_initialize_response_for_client(response.clone(), has_formatter)
                } else {
                    response.clone()
                };
                self.connection.sender.send(forwarded.into())?;
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
            } else if route.session_id.kind == ChildKind::Lint {
                let message = initialize_error_message
                    .as_ref()
                    .map(|message| format!("oxlint initialize failed: {message}"))
                    .unwrap_or_else(|| "oxlint initialize failed".to_owned());
                self.drop_session(&route.session_id, &message, true)?;
            } else {
                let message = initialize_error_message
                    .as_ref()
                    .map(|message| format!("oxfmt initialize failed: {message}"))
                    .unwrap_or_else(|| "oxfmt initialize failed".to_owned());
                self.drop_session(&route.session_id, &message, true)?;
            }

            return Ok(());
        }

        self.client_request_routes.remove(&response.id);
        self.connection.sender.send(response.into())?;
        Ok(())
    }

    fn resolve_target_for_request(&mut self, request: &Request) -> Result<Option<SessionId>> {
        let child_kind = if request.method == "textDocument/formatting" {
            ChildKind::Format
        } else {
            ChildKind::Lint
        };

        let project_key = if let Some(uri) = find_uri(&request.params) {
            self.resolve_target_for_uri(&uri)?
        } else {
            self.default_project.clone()
        };

        Ok(project_key.and_then(|project| {
            let session_id = SessionId {
                project,
                kind: child_kind,
            };

            self.sessions.contains_key(&session_id).then_some(session_id)
        }))
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
                if let Some(project_key) = target.clone() {
                    self.documents
                        .insert(uri.as_str().to_owned(), project_key.clone());
                }
            }
            return Ok(target.map_or(Dispatch::None, Dispatch::Single));
        }

        Ok(self.default_project.clone().map_or(Dispatch::None, Dispatch::Single))
    }

    fn resolve_target_for_uri(&mut self, uri: &Url) -> Result<Option<SessionKey>> {
        if let Some(project_key) = self.documents.get(uri.as_str()).cloned() {
            return Ok(Some(project_key));
        }

        let file_path = match uri.to_file_path() {
            Ok(file_path) => file_path,
            Err(_) => return Ok(None),
        };

        let Some(context) = self.discovery.maybe_context_for_uri_path(&file_path)? else {
            return Ok(None);
        };
        let project_key = context.key.clone();
        self.ensure_project_sessions(&context, true)?;
        Ok(Some(project_key))
    }

    fn ensure_project_sessions(&mut self, context: &ProjectContext, send_initialize: bool) -> Result<()> {
        self.ensure_session(
            SessionId {
                project: context.key.clone(),
                kind: ChildKind::Lint,
            },
            context.root.clone(),
            &context.lint_command,
            send_initialize,
        )?;

        if context.format_command.is_some() {
            self.ensure_formatter_initialized(context)?;
        }

        Ok(())
    }

    fn ensure_formatter_initialized(&mut self, context: &ProjectContext) -> Result<()> {
        let Some(format_command) = context.format_command.as_ref() else {
            return Ok(());
        };

        self.ensure_session(
            SessionId {
                project: context.key.clone(),
                kind: ChildKind::Format,
            },
            context.root.clone(),
            format_command,
            true,
        )
    }

    fn ensure_session(
        &mut self,
        session_id: SessionId,
        root: Option<PathBuf>,
        command_spec: &CommandSpec,
        send_initialize: bool,
    ) -> Result<()> {
        if self.sessions.contains_key(&session_id) {
            return Ok(());
        }

        let session = Session::spawn(session_id.clone(), root.clone(), command_spec, self.events_tx.clone())?;

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
                        session_id: session_id.clone(),
                        forward_to_client: false,
                    },
                );

                let rewritten = rewrite_initialize_params(&params, root.as_deref())?;
                session.send(Request::new(init_id, "initialize".into(), rewritten).into())?;
            }
        }

        self.sessions.insert(session_id, session);
        Ok(())
    }

    fn dispatch_to_session(&mut self, session_id: SessionId, message: Message) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Err(anyhow!("attempted to route to a missing session"));
        };

        let is_initialized_notification = matches!(
            &message,
            Message::Notification(notification) if notification.method == "initialized"
        );

        if session.initialized {
            session.send(message)?;
        } else if is_initialized_notification {
            // Defer client `initialized` until the child responds to its own initialize request.
            // That keeps late-starting formatter sessions from seeing the notification twice.
        } else {
            session.queued_messages.push(message);
        }

        Ok(())
    }

    fn dispatch_project_notification(
        &mut self,
        project_key: SessionKey,
        notification: Notification,
    ) -> Result<()> {
        for kind in [ChildKind::Lint, ChildKind::Format] {
            let session_id = SessionId {
                project: project_key.clone(),
                kind,
            };

            if self.sessions.contains_key(&session_id) {
                self.dispatch_to_session(session_id, notification.clone().into())?;
            }
        }

        Ok(())
    }

    fn respond_missing_target(&mut self, request: Request) -> Result<()> {
        let result = if request.method == "textDocument/formatting"
            || request.method == "textDocument/codeAction"
        {
            Value::Array(Vec::new())
        } else {
            Value::Null
        };
        self.connection
            .sender
            .send(Response::new_ok(request.id, result).into())?;
        Ok(())
    }

    fn broadcast_notification(&self, notification: Notification) {
        for session in self.sessions.values() {
            if session.initialized {
                let _ = session.send(notification.clone().into());
            }
        }
    }

    fn broadcast_or_dispatch_initialized(&mut self, notification: Notification) -> Result<()> {
        if let Some(state) = self.client_initialize.as_mut() {
            state.initialized = Some(notification.clone());
        }

        if let Some(default_project) = self.default_project.clone() {
            self.dispatch_project_notification(default_project, notification)?;
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

        if let Some(session_id) = self.client_request_routes.get(&cancelled_id) {
            if let Some(session) = self.sessions.get(session_id) {
                session.send(notification.clone().into())?;
            }
        }

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
            "oxc-lsp/{prefix}/{}",
            self.internal_request_counter
        ))
    }

    fn drop_session(&mut self, session_id: &SessionId, message: &str, terminate: bool) -> Result<()> {
        if session_id.kind == ChildKind::Lint {
            self.documents.retain(|_, key| key != &session_id.project);
        }

        let response_ids = self
            .client_request_routes
            .iter()
            .filter_map(|(request_id, id)| {
                if id == session_id {
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
                if &route.session_id == session_id && route.forward_to_client {
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
            .retain(|_, route| &route.session_id != session_id);
        self.child_request_routes
            .retain(|_, route| &route.session_id != session_id);

        if let Some(mut session) = self.sessions.remove(session_id) {
            if terminate {
                session.terminate();
            }
        }

        if session_id.kind == ChildKind::Lint && self.default_project.as_ref() == Some(&session_id.project) {
            self.default_project = None;
        }

        self.log_warning(format!("{message}: {session_id:?}"))?;
        Ok(())
    }

    fn root_for_session(&self, session_id: &SessionId) -> Option<&Path> {
        self.sessions.get(session_id).and_then(|session| session.root.as_deref())
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

fn normalize_initialize_response_for_client(
    mut response: Response,
    has_formatter: bool,
) -> Response {
    let Some(result) = response.result.as_mut() else {
        return response;
    };
    let Some(object) = result.as_object_mut() else {
        return response;
    };

    let capabilities = object
        .entry("capabilities")
        .or_insert_with(|| json!({}));
    if has_formatter {
        if let Some(capabilities) = capabilities.as_object_mut() {
            capabilities
                .entry("documentFormattingProvider")
                .or_insert_with(|| Value::Bool(true));
        }
    }

    let version = object
        .get("serverInfo")
        .and_then(Value::as_object)
        .and_then(|server_info| server_info.get("version"))
        .cloned();
    object.insert(
        "serverInfo".into(),
        json!({
            "name": "oxc-lsp",
            "version": version.unwrap_or(Value::String("0.1.0".into())),
        }),
    );

    response
}

fn silent_initialize_result() -> Value {
    json!({
        "capabilities": {},
        "serverInfo": {
            "name": "oxc-lsp",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
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
    use serde_json::json;

    use super::{
        normalize_child_response, normalize_initialize_response_for_client, silent_initialize_result,
    };

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

    #[test]
    fn adds_formatting_capability_to_initialize_result_when_formatter_exists() {
        let response = normalize_initialize_response_for_client(
            Response::new_ok(
                RequestId::from(1),
                json!({
                    "capabilities": {
                        "hoverProvider": true
                    },
                    "serverInfo": {
                        "name": "oxlint",
                        "version": "1.2.3"
                    }
                }),
            ),
            true,
        );

        assert_eq!(
            response.result.unwrap()["capabilities"]["documentFormattingProvider"],
            json!(true)
        );
    }

    #[test]
    fn leaves_formatting_capability_disabled_when_formatter_is_missing() {
        let response = normalize_initialize_response_for_client(
            Response::new_ok(
                RequestId::from(1),
                json!({
                    "capabilities": {
                        "hoverProvider": true
                    }
                }),
            ),
            false,
        );

        assert!(
            response.result.unwrap()["capabilities"]
                .get("documentFormattingProvider")
                .is_none()
        );
    }

    #[test]
    fn silent_initialize_result_has_no_capabilities() {
        assert_eq!(silent_initialize_result()["capabilities"], json!({}));
    }
}
