//! Several frontends attached to one live agent.
//!
//! The transport here is a `tokio::io::duplex` pair per client, which is the
//! whole reason `WireServer::serve_connection` takes a reader and a writer
//! rather than reaching for stdio: attaching a second client is calling it a
//! second time, and a test can do that as easily as a listener does.

mod wire_agent_fixture;

use serde_json::{Value, json};

use dvadva_agent::wire::WIRE_PROTOCOL_VERSION;

use wire_agent_fixture::{
    TestClient, asking_agent, event_type, is_answer_to, is_event, request_type, talking_agent,
};

// ----------------------------------------------------------------- the tests

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_initialize_on_one_agent_without_their_ids_colliding() {
    let agent = talking_agent();
    let mut first = TestClient::attach(&agent.server);
    let mut second = TestClient::attach(&agent.server);

    let first_result = first.initialize().await;
    let second_result = second.initialize().await;

    for result in [&first_result, &second_result] {
        assert_eq!(
            result.pointer("/result/protocol_version").unwrap(),
            WIRE_PROTOCOL_VERSION
        );
        assert_eq!(
            result.pointer("/result/capabilities/multi_client"),
            Some(&json!(true)),
            "1.3 advertises that a second client is welcome"
        );
    }

    // Both minted the id "1". Each must have been answered exactly once: a
    // shared outbound queue would have handed one client both answers.
    for client in [&mut first, &mut second] {
        let extra: Vec<Value> = client
            .drain()
            .await
            .into_iter()
            .filter(|frame| is_answer_to(frame, "1"))
            .collect();
        assert!(
            extra.is_empty(),
            "a client received another client's answer: {extra:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_is_broadcast_but_its_result_goes_only_to_the_client_that_asked() {
    let agent = talking_agent();
    let mut asker = TestClient::attach(&agent.server);
    let mut watcher = TestClient::attach(&agent.server);
    asker.initialize().await;
    watcher.initialize().await;
    watcher.drain().await;

    let prompt = asker.call("prompt", json!({"user_input": "hi"})).await;
    asker
        .wait_for("the prompt result", |frame| is_answer_to(frame, &prompt))
        .await;

    let seen = watcher.drain().await;
    let kinds: Vec<&str> = seen.iter().filter_map(event_type).collect();
    assert!(
        kinds.contains(&"TurnBegin") && kinds.contains(&"TurnEnd"),
        "the watcher should have seen the whole turn; saw {kinds:?}"
    );
    let answers: Vec<&Value> = seen
        .iter()
        .filter(|frame| frame.get("method").is_none())
        .collect();
    assert!(
        answers.is_empty(),
        "the watcher was sent someone else's answer: {answers:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_can_attach_to_a_parked_agent_and_answer_the_approval() {
    let agent = asking_agent();
    let mut first = TestClient::attach(&agent.server);
    first.initialize().await;

    let prompt = first.call("prompt", json!({"user_input": "go"})).await;
    let request = first
        .wait_for("the approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    let request_id = request
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // A second frontend arrives while the agent is parked. Neither the
    // initialize nor the replay may be refused for "a turn is in progress":
    // that state is exactly the one worth attaching to.
    let mut second = TestClient::attach(&agent.server);
    second.initialize().await;
    let replay = second.call("replay", json!({})).await;
    second
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;

    // And the request is handed over live, after the replay, so the newcomer
    // can actually answer it instead of just seeing a dead dialog.
    let handed_over = second
        .wait_for("the re-armed approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    assert_eq!(
        handed_over.get("id").and_then(Value::as_str),
        Some(request_id.as_str())
    );

    second
        .send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"request_id": request_id, "response": "approve"},
        }))
        .await;

    // The first client never answered, so its dialog has to come down on the
    // resolution event rather than hang there deciding nothing.
    let dismissal = first
        .wait_for("the approval resolution", |frame| {
            is_event(frame) && event_type(frame) == Some("ApprovalResponse")
        })
        .await;
    assert_eq!(
        dismissal.pointer("/params/payload/request_id"),
        Some(&json!(request_id))
    );

    let finished = first
        .wait_for("the prompt result", |frame| is_answer_to(frame, &prompt))
        .await;
    assert_eq!(finished.pointer("/result/status"), Some(&json!("finished")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_talking_still_gets_its_catch_up() {
    let agent = talking_agent();
    let mut client = TestClient::attach(&agent.server);
    client.initialize().await;
    let replay = client.call("replay", json!({})).await;

    // Half-close, the way a script that pipes its requests in and reads the
    // answers out does. Closing the read half means the client stopped
    // talking, not that it stopped listening, so its catch-up must finish.
    drop(client.writer.take());

    let answer = client
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;
    assert_eq!(answer.pointer("/result/status"), Some(&json!("finished")));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_tool_name_belongs_to_one_client_and_leaves_with_it() {
    let agent = talking_agent();
    let tool = json!([{
        "name": "peek",
        "description": "Ask the frontend.",
        "parameters": {"type": "object"},
    }]);

    let mut owner = TestClient::attach(&agent.server);
    let accepted = owner
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let accepted = owner
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &accepted)
        })
        .await;
    assert_eq!(
        accepted.pointer("/result/external_tools/accepted"),
        Some(&json!(["peek"]))
    );

    // A second client offering the same name is refused rather than silently
    // shadowing the first: only the registrant can service a call to it.
    let mut rival = TestClient::attach(&agent.server);
    let refused = rival
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let refused = rival
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &refused)
        })
        .await;
    assert_eq!(
        refused.pointer("/result/external_tools/rejected/0/name"),
        Some(&json!("peek"))
    );
    assert!(
        refused
            .pointer("/result/external_tools/accepted")
            .and_then(Value::as_array)
            .is_none_or(|accepted| accepted.is_empty())
    );

    // The claim is released when its client goes: a registration nobody can
    // service would hang the next turn that called it.
    owner.detach().await;
    rival.detach().await;

    let mut heir = TestClient::attach(&agent.server);
    let inherited = heir
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let inherited = heir
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &inherited)
        })
        .await;
    assert_eq!(
        inherited.pointer("/result/external_tools/accepted"),
        Some(&json!(["peek"])),
        "the name should be free once its owner detached"
    );
}
