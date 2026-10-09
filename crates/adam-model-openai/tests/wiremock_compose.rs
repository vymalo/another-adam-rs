//! `OpenAiCompatible` against the `mock-openai` WireMock of `compose.yaml`.
//!
//! Runs only when `ADAM_TEST_MOCK_OPENAI_URL` is set (the mock's root, for
//! example `http://127.0.0.1:8081`, without `/v1`); otherwise every test
//! passes without doing anything. Start the mock with
//! `docker compose up -d --wait mock-openai`; the scenario switches these
//! tests use are documented in `docs/reference/dev-stack.md`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::BTreeMap;
use std::time::Duration;

use adam_model::{
    FinishReason, Message, ModelClient, ModelDelta, ModelError, ModelRequest, ToolCall, ToolChoice,
    ToolSpec, Usage,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use futures::TryStreamExt;
use secrecy::SecretString;
use serde_json::json;

fn mock_url() -> Option<String> {
    std::env::var("ADAM_TEST_MOCK_OPENAI_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
}

/// A client for the mock, with an optional `X-Mock-Scenario` header.
fn client(base_url: &str, scenario: Option<&str>) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(base_url, SecretString::from("mock-api-key"));
    config.timeout = Duration::from_secs(20);
    config.extra_headers = BTreeMap::new();
    if let Some(scenario) = scenario {
        config
            .extra_headers
            .insert("X-Mock-Scenario".into(), scenario.into());
    }
    OpenAiCompatible::new(config).expect("client")
}

fn request(prompt: &str) -> ModelRequest {
    let mut req = ModelRequest::new("mock-model");
    req.system = Some("be brief".into());
    req.messages.push(Message::user_text(prompt));
    req.max_output_tokens = Some(64);
    req
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get the current weather for a city".into(),
        parameters: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }
}

fn tool_request(prompt: &str) -> ModelRequest {
    let mut req = request(prompt);
    req.tools = vec![weather_tool()];
    req.tool_choice = ToolChoice::Auto;
    req
}

async fn stream_of(client: &OpenAiCompatible, req: ModelRequest) -> Vec<ModelDelta> {
    client
        .stream(req)
        .await
        .expect("stream")
        .try_collect()
        .await
        .expect("deltas")
}

#[tokio::test]
async fn text_answer_complete_and_stream_on_both_paths() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    // The mock serves `/v1/chat/completions` and `/chat/completions`.
    for base in [format!("{root}/v1"), root.clone()] {
        let client = client(&base, None);

        let resp = client.complete(request("hi")).await.expect("complete");
        assert_eq!(resp.finish, FinishReason::Stop, "{base}");
        assert!(resp.message.tool_calls().is_empty());
        assert!(!resp.message.text().is_empty());
        assert_eq!(resp.usage, Usage::new(12, 14));

        let deltas = stream_of(&client, request("hi")).await;
        let text: String = deltas
            .iter()
            .filter_map(|d| match d {
                ModelDelta::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("Hello from the mock model."), "{text}");
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::Stop);
        assert_eq!(done.message.text(), text);
        // The usage chunk (empty `choices`) that follows the finish chunk.
        assert_eq!(done.usage.output_tokens, 14);
    }
}

