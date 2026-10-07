//! Push notifications, end to end: the official client registers webhooks, the deliverer sends to
//! a local webhook that records what it gets.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::sync::Arc;
use std::time::Duration;

use a2a::{
    AuthenticationInfo, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, ListTaskPushNotificationConfigsRequest,
    SendMessageConfiguration, TaskPushNotificationConfig, TaskState, error_code,
};
use adam_a2a::push::{NewPushConfig, PushCursor, PushPolicy, PushState, PushStore, PushSupport};
use support::*;

fn config(task_id: &str, url: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        url: url.to_owned(),
        id: None,
        task_id: task_id.to_owned(),
        token: Some("tok-123".to_owned()),
        authentication: Some(AuthenticationInfo {
            scheme: "Bearer".to_owned(),
            credentials: Some("cred-secret".to_owned()),
        }),
        tenant: None,
    }
}

/// A held task, with the webhook registered in the message itself: the baseline is the task as it
/// was created, so what the webhook hears does not depend on how fast the client is.
async fn send_hold_with_hook(client: &Client, url: &str) -> a2a::Task {
    let mut request = send("[hold] go", None);
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", url)),
        history_length: None,
        return_immediately: Some(true),
    });
    task_of(client.send_message(&request).await.unwrap())
}

async fn started(push: Option<PushSupport>) -> TestServer {
    TestServer::start(Setup {
        push,
        ..Setup::default()
    })
    .await
}

/// The label sequence a `[hold]` task produces once released: working, the artifact, completed.
const HOLD_RUN: [&str; 3] = [
    "status:TASK_STATE_WORKING",
    "artifact:echo: [hold] go",
    "status:TASK_STATE_COMPLETED",
];

// ---------------------------------------------------------------------- off

#[tokio::test]
async fn push_is_off_by_default_and_the_card_says_so() {
    let server = started(None).await;
    let client = server.client(Some(TOKEN)).await;
    let card = reqwest_card(&server).await;
    assert_eq!(card["capabilities"]["pushNotifications"], false);
    let task = task_of(client.send_message(&send("hi", None)).await.unwrap());
    let err = client
        .create_push_config(&config(&task.id, "https://hooks.example.com/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    for call in [
        client
            .get_push_config(&GetTaskPushNotificationConfigRequest {
                task_id: task.id.clone(),
                id: "x".into(),
                tenant: None,
            })
            .await
            .map(|_| ()),
        client
            .list_push_configs(&ListTaskPushNotificationConfigsRequest {
                task_id: task.id.clone(),
                page_size: None,
                page_token: None,
                tenant: None,
            })
            .await
            .map(|_| ()),
        client
            .delete_push_config(&DeleteTaskPushNotificationConfigRequest {
                task_id: task.id.clone(),
                id: "x".into(),
                tenant: None,
            })
            .await,
    ] {
        assert_eq!(
            call.unwrap_err().code,
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        );
    }
}

#[tokio::test]
async fn a_policy_that_allows_nothing_is_off_too() {
    let store = adam_a2a::push::InMemoryPushStore::new();
    let none = PushPolicy::new(Vec::<String>::new()).unwrap();
    let server = started(Some(PushSupport::new(Arc::new(store), none))).await;
    let card = reqwest_card(&server).await;
    assert_eq!(card["capabilities"]["pushNotifications"], false);
    let client = server.client(Some(TOKEN)).await;
    let task = task_of(client.send_message(&send("hi", None)).await.unwrap());
    let err = client
        .create_push_config(&config(&task.id, "https://hooks.example.com/x"))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
}

#[tokio::test]
async fn an_inline_config_is_refused_when_push_is_off_and_no_task_is_created() {
    let server = started(None).await;
    let client = server.client(Some(TOKEN)).await;
    let mut request = send("hi", None);
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", "https://hooks.example.com/x")),
        history_length: None,
        return_immediately: Some(true),
    });
    let err = client.send_message(&request).await.unwrap_err();
    assert_eq!(err.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    assert!(server.backend.task_ids().is_empty(), "nothing was started");
}

