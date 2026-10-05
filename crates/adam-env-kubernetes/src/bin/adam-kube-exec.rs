//! `adam-kube-exec`: run one command in a run pod and end with its exit code.
//!
//! The coder spawns it for every command of a run that runs in a pod (it is what
//! [`PreparedCommand`](adam_workspace::PreparedCommand) of the Kubernetes environment names), in a
//! process group of its own, with stdin and stdout piped as for any command. It runs
//! `adam-exec run|shell <id> <cwd> :<word>...` in the run container with `pods/exec`, copies stdin
//! to it, copies its stdout and stderr back, and exits with the command's exit code.
//!
//! ```text
//! adam-kube-exec --namespace NS --pod POD [--container NAME] [--adam-exec PATH]
//!                [--env NAME=VALUE]... [--unset NAME]... run|shell ID CWD :WORD...
//! ```
//!
//! It reaches the cluster as the coder's ServiceAccount does (`KUBERNETES_SERVICE_HOST` and the
//! mounted token), or through `KUBECONFIG` in development. The coder starts it with an empty
//! environment and only those variables.
//!
//! | Exit code | Meaning |
//! |---|---|
//! | the command's own | it ran and ended (0 is success); a command killed by a signal has 128 plus the signal |
//! | 64 | the command line is wrong (nothing was run) |
//! | 69 | the cluster could not run it, or the connection ended before the command did |
//! | 129, 130, 143 | the client itself was hung up, interrupted or terminated (the command in the pod is stopped by `adam-exec kill`, which the coder asks for) |
//!
//! The values 64 and 69 are those of BSD `sysexits.h` (`EX_USAGE`, `EX_UNAVAILABLE`), *unverified*
//! (from memory), as the coder's own exit codes are.

use adam_env_kubernetes::{
    ExitStatus, Invocation, Target, install_crypto_provider, stdin_is_null, stream,
};
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};
use tokio::signal::unix::{SignalKind, signal};

/// The command line is wrong.
const EX_USAGE: u8 = 64;
/// The cluster could not run the command.
const EX_UNAVAILABLE: u8 = 69;

fn fail(code: u8, message: &str) -> u8 {
    eprintln!("adam-kube-exec: {message}");
    code
}

/// Exit without waiting for the runtime: a read of stdin that nobody will finish is a blocking
/// task, and dropping the runtime would wait for it.
fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => std::process::exit(i32::from(fail(EX_UNAVAILABLE, &format!("no runtime: {e}")))),
    };
    let code = runtime.block_on(run());
    std::process::exit(i32::from(code));
}

async fn run() -> u8 {
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(_) => return fail(EX_USAGE, "an argument is not UTF-8"),
        }
    }
    let invocation = match Invocation::parse(args) {
        Ok(invocation) => invocation,
        Err(e) => return fail(EX_USAGE, &e.to_string()),
    };
    // The image's binaries are built together, so this binary's tree can enable two TLS providers.
    install_crypto_provider();
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            return fail(EX_UNAVAILABLE, &format!("cannot reach the cluster: {e}"));
        }
    };
    let pods: Api<Pod> = Api::namespaced(client, &invocation.namespace);
    let with_stdin = !stdin_is_null();
    let target = Target {
        pod: &invocation.pod,
        container: invocation.container.as_deref(),
        argv: invocation.remote_argv(),
        stdin: with_stdin,
    };

    let (Ok(mut hangup), Ok(mut interrupt), Ok(mut terminate)) = (
        signal(SignalKind::hangup()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        return fail(EX_UNAVAILABLE, "cannot listen for signals");
    };
    let command = stream(
        &pods,
        target,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
    );
    tokio::select! {
        ended = command => match ended {
            Ok(ExitStatus::Code(code)) => u8::try_from(code & 0xff).unwrap_or(1),
            Ok(ExitStatus::Failed(why)) => fail(EX_UNAVAILABLE, &why),
            Err(e) => fail(EX_UNAVAILABLE, &e.to_string()),
        },
        _ = hangup.recv() => 129,
        _ = interrupt.recv() => 130,
        _ = terminate.recv() => 143,
    }
}