#[tokio::test]
async fn tool_call_by_keyword_and_by_header_then_final_answer() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let base = format!("{root}/v1");
    let by_keyword = client(&base, None);
    let by_header = client(&base, Some("tool-call"));

    for (client, prompt) in [
        (&by_keyword, "[mock:tool-call] weather in Paris?"),
        (&by_header, "weather in Paris?"),
    ] {
        // Non-streaming: the first declared tool is called with `{}`.
        let resp = client
            .complete(tool_request(prompt))
            .await
            .expect("complete");
        assert_eq!(resp.finish, FinishReason::ToolCalls);
        let calls = resp.message.tool_calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, json!({}));
        assert!(!calls[0].id.is_empty());

        // Streaming: the same call, assembled from argument fragments.
        let deltas = stream_of(client, tool_request(prompt)).await;
        assert!(
            deltas.iter().any(|d| matches!(
                d,
                ModelDelta::ToolCallStarted { name, .. } if name == "get_weather"
            )),
            "{deltas:?}"
        );
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::ToolCalls);
        assert_eq!(done.message.tool_calls()[0].name, "get_weather");
        assert_eq!(done.message.tool_calls()[0].arguments, json!({}));
        assert_eq!(done.usage.input_tokens, 20);

        // Once the history holds a tool result, the mock answers in text even
        // though the keyword / header is still there: the agent loop ends.
        let mut follow_up = tool_request(prompt);
        follow_up.messages.push(Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: calls[0].id.clone(),
                name: "get_weather".into(),
                arguments: json!({"city": "Paris"}),
            }],
            reasoning: None,
        });
        follow_up
            .messages
            .push(Message::tool_result(calls[0].id.clone(), "sunny, 21C"));
        let resp = client.complete(follow_up.clone()).await.expect("final");
        assert_eq!(resp.finish, FinishReason::Stop);
        assert!(resp.message.tool_calls().is_empty());
        let deltas = stream_of(client, follow_up).await;
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::Stop);
    }
}