/// An inline config that breaks the policy fails the call before the message is submitted.
#[tokio::test]
async fn an_inline_config_the_policy_refuses_creates_no_task() {
    let store = adam_a2a::push::InMemoryPushStore::new();
    let policy = PushPolicy::new(["hooks.example.com"]).unwrap();
    let server = started(Some(PushSupport::new(Arc::new(store.clone()), policy))).await;
    let client = server.client(Some(TOKEN)).await;
    let mut request = send("hi", None);
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", "https://evil.example.org/hook")),
        history_length: None,
        return_immediately: Some(true),
    });
    let err = client.send_message(&request).await.unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert!(server.backend.task_ids().is_empty(), "nothing was started");
    assert!(store.records().is_empty());
}

/// The cap of a task that already has its sixteen is known before the message is submitted.
#[tokio::test]
async fn an_inline_config_over_the_cap_of_the_task_it_continues_is_refused_before_the_message() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push)).await;
    let client = server.client(Some(TOKEN)).await;
    let task = task_of(client.send_message(&send("[hold] go", None)).await.unwrap());
    for i in 0..16 {
        let mut c = config(&task.id, &hook.url());
        c.id = Some(format!("c{i}"));
        client.create_push_config(&c).await.unwrap();
    }
    let before = history_len(&client, &task.id).await;
    let mut follow_up = send("more", None);
    follow_up.message.task_id = Some(task.id.clone());
    follow_up.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", &hook.url())),
        history_length: None,
        return_immediately: Some(true),
    });
    let err = client.send_message(&follow_up).await.unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert_eq!(
        history_len(&client, &task.id).await,
        before,
        "the message was not submitted"
    );
    assert_eq!(store.records().len(), 16);
}

async fn history_len(client: &Client, id: &str) -> usize {
    let mut request = get(id);
    request.history_length = Some(100);
    let task = client.get_task(&request).await.unwrap();
    task.history.map_or(0, |h| h.len())
}

/// A push store whose `put` fails, over a real one.
struct PutFails(adam_a2a::push::InMemoryPushStore);

#[async_trait::async_trait]
impl PushStore for PutFails {
    async fn put(
        &self,
        _new: NewPushConfig,
    ) -> Result<adam_a2a::push::PushRecord, adam_a2a::push::PushStoreError> {
        Err(adam_a2a::push::PushStoreError::Unavailable(
            "the database went away".into(),
        ))
    }
    async fn list(
        &self,
        task_id: &str,
    ) -> Result<Vec<adam_a2a::push::PushRecord>, adam_a2a::push::PushStoreError> {
        self.0.list(task_id).await
    }
    async fn delete(
        &self,
        task_id: &str,
        id: &str,
    ) -> Result<bool, adam_a2a::push::PushStoreError> {
        self.0.delete(task_id, id).await
    }
    async fn claim_due(
        &self,
        worker: &str,
        now: chrono::DateTime<chrono::Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Result<Vec<adam_a2a::push::PushRecord>, adam_a2a::push::PushStoreError> {
        self.0.claim_due(worker, now, ttl, limit).await
    }
    async fn commit(
        &self,
        task_id: &str,
        id: &str,
        expected_version: u64,
        progress: adam_a2a::push::PushProgress,
    ) -> Result<adam_a2a::push::PushRecord, adam_a2a::push::PushStoreError> {
        self.0.commit(task_id, id, expected_version, progress).await
    }
}

/// The task exists once the message is submitted: a store that fails after that does not fail the
/// call, or the client would send the message again.
#[tokio::test]
async fn a_store_that_fails_after_the_task_exists_still_returns_the_task() {
    let hook = Webhook::start().await;
    let inner = adam_a2a::push::InMemoryPushStore::new();
    let push = PushSupport::new(Arc::new(PutFails(inner.clone())), local_policy(&[&hook]));
    let server = started(Some(push)).await;
    let client = server.client(Some(TOKEN)).await;
    let mut request = send("[hold] go", None);
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", &hook.url())),
        history_length: None,
        return_immediately: Some(true),
    });
    let task = task_of(client.send_message(&request).await.unwrap());
    assert_eq!(server.backend.task_ids().len(), 1, "one task");
    assert!(inner.records().is_empty(), "and no config");
    // The client can register the webhook itself.
    let err = client
        .create_push_config(&config(&task.id, &hook.url()))
        .await
        .unwrap_err();
    assert_ne!(err.code, error_code::INVALID_PARAMS, "{err:?}");
}

