//! The client: run one command through the server and bring its output back.
//!
//! The coder uses it in its own process (`adam-workspace`), and the `adam-git` relay is a command
//! line around it. A client **never falls back**: when the socket cannot be reached the command is
//! an error ([`ClientError::Unreachable`]) and git is not started anywhere else.
//!
//! Dropping the future of a call closes the connection, and the server kills the command's process
//! group: a timeout around [`Client::output`] ends the command too.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::UnixStream;

use crate::error::ClientError;
use crate::protocol::{
    ClientFrame, DATA_CHUNK, Exit, ProtocolError, Request, ServerFrame, read_server_frame,
    write_client_frame,
};

/// A command's result, as [`Client::output`] collects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// How the command ended.
    pub exit: Exit,
    /// What it wrote to its standard output.
    pub stdout: Vec<u8>,
    /// What it wrote to its standard error.
    pub stderr: Vec<u8>,
}

/// A handle on the server's socket. Cheap to clone; it holds a path and opens a connection for each
/// command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    /// A client of the server at `socket`. Nothing is connected until a command is run.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Run `request` with `stdin` as its standard input and collect its output.
    ///
    /// # Errors
    ///
    /// [`ClientError`]: the server cannot be reached, refuses the command, or the conversation
    /// fails. A command that ran and failed is not an error: it is an [`Output`] with its exit
    /// status.
    pub async fn output(&self, request: Request, stdin: &[u8]) -> Result<Output, ClientError> {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = self.run(request, stdin, &mut stdout, &mut stderr).await?;
        Ok(Output {
            exit,
            stdout,
            stderr,
        })
    }

    /// Run `request`, feeding it `stdin` until that ends and copying its output to `stdout` and
    /// `stderr` as it arrives. Returns how it ended.
    ///
    /// The command's standard input is read concurrently with its output and is not waited for: a
    /// command that ends while `stdin` has more to give ends the call.
    ///
    /// # Errors
    ///
    /// As [`output`](Self::output); and [`ClientError::Output`] when `stdout` or `stderr` fail.
    pub async fn run<I, O, E>(
        &self,
        request: Request,
        stdin: I,
        stdout: &mut O,
        stderr: &mut E,
    ) -> Result<Exit, ClientError>
    where
        I: AsyncRead + Unpin,
        O: AsyncWrite + Unpin,
        E: AsyncWrite + Unpin,
    {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| ClientError::Unreachable {
                    socket: self.socket.clone(),
                    source,
                })?;
        let (mut reader, mut writer) = stream.into_split();
        write_client_frame(&mut writer, &ClientFrame::Request(request))
            .await
            .map_err(ClientError::Protocol)?;

        // The input is sent from a future of its own, so that a command that answers before it has
        // read everything is not waited for, and one that reads slowly does not hold its output up.
        let feed = async move {
            let mut stdin = stdin;
            let mut chunk = vec![0u8; DATA_CHUNK];
            loop {
                // An input that fails is an input that ends: the command is told so.
                let n = stdin.read(&mut chunk).await.unwrap_or_default();
                let frame = if n == 0 {
                    ClientFrame::StdinEnd
                } else {
                    ClientFrame::Stdin(chunk[..n].to_vec())
                };
                if write_client_frame(&mut writer, &frame).await.is_err() || n == 0 {
                    break;
                }
            }
            // Kept open: the server reads the end of the connection as "the client has gone". The
            // writer lives as long as this future, which the end of the call drops.
            std::future::pending::<()>().await;
        };
        tokio::pin!(feed);

        let receive = async {
            loop {
                match read_server_frame(&mut reader).await {
                    Ok(Some(ServerFrame::Stdout(bytes))) => {
                        stdout
                            .write_all(&bytes)
                            .await
                            .map_err(ClientError::Output)?;
                    }
                    Ok(Some(ServerFrame::Stderr(bytes))) => {
                        stderr
                            .write_all(&bytes)
                            .await
                            .map_err(ClientError::Output)?;
                    }
                    Ok(Some(ServerFrame::Exit(exit))) => break Ok(exit),
                    Ok(Some(ServerFrame::Refused(refusal))) => {
                        break Err(ClientError::Refused(refusal));
                    }
                    Ok(None) => break Err(ClientError::Disconnected),
                    Err(ProtocolError::Truncated) => break Err(ClientError::Disconnected),
                    Err(e) => break Err(ClientError::Protocol(e)),
                }
            }
        };
        let result = tokio::select! {
            result = receive => result,
            // `feed` waits for ever after its last frame: this branch is never taken.
            () = &mut feed => Err(ClientError::Disconnected),
        };
        stdout.flush().await.map_err(ClientError::Output)?;
        stderr.flush().await.map_err(ClientError::Output)?;
        result
    }
}