#[tokio::test]
async fn error_scenarios_map_onto_model_errors() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let base = format!("{root}/v1");
    type Check = fn(&ModelError) -> bool;
    let scenarios: [(&str, Check); 4] = [
        ("rate-limit", |e| {
            matches!(
                e,
                ModelError::RateLimited {
                    retry_after: Some(d)
                } if *d == Duration::from_secs(2)
            )
        }),
        ("server-error", |e| {
            matches!(e, ModelError::Transient { .. })
        }),
        ("unauthorized", |e| matches!(e, ModelError::Auth(_))),
        ("context-length", |e| {
            matches!(e, ModelError::ContextLength(_))
        }),
    ];

    for (scenario, is_expected) in scenarios {
        // Selected by the header ...
        let by_header = client(&base, Some(scenario));
        let err = by_header.complete(request("hi")).await.expect_err(scenario);
        assert!(is_expected(&err), "{scenario} (header): {err:?}");
        let Err(err) = by_header.stream(request("hi")).await else {
            panic!("{scenario} (header): the stream must fail before it starts")
        };
        assert!(is_expected(&err), "{scenario} (header, stream): {err:?}");

        // ... and by the keyword in the prompt.
        let plain = client(&base, None);
        let prompt = format!("please fail with [mock:{scenario}]");
        let err = plain.complete(request(&prompt)).await.expect_err(scenario);
        assert!(is_expected(&err), "{scenario} (keyword): {err:?}");
        let Err(err) = plain.stream(request(&prompt)).await else {
            panic!("{scenario} (keyword): the stream must fail before it starts")
        };
        assert!(is_expected(&err), "{scenario} (keyword, stream): {err:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted models answer a stream as they answer a completion
// ---------------------------------------------------------------------------------------------

/// The persona lines of an agent's system prompt, which the scripts read their name and summary from.
const PERSONA: &str =
    "Your name is Adam.\nIn one sentence: I answer questions, research and write documents.\n";

/// What a person says when asked the three questions of `[mock:choices]`.
const ANSWERS: &str =
    "The person answered through the interface:\n- db: pg\n- auth: keycloak\n- deploy: compose";

/// A request of `model` over `history`, as the agents send it (every tool they have is not needed:
/// the scripts read the history).
fn scripted(model: &str, system: &str, history: &[Message]) -> ModelRequest {
    let mut req = ModelRequest::new(model);
    req.system = Some(system.to_owned());
    req.messages = history.to_vec();
    req.max_output_tokens = Some(4096);
    req
}

/// Plays a scripted model from `history` to its answer in words: each turn is asked as a completion and
/// as a stream, which must say the same (the text, the tool calls, why it stopped, the usage), and
/// the calls are answered (`ask_user` with [`ANSWERS`], the others with "ok") and asked again.
/// Returns what the stream's text deltas were at each turn.
async fn play(
    client: &OpenAiCompatible,
    model: &str,
    system: &str,
    mut history: Vec<Message>,
) -> Vec<Vec<String>> {
    play_on(client, model, system, &mut history).await
}

/// [`play`] over `history`, which keeps what was said (the model's messages and the answers to its
/// calls), so that a script that stops to ask can be played on once the person has answered.
async fn play_on(
    client: &OpenAiCompatible,
    model: &str,
    system: &str,
    history: &mut Vec<Message>,
) -> Vec<Vec<String>> {
    play_answering(client, model, system, history, &|call| {
        if call.name == "ask_user" {
            ANSWERS.to_owned()
        } else {
            "ok".to_owned()
        }
    })
    .await
}

/// [`play_on`] with the result of each call chosen by `answer`: for a script that goes one way or
/// the other by what a tool said.
async fn play_answering(
    client: &OpenAiCompatible,
    model: &str,
    system: &str,
    history: &mut Vec<Message>,
    answer: &dyn Fn(&ToolCall) -> String,
) -> Vec<Vec<String>> {
    let mut turns = Vec::new();
    for turn in 0..12 {
        let req = scripted(model, system, history);
        let complete = client
            .complete(req.clone())
            .await
            .unwrap_or_else(|e| panic!("{model} turn {turn} (complete): {e}"));
        let deltas = client
            .stream(req)
            .await
            .unwrap_or_else(|e| panic!("{model} turn {turn} (stream): {e}"))
            .try_collect::<Vec<ModelDelta>>()
            .await
            .unwrap_or_else(|e| panic!("{model} turn {turn} (stream): {e}"));
        let Some(ModelDelta::Finished(streamed)) = deltas.last() else {
            panic!("{model} turn {turn}: a stream ends with Finished: {deltas:?}")
        };
        assert_eq!(
            streamed, &complete,
            "{model} turn {turn}: the stream says what the completion does"
        );
        let texts: Vec<String> = deltas
            .iter()
            .filter_map(|d| match d {
                ModelDelta::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts.concat(),
            complete.message.text(),
            "{model} turn {turn}"
        );
        turns.push(texts);
        let calls = complete.message.tool_calls().to_vec();
        if calls.is_empty() {
            return turns;
        }
        history.push(complete.message.clone());
        for call in calls {
            let result = answer(&call);
            history.push(Message::tool_result(call.id, result));
        }
    }
    panic!("{model}: the script did not end in twelve turns")
}

/// The words of a model that writes slowly are several deltas (what a screen shows growing), for the
/// answers that end a script.
fn grows(turns: &[Vec<String>], at_least: usize, what: &str) {
    let last = turns.last().expect("a turn");
    assert!(
        last.len() >= at_least,
        "{what}: the answer is {} deltas, not {at_least}: {last:?}",
        last.len()
    );
}

#[tokio::test]
async fn the_scripted_models_stream_what_they_complete() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let user = |text: &str| vec![Message::user_text(text)];

    // The coder: a greeting, then a task from the first call to the pull request (with OpenCode, and
    // without), and a task that asks three questions at once.
    let greeting = play(&client, "mock-coder", PERSONA, user("Hi")).await;
    assert_eq!(greeting.len(), 1);
    grows(&greeting, 6, "the coder's greeting");

    let task = "In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello.";
    let mut history = user(task);
    let with_opencode = play_on(&client, "mock-coder", "x", &mut history).await;
    // The default script reads the repository's branches through the GitHub MCP server right after
    // preparing the workspace (the mock of dev/coder-agent/mcp.json answers it).
    let calls: Vec<(String, serde_json::Value)> = history
        .iter()
        .flat_map(|m| m.tool_calls().to_vec())
        .map(|c| (c.name, c.arguments))
        .collect();
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "prepare_workspace",
            "github__list_branches",
            "delegate_to_opencode",
            "run_checks",
            "commit_and_push",
            "open_pull_request"
        ]
    );
    assert_eq!(
        calls[1].1,
        json!({"owner": "local", "repo": "sandbox"}),
        "the branches of the repository the task names"
    );
    assert_eq!(
        with_opencode.len(),
        7,
        "six calls (the second reads the repository's branches over MCP, `github__list_branches`), the last the pull request, then the answer"
    );
    grows(&with_opencode, 6, "the coder's last answer");
    let without = play(
        &client,
        "mock-coder",
        "x",
        user(&format!("{task} [mock:no-opencode]")),
    )
    .await;
    assert_eq!(without.len(), 5, "four calls, then the answer");
    grows(&without, 6, "the coder's last answer without OpenCode");

    // The same task with the file tools: it reads and writes the files itself, and OpenCode is
    // never called (the `[mock:files]` script: `read_file`, `write_file`).
    let files = play(
        &client,
        "mock-coder",
        "x",
        user(&format!("{task} [mock:files]")),
    )
    .await;
    assert_eq!(
        files.len(),
        7,
        "six calls, the last the pull request, then the answer"
    );
    grows(&files, 6, "the coder's last answer with the file tools");

    let choices = play(
        &client,
        "mock-coder",
        "x",
        user("[mock:choices] set up the project"),
    )
    .await;
    assert_eq!(choices.len(), 2, "the form, then the answer");
    grows(&choices, 2, "the coder's answer to the form");

    // The general agent: answered in role, and once a tool result is in.
    let chat = play(&client, "mock-assistant", PERSONA, user("Hi")).await;
    grows(&chat, 2, "the assistant's greeting");
    let looked = vec![
        Message::user_text("Hi"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "search__web_search".into(),
                arguments: json!({}),
            }],
            reasoning: None,
        },
        Message::tool_result("c1", "1. A"),
    ];
    let after_tool = play(&client, "mock-assistant", "x", looked).await;
    assert_eq!(after_tool.len(), 1);
    grows(&after_tool, 2, "the assistant's answer after a tool");

    // The researcher: the sources as cards, then the answer in words.
    let cards = play(
        &client,
        "mock-researcher",
        "x",
        user("[mock:cards] what is async rust?"),
    )
    .await;
    assert_eq!(cards.len(), 2);
    grows(&cards, 2, "the researcher's answer");
}