// ------------------------------------------------------------ configurations

#[tokio::test]
async fn configs_are_created_read_listed_and_deleted_and_secrets_are_write_only() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push)).await;
    assert_eq!(
        reqwest_card(&server).await["capabilities"]["pushNotifications"],
        true
    );
    let client = server.client(Some(TOKEN)).await;
    let mut hold = send("[hold] go", None);
    hold.message.message_id = "m-1".into();
    let task = task_of(client.send_message(&hold).await.unwrap());

    let mut with_id = config(&task.id, &hook.url());
    with_id.id = Some("mine".into());
    let created = client.create_push_config(&with_id).await.unwrap();
    assert_eq!(created.id.as_deref(), Some("mine"));
    assert_eq!(created.task_id, task.id);
    assert_eq!(created.url, hook.url());
    assert_eq!(created.token, None, "the token is write-only");
    let auth = created.authentication.unwrap();
    assert_eq!(auth.scheme, "Bearer");
    assert_eq!(auth.credentials, None, "the credentials are write-only");
    // The store holds what the client gave it.
    let stored = store.records();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].config.token.as_deref(), Some("tok-123"));
    assert_eq!(stored[0].owner, "token-0");

    // An id is assigned when none is given.
    let second = client
        .create_push_config(&config(&task.id, &hook.url()))
        .await
        .unwrap();
    let assigned = second.id.clone().unwrap();
    assert!(!assigned.is_empty() && assigned != "mine");

    let got = client
        .get_push_config(&GetTaskPushNotificationConfigRequest {
            task_id: task.id.clone(),
            id: "mine".into(),
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(got.id.as_deref(), Some("mine"));
    assert_eq!(got.token, None);

    let list = |page_size: Option<i32>, token: Option<String>| {
        let client = &client;
        let task_id = task.id.clone();
        async move {
            client
                .list_push_configs(&ListTaskPushNotificationConfigsRequest {
                    task_id,
                    page_size,
                    page_token: token,
                    tenant: None,
                })
                .await
                .unwrap()
        }
    };
    let all = list(None, None).await;
    let mut ids: Vec<_> = all.configs.iter().map(|c| c.id.clone().unwrap()).collect();
    ids.sort();
    let mut want = vec!["mine".to_owned(), assigned.clone()];
    want.sort();
    assert_eq!(ids, want);
    assert_eq!(all.next_page_token, None);
    // Pagination: one at a time, by cursor, until the end.
    let first = list(Some(1), None).await;
    assert_eq!(first.configs.len(), 1);
    let token = first.next_page_token.clone().expect("more pages");
    let next = list(Some(1), Some(token)).await;
    assert_eq!(next.configs.len(), 1);
    assert_ne!(next.configs[0].id, first.configs[0].id);
    assert_eq!(next.next_page_token, None);
    let bad = client
        .list_push_configs(&ListTaskPushNotificationConfigsRequest {
            task_id: task.id.clone(),
            page_size: None,
            page_token: Some("%%%".into()),
            tenant: None,
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code, error_code::INVALID_PARAMS);

    // Replacing an id replaces the config.
    let mut again = with_id.clone();
    again.url = format!("{}?v=2", hook.url());
    client.create_push_config(&again).await.unwrap();
    assert_eq!(list(None, None).await.configs.len(), 2);
    assert!(
        store
            .records()
            .iter()
            .any(|r| r.id == "mine" && r.config.url.ends_with("?v=2")),
        "the config was replaced"
    );

    // Delete, twice (idempotent), and the config is gone.
    let delete = DeleteTaskPushNotificationConfigRequest {
        task_id: task.id.clone(),
        id: "mine".into(),
        tenant: None,
    };
    client.delete_push_config(&delete).await.unwrap();
    client.delete_push_config(&delete).await.unwrap();
    let err = client
        .get_push_config(&GetTaskPushNotificationConfigRequest {
            task_id: task.id.clone(),
            id: "mine".into(),
            tenant: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
    assert_eq!(list(None, None).await.configs.len(), 1);
}

#[tokio::test]
async fn another_caller_cannot_touch_the_configs_of_a_task() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push)).await;
    let owner = server.client(Some(TOKEN)).await;
    let other = server.client(Some(OTHER_TOKEN)).await;
    let task = task_of(owner.send_message(&send("[hold] go", None)).await.unwrap());
    let mut mine = config(&task.id, &hook.url());
    mine.id = Some("mine".into());
    owner.create_push_config(&mine).await.unwrap();

    let not_found = |code: i32| assert_eq!(code, error_code::TASK_NOT_FOUND);
    not_found(
        other
            .create_push_config(&config(&task.id, &hook.url()))
            .await
            .unwrap_err()
            .code,
    );
    not_found(
        other
            .get_push_config(&GetTaskPushNotificationConfigRequest {
                task_id: task.id.clone(),
                id: "mine".into(),
                tenant: None,
            })
            .await
            .unwrap_err()
            .code,
    );
    not_found(
        other
            .list_push_configs(&ListTaskPushNotificationConfigsRequest {
                task_id: task.id.clone(),
                page_size: None,
                page_token: None,
                tenant: None,
            })
            .await
            .unwrap_err()
            .code,
    );
    not_found(
        other
            .delete_push_config(&DeleteTaskPushNotificationConfigRequest {
                task_id: task.id.clone(),
                id: "mine".into(),
                tenant: None,
            })
            .await
            .unwrap_err()
            .code,
    );
    // Neither a made-up task id: the same answer, so nothing is learnt about other callers' tasks.
    not_found(
        other
            .create_push_config(&config("no-such-task", &hook.url()))
            .await
            .unwrap_err()
            .code,
    );
    // Untouched.
    assert_eq!(store.records().len(), 1);
    assert_eq!(store.records()[0].owner, "token-0");
}

#[tokio::test]
async fn a_webhook_the_deployment_did_not_allow_is_refused_with_invalid_params() {
    let hook = Webhook::start().await;
    let store = adam_a2a::push::InMemoryPushStore::new();
    // Production-like policy: https only, a host allow-list, no private addresses.
    let policy = PushPolicy::new(["hooks.example.com", "https://api.partner.io/a2a/"]).unwrap();
    let server = started(Some(PushSupport::new(Arc::new(store.clone()), policy))).await;
    let client = server.client(Some(TOKEN)).await;
    let task = task_of(client.send_message(&send("[hold] go", None)).await.unwrap());

    for url in [
        "https://evil.example.org/hook",           // not on the list
        "http://hooks.example.com/hook",           // not https
        "https://hooks.example.com.evil.org/hook", // a longer host
        "https://api.partner.io/other/",           // outside the prefix
        "https://user:pw@hooks.example.com/hook",  // credentials in the URL
        "ftp://hooks.example.com/hook",
        "not a url",
        &hook.url(), // loopback
        "https://127.0.0.1/hook",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/hook",
        "https://10.0.0.5/hook",
        "https://localhost/hook",
    ] {
        let err = client
            .create_push_config(&config(&task.id, url))
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS, "{url}: {err:?}");
    }
    assert!(store.records().is_empty(), "nothing was stored");
    // The two allowed shapes are accepted.
    for url in [
        "https://hooks.example.com/any/path?x=1",
        "https://api.partner.io/a2a/tenant-1",
    ] {
        client
            .create_push_config(&config(&task.id, url))
            .await
            .unwrap();
    }
    assert_eq!(store.records().len(), 2);

    // Headers that cannot be sent, and too many configs, are refused too.
    let mut bad = config(&task.id, "https://hooks.example.com/x");
    bad.token = Some("a\r\nX-Evil: 1".into());
    assert_eq!(
        client.create_push_config(&bad).await.unwrap_err().code,
        error_code::INVALID_PARAMS
    );
    let mut bad = config(&task.id, "https://hooks.example.com/x");
    bad.authentication = Some(AuthenticationInfo {
        scheme: "Bearer token".into(),
        credentials: None,
    });
    assert_eq!(
        client.create_push_config(&bad).await.unwrap_err().code,
        error_code::INVALID_PARAMS
    );
    for i in 0..14 {
        let mut c = config(&task.id, "https://hooks.example.com/x");
        c.id = Some(format!("c{i}"));
        client.create_push_config(&c).await.unwrap();
    }
    let err = client
        .create_push_config(&config(&task.id, "https://hooks.example.com/x"))
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        error_code::INVALID_PARAMS,
        "at most 16 configs per task"
    );
}

