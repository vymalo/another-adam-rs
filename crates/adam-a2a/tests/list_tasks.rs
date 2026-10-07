//! `ListTasks` through the official client: filters, pagination, isolation, bad tokens.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::time::Duration;

use a2a::{ListTasksRequest, TaskState, error_code};
use support::*;

fn list(page_size: Option<i32>, token: Option<String>) -> ListTasksRequest {
    ListTasksRequest {
        context_id: None,
        status: None,
        page_size,
        page_token: token,
        history_length: None,
        status_timestamp_after: None,
        include_artifacts: None,
        tenant: None,
    }
}

async fn server() -> TestServer {
    TestServer::start(Setup::default()).await
}

/// Five finished tasks of the caller, two contexts, spaced so their updates are ordered.
async fn five_tasks(client: &Client) -> Vec<String> {
    let mut ids = Vec::new();
    for (i, context) in ["ctx-a", "ctx-b", "ctx-a", "ctx-b", "ctx-a"]
        .into_iter()
        .enumerate()
    {
        let mut request = send(&format!("job {i}"), Some(context));
        request.message.message_id = format!("m-{i}");
        let task = task_of(client.send_message(&request).await.unwrap());
        wait_for_state(client, &task.id, TaskState::Completed).await;
        // Distinct update times, in creation order.
        tokio::time::sleep(Duration::from_millis(5)).await;
        ids.push(task.id);
    }
    ids
}

#[tokio::test]
async fn it_lists_the_callers_tasks_newest_first_with_the_response_the_spec_asks_for() {
    let server = server().await;
    let client = server.client(Some(TOKEN)).await;
    let ids = five_tasks(&client).await;
    let response = client.list_tasks(&list(None, None)).await.unwrap();
    let got: Vec<_> = response.tasks.iter().map(|t| t.id.clone()).collect();
    let mut want = ids.clone();
    want.reverse();
    assert_eq!(got, want, "most recently updated first");
    assert_eq!(response.total_size, 5);
    assert_eq!(response.page_size, 50, "the default page size");
    assert_eq!(
        response.next_page_token, "",
        "the last page has an empty token"
    );
    assert!(
        response.tasks.iter().all(|t| t.artifacts.is_none()),
        "artifacts are left out by default"
    );
    assert!(response.tasks.iter().all(|t| t.history.is_none()));

    let with = ListTasksRequest {
        include_artifacts: Some(true),
        ..list(None, None)
    };
    let response = client.list_tasks(&with).await.unwrap();
    assert!(
        response
            .tasks
            .iter()
            .all(|t| t.artifacts.as_ref().is_some_and(|a| a.len() == 1))
    );
}

#[tokio::test]
async fn pages_follow_each_other_by_cursor_without_overlap_or_gaps() {
    let server = server().await;
    let client = server.client(Some(TOKEN)).await;
    let ids = five_tasks(&client).await;
    let mut seen = Vec::new();
    let mut token = None;
    let mut pages = 0;
    loop {
        let response = client
            .list_tasks(&list(Some(2), token.clone()))
            .await
            .unwrap();
        pages += 1;
        assert!(response.tasks.len() <= 2);
        assert_eq!(response.page_size, 2);
        assert_eq!(
            response.total_size, 5,
            "the total does not shrink with the page"
        );
        seen.extend(response.tasks.iter().map(|t| t.id.clone()));
        if response.next_page_token.is_empty() {
            break;
        }
        token = Some(response.next_page_token);
        assert!(pages < 10, "the cursor must end");
    }
    assert_eq!(pages, 3);
    let mut want = ids;
    want.reverse();
    assert_eq!(seen, want);
    // A page size above the maximum is clamped, not refused; zero and negative mean the default.
    assert_eq!(
        client
            .list_tasks(&list(Some(1000), None))
            .await
            .unwrap()
            .page_size,
        100
    );
    assert_eq!(
        client
            .list_tasks(&list(Some(0), None))
            .await
            .unwrap()
            .page_size,
        50
    );
    assert_eq!(
        client
            .list_tasks(&list(Some(-4), None))
            .await
            .unwrap()
            .page_size,
        50
    );
}