/// The scratch script: a project is built and checked, the model asks where to put it (the run
/// parks on that text), and once the person names a repository it publishes there, commits, pushes
/// and opens the pull request, in that repository's slot. The repository is named in the task
/// text (`fib-<hex>`), so a second run on the same stack never meets the first one's.
#[tokio::test]
async fn the_scratch_script_asks_where_to_publish_and_goes_on_when_told() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let id = "fib-1a2b3c4d5e";
    let task = format!(
        "Write a fib.sh that prints the first 7 Fibonacci numbers. I'll give you the repo later. [mock:scratch] {id}"
    );
    let mut history = vec![Message::user_text(task)];
    let first = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(first.len(), 5, "four calls, then the question");
    let question = first.last().expect("a turn").concat();
    assert!(
        question.contains("which repository should I publish it to"),
        "{question}"
    );
    grows(&first, 2, "the question");

    // What the coder makes of a text stop: the text is the question of an `ask_user` call (its id is
    // `stop` and the turn), and the person's answer is that call's result.
    let Message::Assistant { content, .. } = Message::assistant_text(&question) else {
        unreachable!("an assistant message")
    };
    history.push(Message::Assistant {
        content,
        tool_calls: vec![ToolCall {
            id: "stop00004".into(),
            name: "ask_user".into(),
            arguments: json!({"question": question}),
        }],
        reasoning: None,
    });
    history.push(Message::tool_result(
        "stop00004",
        format!("Publish it to http://git-server:8080/scratch/{id}.git"),
    ));
    let second = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(second.len(), 4, "three calls, then the answer");
    grows(&second, 6, "the coder's last answer after publishing");

    let calls: Vec<(String, serde_json::Value)> = history
        .iter()
        .flat_map(|m| m.tool_calls().to_vec())
        .filter(|c| c.name != "ask_user")
        .map(|c| (c.name, c.arguments))
        .collect();
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "start_scratch",
            "write_file",
            "write_file",
            "run_checks",
            "publish_scratch",
            "commit_and_push",
            "open_pull_request"
        ]
    );
    assert_eq!(calls[3].1["repo"], "fib", "the check runs in the project");
    assert_eq!(
        calls[4].1["repo_url"],
        format!("http://git-server:8080/scratch/{id}.git").as_str()
    );
    for (name, args) in &calls[5..] {
        assert_eq!(args["repo"], id, "{name} works in the repository's slot");
    }
}