/// The name earlier drafts of the protocol used for the config of a send
/// (`pushNotificationConfig`) is not read by the SDK's JSON-RPC layer, which would drop it and start
/// a task whose client waits for notifications nobody registered: it is refused, loudly, and starts
/// nothing. The 1.0 name (`taskPushNotificationConfig`) works over the same wire.
#[tokio::test]
async fn the_old_name_of_the_config_of_a_send_is_refused_and_the_new_one_works() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push)).await;
    let post = |name: &'static str, id: &'static str| {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "SendMessage",
            "params": {
                "message": {"messageId": id, "role": "ROLE_USER", "parts": [{"text": "[hold] go"}]},
                "configuration": {
                    "returnImmediately": true,
                    name: {"url": hook.url(), "token": "t-wire"}
                }
            }
        });
        let url = server.base();
        async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(url)
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let old = post("pushNotificationConfig", "old").await;
    assert_eq!(old["error"]["code"], error_code::INVALID_PARAMS, "{old}");
    assert_eq!(old["id"], "old");
    assert!(
        old["error"]["message"]
            .as_str()
            .unwrap()
            .contains("taskPushNotificationConfig"),
        "{old}"
    );
    assert!(server.backend.task_ids().is_empty(), "nothing was started");
    assert!(store.records().is_empty());

    let new = post("taskPushNotificationConfig", "new").await;
    assert!(new["result"]["task"]["id"].is_string(), "{new}");
    let records = store.records();
    assert_eq!(records.len(), 1, "{new}");
    assert_eq!(records[0].config.token.as_deref(), Some("t-wire"));
}

