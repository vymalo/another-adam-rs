//! `adam-git`: stands in for `git` and runs it in the git sandbox.
//!
//! ```text
//! GIT_SANDBOX_SOCKET=/run/git/git.sock adam-git status --porcelain
//! ```
//!
//! It sends its arguments (everything after its own name is git's), its working directory, the
//! variables of its environment that the sandbox allows and its standard input over the socket, and
//! copies the command's output to its own. It ends with git's exit code (128 plus the signal for a
//! command that a signal killed). **It never runs git itself**: with no socket, or no one on it, it
//! says so and ends.
//!
//! | Exit code | Meaning |
//! |---|---|
//! | the command's own | it ran and ended |
//! | 78 | `GIT_SANDBOX_SOCKET` is not set, or not an absolute path (nothing was run) |
//! | 69 | nobody answers at the socket (nothing was run) |
//! | 77 | the sandbox refused the command, for instance a directory outside its root (nothing was run) |
//! | 74 | the conversation failed: the connection ended before the command did, or broke the protocol |
//! | 141 | the output could not be written (a closed pipe) |

use std::ffi::OsString;
use std::process::ExitCode;

use adam_git_sandbox::{
    Client, ClientError, EX_CONFIG, EX_IOERR, Request, SOCKET_ENV, environment, socket_from_env,
};

/// The code of a command whose output has nowhere to go: what a shell reports for SIGPIPE.
const EX_PIPE: u8 = 141;

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("adam-git: {message}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    // Not `#[tokio::main]`: a read of standard input that nobody finishes is a blocking task, and
    // dropping the runtime would wait for it. The process exits without waiting.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(EX_IOERR, &format!("no runtime: {e}")),
    };
    let code = runtime.block_on(run());
    std::process::exit(i32::from(code));
}

async fn run() -> u8 {
    let socket = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(Some(socket)) => socket,
        Ok(None) => {
            eprintln!(
                "adam-git: invalid configuration: {SOCKET_ENV} is not set; adam-git runs git only in the git sandbox and never itself"
            );
            return EX_CONFIG;
        }
        Err(message) => {
            eprintln!("adam-git: invalid configuration: {message}");
            return EX_CONFIG;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("adam-git: cannot tell the working directory: {e}");
            return EX_IOERR;
        }
    };
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    // The client drops what the sandbox would drop: a secret of this environment does not leave it.
    let env = environment::filter(std::env::vars_os())
        .into_iter()
        .filter(|(name, _)| name != "PATH")
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)));
    let request = Request::new(argv, cwd, env);

    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let ran = Client::new(socket)
        .run(request, tokio::io::stdin(), &mut stdout, &mut stderr)
        .await;
    match ran {
        Ok(exit) => exit.shell_code(),
        Err(ClientError::Output(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => EX_PIPE,
        Err(e) => {
            eprintln!("adam-git: {}", adam_error::report(&e));
            e.exit_code()
        }
    }
}
