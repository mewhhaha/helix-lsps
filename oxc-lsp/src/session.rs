use std::{
    io::{BufRead, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use lsp_server::Message;
use tracing::{error, info, warn};

use crate::discovery::{CommandSpec, SessionKey};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ChildKind {
    Lint,
    Format,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId {
    pub project: SessionKey,
    pub kind: ChildKind,
}

#[derive(Debug)]
pub enum SessionEvent {
    Message(SessionId, u64, Message),
    Closed(SessionId, u64),
}

pub struct Session {
    pub id: SessionId,
    pub root: Option<std::path::PathBuf>,
    pub initialized: bool,
    pub generation: u64,
    pub queued_messages: Vec<Message>,
    writer: Sender<Message>,
    child: Child,
}

impl Session {
    pub fn spawn(
        id: SessionId,
        root: Option<std::path::PathBuf>,
        command_spec: &CommandSpec,
        generation: u64,
        events: Sender<SessionEvent>,
    ) -> Result<Self> {
        let mut command = Command::new(&command_spec.program);
        command
            .args(&command_spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(cwd) = &command_spec.cwd {
            command.current_dir(cwd);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", command_spec.program.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to acquire child stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to acquire child stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to acquire child stderr"))?;

        let (writer, receiver) = crossbeam_channel::unbounded();
        if let Err(error) = spawn_writer(id.clone(), stdin, receiver)
            .and_then(|()| spawn_reader(id.clone(), generation, stdout, events.clone()))
            .and_then(|()| spawn_stderr_logger(id.clone(), stderr))
        {
            terminate_child(&mut child);
            return Err(error);
        }

        info!(
            id = ?id,
            program = %command_spec.program.display(),
            "spawned oxc child session"
        );

        Ok(Self {
            id,
            root,
            initialized: false,
            generation,
            queued_messages: Vec::new(),
            writer,
            child,
        })
    }

    pub fn send(&self, message: Message) -> Result<()> {
        self.writer
            .send(message)
            .map_err(|error| anyhow!("failed to forward message to session: {error}"))
    }

    pub fn drain_queue(&mut self) -> Result<()> {
        let queued = std::mem::take(&mut self.queued_messages);
        for message in queued {
            self.send(message)?;
        }
        Ok(())
    }

    pub fn terminate(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    warn!(id = ?self.id, "failed to kill child session: {error}");
                }
                let _ = self.child.wait();
            }
            Err(error) => warn!(id = ?self.id, "failed to query child status: {error}"),
        }
    }
}

fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        Err(_) => {}
    }
}

fn spawn_writer(
    id: SessionId,
    stdin: ChildStdin,
    receiver: crossbeam_channel::Receiver<Message>,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("oxc-writer-{id:?}"))
        .spawn(move || {
            let mut stdin = BufWriter::new(stdin);
            for message in receiver {
                if let Err(error) = message.write(&mut stdin) {
                    error!(id = ?id, "failed to write to child session: {error}");
                    break;
                }
            }
        })
        .context("failed to spawn oxc writer thread")?;

    Ok(())
}

fn spawn_reader(
    id: SessionId,
    generation: u64,
    stdout: ChildStdout,
    events: Sender<SessionEvent>,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("oxc-reader-{id:?}"))
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);

            loop {
                match Message::read(&mut stdout) {
                    Ok(Some(message)) => {
                        if events
                            .send(SessionEvent::Message(id.clone(), generation, message))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        error!(id = ?id, "failed to read child session message: {error}");
                        break;
                    }
                }
            }

            let _ = events.send(SessionEvent::Closed(id, generation));
        })
        .context("failed to spawn oxc reader thread")?;

    Ok(())
}

fn spawn_stderr_logger(id: SessionId, stderr: ChildStderr) -> Result<()> {
    thread::Builder::new()
        .name(format!("oxc-stderr-{id:?}"))
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => warn!(id = ?id, "oxc child stderr: {line}"),
                    Err(error) => {
                        error!(id = ?id, "failed to read child stderr: {error}");
                        break;
                    }
                }
            }
        })
        .context("failed to spawn oxc stderr thread")?;

    Ok(())
}