// ------------------------------------------------------------------ delivery

#[tokio::test]
async fn each_state_change_is_delivered_in_order_with_the_token_and_the_credentials() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    // Slow steps: the config below is created while the task is still `submitted`.
    let server = TestServer::start(Setup {
        push: Some(push.clone()),
        backend: adam_a2a::InMemoryBackend::with_config(adam_a2a::InMemoryConfig {
            step_delay: Duration::from_millis(250),
        }),
        ..Setup::default()
    })
    .await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    let client = server.client(Some(TOKEN)).await;

    let task = task_of(client.send_message(&send("[hold] go", None)).await.unwrap());
    client
        .create_push_config(&config(&task.id, &hook.url()))
        .await
        .unwrap();

    // The task is held while working: the webhook hears `working`, and nothing else yet.
    let first = hook.wait_accepted(1).await;
    assert_eq!(first[0].label(), HOLD_RUN[0]);
    assert!(server.backend.release(&task.id));
    let all = hook.wait_accepted(3).await;
    assert_eq!(hook.labels(), HOLD_RUN);
    for received in &all {
        assert_eq!(received.task_id(), Some(task.id.as_str()));
        assert_eq!(received.header("authorization"), Some("Bearer cred-secret"));
        assert_eq!(received.header("a2a-notification-token"), Some("tok-123"));
        assert_eq!(
            received.header("content-type"),
            Some("application/a2a+json")
        );
    }
    // The terminal state ends delivery: the config is done, and nothing more arrives.
    for _ in 0..200 {
        if store.records()[0].state == PushState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.records()[0].state, PushState::Done);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hook.accepted().len(), 3, "nothing after the terminal state");
    // GetTask is the truth, and agrees.
    assert_eq!(
        wait_for_state(&client, &task.id, TaskState::Completed)
            .await
            .id,
        task.id
    );
    delivery.stop().await;
}