/// The devcontainer scripts (slice 7b): each plays from its task to its answer, as a completion and as a
/// stream, and says the same. `devcontainer` works in the repository's own environment (it looks for a
/// tool only that has, looks at the environment, has OpenCode record the tool's output, checks, pushes
/// and opens the pull request); `default-env` asks where it is, then does the default task;
/// `broken-env` meets an environment that is broken and asks the person; `no-runtime` meets a tool that
/// the repository's environment would have and the deployment lacks, and asks the person.
#[tokio::test]
async fn the_devcontainer_scripts_play_to_their_answers_both_ways() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let named = |repo: &str, switch: &str| {
        vec![Message::user_text(format!(
            "In http://git-server:8080/{repo}.git (base branch main), do the task. {switch}"
        ))]
    };
    let calls_of = |history: &[Message]| -> Vec<(String, serde_json::Value)> {
        history
            .iter()
            .flat_map(|m| m.tool_calls().to_vec())
            .map(|c| (c.name, c.arguments))
            .collect()
    };

    let mut history = named("local/devbox", "[mock:devcontainer]");
    let turns = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(turns.len(), 8, "seven calls, then the answer");
    grows(&turns, 6, "the coder's last answer in the devcontainer");
    let calls = calls_of(&history);
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "prepare_workspace",
            "run_command",
            "run_command",
            "delegate_to_opencode",
            "run_checks",
            "commit_and_push",
            "open_pull_request"
        ]
    );
    assert_eq!(
        calls[0].1["repo_url"],
        "http://git-server:8080/local/devbox.git"
    );
    assert_eq!(calls[1].1["command"], "devbox-tool --version");
    assert_eq!(calls[2].1["command"], "env");
    assert!(
        calls[3].1["instructions"]
            .as_str()
            .is_some_and(|i| i.contains("[mock:oc-devbox]")),
        "OpenCode's script is selected by its instructions: {}",
        calls[3].1
    );
    assert_eq!(calls[4].1["command"], "sh ./check.sh");

    let mut history = named("local/sandbox", "[mock:default-env]");
    let turns = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(turns.len(), 6, "five calls, then the answer");
    grows(
        &turns,
        6,
        "the coder's last answer in the default environment",
    );
    let calls = calls_of(&history);
    assert_eq!(
        calls[0].1["repo_url"],
        "http://git-server:8080/local/sandbox.git"
    );
    assert_eq!(
        calls[1].1["command"], "test -d /opt/flutter && echo coder-env || echo devcontainer-env",
        "only the coder's own image has /opt/flutter"
    );

    let mut history = named("local/devbox-broken", "[mock:broken-env]");
    let turns = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(turns.len(), 3, "two calls, then the question");
    let question = turns.last().expect("a turn").concat();
    assert!(
        question.contains("privileged") && question.contains("go on in the default environment"),
        "{question}"
    );
    assert_eq!(calls_of(&history)[1].1["command"], "true");

    let mut history = named("local/devbox", "[mock:no-runtime]");
    let turns = play_on(&client, "mock-coder", "x", &mut history).await;
    assert_eq!(turns.len(), 3, "two calls, then the question");
    let question = turns.last().expect("a turn").concat();
    assert!(
        question.contains("devbox-tool") && question.contains("without a container runtime"),
        "{question}"
    );
}

