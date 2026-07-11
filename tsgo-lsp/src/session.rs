use std::{
    io::{BufRead, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use lsp_server::Message;
use tracing::{error, info, warn};

use crate::discovery::{ProjectContext, SessionKey};

// Sessions can be dropped and respawned under the same key; the generation
// lets the proxy ignore events from a previous child that shares the key.
static NEXT_SESSION_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum SessionEvent {
    Message(SessionKey, u64, Message),
    Closed(SessionKey, u64),
}

pub struct Session {
    pub context: ProjectContext,
    pub generation: u64,
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

        let generation = NEXT_SESSION_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (writer, receiver) = crossbeam_channel::unbounded();
        if let Err(error) = spawn_writer(context.key.clone(), stdin, receiver)
            .and_then(|()| spawn_reader(context.key.clone(), generation, stdout, events.clone()))
            .and_then(|()| spawn_stderr_logger(context.key.clone(), stderr))
        {
            terminate_child(&mut child);
            return Err(error);
        }

        info!(
            key = ?context.key,
            program = %context.command.program.display(),
            "spawned tsgo child session"
        );

        Ok(Self {
            context,
            generation,
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
    key: SessionKey,
    stdin: ChildStdin,
    receiver: crossbeam_channel::Receiver<Message>,
) -> Result<()> {
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
        .context("failed to spawn tsgo writer thread")?;

    Ok(())
}

fn spawn_reader(
    key: SessionKey,
    generation: u64,
    stdout: ChildStdout,
    events: Sender<SessionEvent>,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("tsgo-reader-{key:?}"))
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);

            loop {
                match Message::read(&mut stdout) {
                    Ok(Some(message)) => {
                        if events
                            .send(SessionEvent::Message(key.clone(), generation, message))
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

            let _ = events.send(SessionEvent::Closed(key, generation));
        })
        .context("failed to spawn tsgo reader thread")?;

    Ok(())
}

fn spawn_stderr_logger(key: SessionKey, stderr: ChildStderr) -> Result<()> {
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
        .context("failed to spawn tsgo stderr thread")?;

    Ok(())
}