#[tokio::test]
async fn the_filters_narrow_the_list_and_the_cursor_is_bound_to_them() {
    let server = server().await;
    let client = server.client(Some(TOKEN)).await;
    let ids = five_tasks(&client).await;
    // By context.
    let a = ListTasksRequest {
        context_id: Some("ctx-a".into()),
        ..list(None, None)
    };
    let response = client.list_tasks(&a).await.unwrap();
    let got: Vec<_> = response.tasks.iter().map(|t| t.id.clone()).collect();
    assert_eq!(got, [ids[4].clone(), ids[2].clone(), ids[0].clone()]);
    assert_eq!(response.total_size, 3);
    // By status: all completed; none working.
    let completed = ListTasksRequest {
        status: Some(TaskState::Completed),
        ..list(None, None)
    };
    assert_eq!(client.list_tasks(&completed).await.unwrap().tasks.len(), 5);
    let working = ListTasksRequest {
        status: Some(TaskState::Working),
        ..list(None, None)
    };
    let none = client.list_tasks(&working).await.unwrap();
    assert!(none.tasks.is_empty() && none.total_size == 0 && none.next_page_token.is_empty());
    // By the time of the last status change: only the tasks updated at or after the 4th.
    let fourth = client
        .get_task(&get(&ids[3]))
        .await
        .unwrap()
        .status
        .timestamp
        .unwrap();
    let after = ListTasksRequest {
        status_timestamp_after: Some(fourth),
        ..list(None, None)
    };
    let response = client.list_tasks(&after).await.unwrap();
    let got: Vec<_> = response.tasks.iter().map(|t| t.id.clone()).collect();
    assert_eq!(got, [ids[4].clone(), ids[3].clone()]);
    // A held task shows up under `working`.
    let held = task_of(
        client
            .send_message(&send("[hold] wait", Some("ctx-a")))
            .await
            .unwrap(),
    );
    wait_for_state(&client, &held.id, TaskState::Working).await;
    let response = client.list_tasks(&working).await.unwrap();
    assert_eq!(response.tasks.len(), 1);
    assert_eq!(response.tasks[0].id, held.id);
    server.backend.release(&held.id);

    // A token from one query is refused with other filters.
    let page = client
        .list_tasks(&ListTasksRequest {
            context_id: Some("ctx-a".into()),
            ..list(Some(1), None)
        })
        .await
        .unwrap();
    assert!(!page.next_page_token.is_empty());
    let err = client
        .list_tasks(&list(Some(1), Some(page.next_page_token.clone())))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
    let err = client
        .list_tasks(&ListTasksRequest {
            context_id: Some("ctx-b".into()),
            ..list(Some(1), Some(page.next_page_token))
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn a_caller_sees_only_their_own_tasks_and_their_tokens_do_not_work_for_others() {
    let server = server().await;
    let alice = server.client(Some(TOKEN)).await;
    let bob = server.client(Some(OTHER_TOKEN)).await;
    let alice_ids = five_tasks(&alice).await;
    let bobs = task_of(
        bob.send_message(&send("bob's", Some("ctx-a")))
            .await
            .unwrap(),
    );
    wait_for_state(&bob, &bobs.id, TaskState::Completed).await;

    let theirs = bob.list_tasks(&list(None, None)).await.unwrap();
    assert_eq!(theirs.tasks.len(), 1);
    assert_eq!(theirs.tasks[0].id, bobs.id);
    assert_eq!(
        theirs.total_size, 1,
        "no count leaks the other caller's tasks"
    );
    let mine = alice.list_tasks(&list(None, None)).await.unwrap();
    assert_eq!(mine.total_size, 5);
    assert!(mine.tasks.iter().all(|t| alice_ids.contains(&t.id)));
    // The same context id, still only their own.
    let ctx = ListTasksRequest {
        context_id: Some("ctx-a".into()),
        ..list(None, None)
    };
    assert_eq!(bob.list_tasks(&ctx).await.unwrap().tasks.len(), 1);
    assert_eq!(alice.list_tasks(&ctx).await.unwrap().tasks.len(), 3);
    // Alice's cursor is refused for Bob (and cannot be used to reach her tasks).
    let page = alice.list_tasks(&list(Some(1), None)).await.unwrap();
    let token = page.next_page_token;
    assert!(!token.is_empty());
    let err = bob
        .list_tasks(&list(Some(1), Some(token)))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn a_token_that_is_not_ours_is_refused_with_one_message() {
    let server = server().await;
    let client = server.client(Some(TOKEN)).await;
    five_tasks(&client).await;
    let mut messages = std::collections::HashSet::new();
    for junk in ["not-a-token", "!!!", "eyJ2IjoxfQ", "AAAA", "e30"] {
        let err = client
            .list_tasks(&list(Some(2), Some(junk.to_owned())))
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS, "{junk}");
        messages.insert(err.message);
    }
    assert_eq!(
        messages.len(),
        1,
        "the client learns nothing about why: {messages:?}"
    );
}

#[tokio::test]
async fn listing_needs_authentication() {
    let server = server().await;
    let anonymous = server.client(None).await;
    let err = anonymous.list_tasks(&list(None, None)).await.unwrap_err();
    assert!(err.message.contains("401") || err.code != 0, "{err:?}");
}