/// The second-repository script: the coder prepares the sandbox and asks the person whether it may
/// add the library, then goes by what `prepare_workspace` says of it. Added (its slot is in the
/// result): it reads the greeting, writes it in the sandbox, checks, pushes and opens the pull
/// request. Refused: it says so, and the run waits for the person.
#[tokio::test]
async fn the_second_repo_script_goes_on_when_the_library_is_added_and_stops_when_it_is_not() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let task = "In http://git-server:8080/local/sandbox.git (base branch main), put our shared greeting into hello.txt. [mock:second-repo]";
    for added in [true, false] {
        let mut history = vec![Message::user_text(task)];
        let turns = play_answering(&client, "mock-coder", "x", &mut history, &|call| match (
            call.name.as_str(),
            call.arguments["repo_url"].as_str(),
        ) {
            ("request_repository", _) => "yes".to_owned(),
            ("prepare_workspace", Some(url)) if url.contains("library") && added => {
                "Worktree ready.\nrepository: library\nslot: library\nbase branch: main".to_owned()
            }
            ("prepare_workspace", Some(url)) if url.contains("library") => format!(
                "Refused: {url} is not a repository the person named in their messages. \
                     Call request_repository."
            ),
            _ => "ok".to_owned(),
        })
        .await;
        let calls: Vec<(String, serde_json::Value)> = history
            .iter()
            .flat_map(|m| m.tool_calls().to_vec())
            .map(|c| (c.name, c.arguments))
            .collect();
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
        let library = "http://git-server:8080/local/library.git";
        assert_eq!(
            calls[1].1["repo_url"], library,
            "the model asks about the library"
        );
        assert!(
            calls[1].1["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "with a reason: {:?}",
            calls[1].1
        );
        assert_eq!(calls[2].1["repo_url"], library);
        if added {
            assert_eq!(
                names,
                [
                    "prepare_workspace",
                    "request_repository",
                    "prepare_workspace",
                    "read_file",
                    "write_file",
                    "run_checks",
                    "commit_and_push",
                    "open_pull_request"
                ]
            );
            assert_eq!(calls[3].1["repo"], "library");
            for (name, args) in &calls[4..] {
                assert_eq!(
                    args["repo"], "sandbox",
                    "{name} works in the sandbox's slot"
                );
            }
            grows(&turns, 6, "the answer after the pull request");
        } else {
            assert_eq!(
                names,
                [
                    "prepare_workspace",
                    "request_repository",
                    "prepare_workspace"
                ]
            );
            let said = turns.last().expect("a turn").concat();
            assert!(
                said.contains("I could not add the library repository"),
                "{said}"
            );
        }
    }
}

