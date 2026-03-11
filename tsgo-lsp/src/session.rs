use std::{
    io::{BufRead, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use lsp_server::Message;
use tracing::{error, info, warn};

use crate::discovery::{ProjectContext, SessionKey};

#[derive(Debug)]
pub enum SessionEvent {
    Message(SessionKey, Message),
    Closed(SessionKey),
}

pub struct Session {
    pub context: ProjectContext,
    pub initialized: bool,
    pub queued_messages: Vec<Message>,
    writer: Sender<Message>,
    child: Child,
}

impl Session {
    pub fn spawn(context: ProjectContext, events: Sender<SessionEvent>) -> Result<Self> {
        let mut command = Command::new(&context.command.program);
        command
            .args(&context.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(cwd) = &context.command.cwd {
            command.current_dir(cwd);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", context.command.program.display()))?;

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
        spawn_writer(context.key.clone(), stdin, receiver);
        spawn_reader(context.key.clone(), stdout, events.clone());
        spawn_stderr_logger(context.key.clone(), stderr);

        info!(
            key = ?context.key,
            program = %context.command.program.display(),
            "spawned tsgo child session"
        );

        Ok(Self {
            context,
            initialized: false,
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
                    warn!(key = ?self.context.key, "failed to kill child session: {error}");
                }
                let _ = self.child.wait();
            }
            Err(error) => warn!(key = ?self.context.key, "failed to query child status: {error}"),
        }
    }
}

fn spawn_writer(
    key: SessionKey,
    stdin: ChildStdin,
    receiver: crossbeam_channel::Receiver<Message>,
) {
    thread::Builder::new()
        .name(format!("tsgo-writer-{key:?}"))
        .spawn(move || {
            let mut stdin = BufWriter::new(stdin);
            for message in receiver {
                if let Err(error) = message.write(&mut stdin) {
                    error!(key = ?key, "failed to write to child session: {error}");
                    break;
                }
            }
        })
        .expect("writer thread should spawn");
}

fn spawn_reader(key: SessionKey, stdout: ChildStdout, events: Sender<SessionEvent>) {
    thread::Builder::new()
        .name(format!("tsgo-reader-{key:?}"))
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);

            loop {
                match Message::read(&mut stdout) {
                    Ok(Some(message)) => {
                        if events
                            .send(SessionEvent::Message(key.clone(), message))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        error!(key = ?key, "failed to read child session message: {error}");
                        break;
                    }
                }
            }

            let _ = events.send(SessionEvent::Closed(key));
        })
        .expect("reader thread should spawn");
}

fn spawn_stderr_logger(key: SessionKey, stderr: ChildStderr) {
    thread::Builder::new()
        .name(format!("tsgo-stderr-{key:?}"))
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => warn!(key = ?key, "tsgo stderr: {line}"),
                    Err(error) => {
                        error!(key = ?key, "failed to read child stderr: {error}");
                        break;
                    }
                }
            }
        })
        .expect("stderr thread should spawn");
}