#[tokio::test]
async fn a_config_created_in_send_message_hears_the_whole_run() {
    let hook = Webhook::start().await;
    let (push, _store) = local_push(&[&hook]);
    let server = started(Some(push.clone())).await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    let client = server.client(Some(TOKEN)).await;

    let mut request = send("[hold] go", None);
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", &hook.url())),
        history_length: None,
        return_immediately: Some(true),
    });
    let task = task_of(client.send_message(&request).await.unwrap());
    // The client did not name a task: the config belongs to the one that was created.
    let listed = client
        .list_push_configs(&ListTaskPushNotificationConfigsRequest {
            task_id: task.id.clone(),
            page_size: None,
            page_token: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(listed.configs.len(), 1);
    assert_eq!(listed.configs[0].task_id, task.id);
    hook.wait_accepted(1).await;
    server.backend.release(&task.id);
    hook.wait_accepted(3).await;
    assert_eq!(hook.labels()[1..], HOLD_RUN[1..]);
    // A repeated request does not add a second config for the same webhook.
    let again = task_of(client.send_message(&request).await.unwrap());
    assert_ne!(
        again.id, task.id,
        "(the in-memory backend has no idempotent submission)"
    );
    delivery.stop().await;
}

#[tokio::test]
async fn a_failing_webhook_is_retried_and_loses_nothing_and_keeps_the_order() {
    let hook = Webhook::start().await;
    // The first three requests fail with a server error, then the webhook recovers.
    hook.script([500, 500, 503]);
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push.clone())).await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    let client = server.client(Some(TOKEN)).await;

    let task = send_hold_with_hook(&client, &hook.url()).await;
    // While the webhook fails, the task moves on; the event that failed is sent again as it was.
    hook.wait_accepted(1).await;
    server.backend.release(&task.id);
    hook.wait_accepted(3).await;
    assert_eq!(
        hook.labels(),
        HOLD_RUN,
        "each state, once accepted, in order"
    );
    let statuses: Vec<u16> = hook.all().iter().map(|r| r.answered).collect();
    assert_eq!(&statuses[..3], [500, 500, 503]);
    let failed: Vec<_> = hook.all().iter().take(3).map(Received::label).collect();
    assert!(
        failed.iter().all(|l| l == HOLD_RUN[0]),
        "the failed event was the one retried: {failed:?}"
    );
    // The failures were recorded and cleared once the webhook recovered.
    for _ in 0..200 {
        if store.records()[0].attempts == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = &store.records()[0];
    assert_eq!(record.attempts, 0);
    assert_eq!(record.last_error, None);
    delivery.stop().await;
}