/// The create-repository script: the project is built, the coder says it can create a repository
/// (the run waits), the person asks for one, the coder calls `create_repository` and the run waits
/// again for the consent, and when it asks again it goes by what the tool said: created, so the
/// project is published there and the pull request opened; declined, so it says so and waits.
#[tokio::test]
async fn the_create_repo_script_publishes_to_the_new_repository_and_stops_when_the_person_declines()
{
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let id = "fib-1a2b3c4d5e";
    let task = format!(
        "Write a fib.sh that prints the first 7 Fibonacci numbers. I'll give you a repo later. [mock:create-repo] {id}"
    );
    for created in [true, false] {
        let mut history = vec![Message::user_text(&task)];
        let first = play_on(&client, "mock-coder", "x", &mut history).await;
        assert_eq!(first.len(), 5, "four calls, then the question");
        let question = first.last().expect("a turn").concat();
        assert!(
            question.contains("I can create a repository for it"),
            "{question}"
        );
        // What the coder makes of a text stop, and the person's answer.
        let Message::Assistant { content, .. } = Message::assistant_text(&question) else {
            unreachable!("an assistant message")
        };
        history.push(Message::Assistant {
            content,
            tool_calls: vec![ToolCall {
                id: "stop00004".into(),
                name: "ask_user".into(),
                arguments: json!({"question": question}),
            }],
            reasoning: None,
        });
        history.push(Message::tool_result(
            "stop00004",
            format!("Create scratch/{id} and put it there"),
        ));
        // The first call to the tool asks the person: their answer is its result. The second
        // creates, or finds the person said no.
        let calls = std::cell::Cell::new(0);
        let turns = play_answering(&client, "mock-coder", "x", &mut history, &|call| {
            if call.name != "create_repository" {
                return "ok".to_owned();
            }
            calls.set(calls.get() + 1);
            match (calls.get(), created) {
                (1, true) => "yes".to_owned(),
                (1, false) => "no".to_owned(),
                (_, true) => format!(
                    "Created scratch/{id} (private, empty: it has no commit yet).\nrepository: http://git-server:8080/scratch/{id}.git"
                ),
                (_, false) => "The person declined to have scratch/x created (private).".to_owned(),
            }
        })
        .await;
        let names: Vec<String> = history
            .iter()
            .flat_map(|m| m.tool_calls().to_vec())
            .filter(|c| c.name != "ask_user")
            .map(|c| c.name)
            .collect();
        let calls: Vec<serde_json::Value> = history
            .iter()
            .flat_map(|m| m.tool_calls().to_vec())
            .filter(|c| c.name == "create_repository")
            .map(|c| c.arguments)
            .collect();
        assert_eq!(calls.len(), 2, "asked, then created");
        assert_eq!(calls[0]["owner"], "scratch");
        assert_eq!(calls[0]["name"], id);
        assert_eq!(calls[0], calls[1], "the same arguments both times");
        if created {
            assert_eq!(
                names,
                [
                    "start_scratch",
                    "write_file",
                    "write_file",
                    "run_checks",
                    "create_repository",
                    "create_repository",
                    "publish_scratch",
                    "commit_and_push",
                    "open_pull_request"
                ]
            );
            grows(&turns, 6, "the answer after the pull request");
        } else {
            assert_eq!(names.len(), 6, "{names:?}");
            let said = turns.last().expect("a turn").concat();
            assert!(said.contains("I did not create the repository"), "{said}");
        }
    }
}

/// A script's answer is written over about two seconds, not all at once: a screen that shows words as
/// they come has something to show (and a test that reads the pieces has more than one to read).
#[tokio::test]
async fn the_coders_last_answer_arrives_over_time() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let client = client(&format!("{root}/v1"), None);
    let mut history = vec![Message::user_text("add hello.txt")];
    history.push(Message::Assistant {
        content: vec![],
        tool_calls: vec![ToolCall {
            id: "coder-call-5".into(),
            name: "open_pull_request".into(),
            arguments: json!({}),
        }],
        reasoning: None,
    });
    history.push(Message::tool_result("coder-call-5", "ok"));
    let started = std::time::Instant::now();
    let mut stream = client
        .stream(scripted("mock-coder", "x", &history))
        .await
        .expect("stream");
    let mut arrivals = Vec::new();
    while let Some(delta) = futures::StreamExt::next(&mut stream).await {
        if let ModelDelta::Text(_) = delta.expect("delta") {
            arrivals.push(started.elapsed());
        }
    }
    assert!(arrivals.len() >= 6, "{} text deltas", arrivals.len());
    let (first, last) = (arrivals[0], *arrivals.last().expect("an arrival"));
    assert!(
        last.saturating_sub(first) >= Duration::from_millis(1000),
        "the words come over time, not at once: {first:?} to {last:?}"
    );
}