#[tokio::test]
async fn delivery_resumes_after_a_restart_on_the_same_store_without_loss() {
    let hook = Webhook::start().await;
    let (push, store) = local_push(&[&hook]);
    let setup = Setup {
        push: Some(push.clone()),
        ..Setup::default()
    };
    let backend = setup.backend.clone();
    let first = TestServer::start(setup).await;
    let dyn_backend: adam_a2a::DynTaskBackend = Arc::new(backend.clone());
    // The webhook is down when the first instance runs.
    hook.answer_with(503);
    let delivery = Delivery::start(push.deliverer(dyn_backend.clone(), fast_options()).unwrap());
    let client = first.client(Some(TOKEN)).await;
    let task = send_hold_with_hook(&client, &hook.url()).await;
    for _ in 0..300 {
        if hook.all().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(hook.all().len() >= 2, "the first instance tried and failed");
    assert!(hook.accepted().is_empty());
    // The first instance goes away while the task finishes behind the failing webhook.
    delivery.stop().await;
    drop(first);
    backend.release(&task.id);
    wait_for_state_in(&backend, &task.id, TaskState::Completed).await;
    assert!(hook.accepted().is_empty());
    assert_eq!(
        store.records()[0].state,
        PushState::Active,
        "nothing was lost, nothing was done"
    );

    // A new server instance and deliverer over the same stores; the webhook is back.
    hook.answer_with(200);
    let second_setup = Setup {
        push: Some(push.clone()),
        backend: backend.clone(),
        ..Setup::default()
    };
    let _second = TestServer::start(second_setup).await;
    let delivery = Delivery::start(push.deliverer(dyn_backend, fast_options()).unwrap());
    hook.wait_accepted(3).await;
    assert_eq!(
        hook.labels(),
        HOLD_RUN,
        "the pending event first, then the rest, each once"
    );
    delivery.stop().await;
}

#[tokio::test]
async fn stopping_the_loop_lets_the_request_in_flight_finish_and_release_its_lease() {
    // A restart (or a deploy) stops the loop while a request is on its way. The lease of that
    // config must not be left to run out: the request finishes and its outcome is committed, so
    // the next instance finds the config due at once rather than a lease-length later.
    let hook = Webhook::start().await;
    hook.answer_after(Duration::from_millis(300));
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push.clone())).await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    let client = server.client(Some(TOKEN)).await;
    let task = send_hold_with_hook(&client, &hook.url()).await;
    server.backend.release(&task.id);
    for _ in 0..300 {
        if !hook.all().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!hook.all().is_empty(), "a request is on its way");
    delivery.stop().await;

    // Another instance, at once: the claim is not blocked by a lease the first one left behind.
    let claimed = store
        .claim_due(
            "next-instance",
            chrono::Utc::now(),
            Duration::from_secs(60),
            8,
        )
        .await
        .unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "the config is due, not leased: {claimed:?}"
    );
}

#[tokio::test]
async fn delivery_gives_up_after_the_bound_and_records_why() {
    let hook = Webhook::start().await;
    hook.answer_with(500);
    let (push, store) = local_push(&[&hook]);
    let server = started(Some(push.clone())).await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let options = fast_options().with_give_up_after(Duration::from_millis(250));
    let delivery = Delivery::start(push.deliverer(backend, options).unwrap());
    let client = server.client(Some(TOKEN)).await;
    let _task = send_hold_with_hook(&client, &hook.url()).await;

    let mut record = store.records()[0].clone();
    for _ in 0..500 {
        record = store.records()[0].clone();
        if record.state == PushState::GaveUp {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(record.state, PushState::GaveUp);
    let why = record.last_error.clone().unwrap();
    assert!(why.contains("gave up") && why.contains("500"), "{why}");
    assert!(
        !why.contains("127.0.0.1") && !why.contains("cred-secret") && !why.contains("tok-123"),
        "{why}"
    );
    assert!(record.attempts >= 2);
    // Nothing more is sent after the bound.
    let sent = hook.all().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hook.all().len(), sent);
    delivery.stop().await;
}

#[tokio::test]
async fn a_redirect_is_never_followed() {
    let target = Webhook::start().await;
    let redirector = Redirector::start(target.url()).await;
    let store = adam_a2a::push::InMemoryPushStore::new();
    // Both are allowed, so only the redirect rule can stop a request to the target.
    let policy = PushPolicy::new([
        format!("127.0.0.1:{}", redirector.port),
        format!("127.0.0.1:{}", target.addr.port()),
    ])
    .unwrap()
    .allow_private_addresses(true);
    let push = PushSupport::new(Arc::new(store.clone()), policy);
    let server = started(Some(push.clone())).await;
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    let client = server.client(Some(TOKEN)).await;
    send_hold_with_hook(
        &client,
        &format!("http://127.0.0.1:{}/hook", redirector.port),
    )
    .await;
    for _ in 0..300 {
        if redirector.hits() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        redirector.hits() >= 2,
        "the redirector was asked, and asked again (a retry)"
    );
    assert!(
        target.all().is_empty(),
        "the redirect target was never contacted"
    );
    let error = store.records()[0].last_error.clone().unwrap();
    assert!(error.contains("redirects are not followed"), "{error}");
    delivery.stop().await;
}

#[tokio::test]
async fn a_private_address_is_refused_at_delivery_even_if_it_was_stored() {
    let hook = Webhook::start().await;
    // A production-like policy: private addresses are not allowed, whatever the allow-list says.
    let store = adam_a2a::push::InMemoryPushStore::new();
    let policy = PushPolicy::new([format!("127.0.0.1:{}", hook.addr.port())]).unwrap();
    let push = PushSupport::new(Arc::new(store.clone()), policy);
    let server = started(Some(push.clone())).await;
    let client = server.client(Some(TOKEN)).await;
    let task = task_of(client.send_message(&send("[hold] go", None)).await.unwrap());
    // Put the config straight into the store, as an older policy (or a bug) could have.
    store
        .put(NewPushConfig {
            task_id: task.id.clone(),
            id: "sneaky".into(),
            owner: "token-0".into(),
            config: config(&task.id, &hook.url()),
            cursor: PushCursor::default(),
        })
        .await
        .unwrap();
    let backend: adam_a2a::DynTaskBackend = Arc::new(server.backend.clone());
    let delivery = Delivery::start(push.deliverer(backend, fast_options()).unwrap());
    for _ in 0..300 {
        if store.records()[0].state == PushState::GaveUp {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = store.records()[0].clone();
    assert_eq!(record.state, PushState::GaveUp);
    assert!(record.last_error.unwrap().contains("webhook refused"));
    assert!(
        hook.all().is_empty(),
        "nothing was sent to a private address"
    );
    delivery.stop().await;
}

// -------------------------------------------------------------------- helpers

async fn reqwest_card(server: &TestServer) -> serde_json::Value {
    let url = format!("{}/.well-known/agent-card.json", server.base());
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_for_state_in(backend: &adam_a2a::InMemoryBackend, id: &str, state: TaskState) {
    use adam_a2a::{Caller, TaskBackend as _};
    let caller = Caller::new("token-0");
    for _ in 0..400 {
        if let Some(task) = backend.get(&caller, id).await.unwrap()
            && task.status.state == state
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

/// A server that answers every POST with a 302 to `location` and counts the requests.
struct Redirector {
    port: u16,
    hits: Arc<std::sync::atomic::AtomicUsize>,
}

impl Redirector {
    async fn start(location: String) -> Self {
        use axum::http::{HeaderValue, StatusCode, header::LOCATION};
        use axum::response::IntoResponse as _;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move || {
                let counter = counter.clone();
                let location = location.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut response = StatusCode::FOUND.into_response();
                    response
                        .headers_mut()
                        .insert(LOCATION, HeaderValue::from_str(&location).unwrap());
                    response
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { port, hits }
    }

    fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}
