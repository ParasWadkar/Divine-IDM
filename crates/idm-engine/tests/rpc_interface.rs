//! The aria2-compatible RPC, over a real socket.
//!
//! `rpc`'s unit tests cover the pieces — gids, state names, the token
//! comparison. They cannot cover the thing a client actually depends on:
//! that the JSON-RPC envelope comes back in the shape aria2 returns it in,
//! that a wrong token is refused with the code clients recognise, and that
//! `system.multicall` wraps each result the way the standard says. All
//! three are properties of the router, so this talks to the router.
//!
//! Bound to a loopback port the system picks, so the test never listens on
//! the network of whoever runs it.

use std::sync::Arc;

use idm_engine::Engine;
use serde_json::{Value, json};

const SECRET: &str = "a-test-secret";

/// An engine whose state folder is a temporary one.
///
/// Without `IDM_STATE_DIR`, `Engine::new` opens the store belonging to
/// whatever copy of DivinePulse is installed on this machine.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded setup, before anything reads the variable.
    unsafe { std::env::set_var("IDM_STATE_DIR", dir.path()) };
    dir
}

struct Served {
    base: String,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn served() -> Served {
    let dir = scratch();
    let engine = Engine::new(dir.path().join("downloads"), 4);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = idm_engine::rpc::serve(engine, listener, Arc::new(SECRET.to_owned()));
    Served { base, task, _dir: dir }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// One call, answered.
async fn ask(served: &Served, method: &str, params: Value) -> Value {
    client()
        .post(format!("{}/jsonrpc", served.base))
        .json(&json!({ "jsonrpc": "2.0", "id": "test", "method": method, "params": params }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_client_can_tell_what_it_is_talking_to() {
    // `system.listMethods` is deliberately the one call that needs no
    // token: it is how a client discovers whether this speaks its language
    // at all, and it says nothing about the machine.
    let served = served().await;
    let answer = ask(&served, "system.listMethods", json!([])).await;
    let listed = answer["result"].as_array().expect("a list of methods");
    assert!(listed.iter().any(|name| name == "aria2.addUri"));
    assert!(listed.iter().any(|name| name == "aria2.tellActive"));
}

#[tokio::test]
async fn a_wrong_token_is_refused_with_the_code_clients_know() {
    // -32098 is what aria2 returns, and what every client matches on to
    // tell "your secret is wrong" apart from "that method is not here".
    let served = served().await;
    let answer = ask(&served, "aria2.getVersion", json!(["token:not-the-secret"])).await;
    assert_eq!(answer["error"]["code"], -32098, "{answer}");
    assert!(answer.get("result").is_none());
}

#[tokio::test]
async fn no_token_at_all_is_refused_too() {
    // Not an authorisation hole shaped like an omission: a call with no
    // parameters must not read as a call whose secret happened to match.
    let served = served().await;
    let answer = ask(&served, "aria2.getVersion", json!([])).await;
    assert_eq!(answer["error"]["code"], -32098, "{answer}");
}

#[tokio::test]
async fn the_right_token_gets_through() {
    let served = served().await;
    let answer = ask(&served, "aria2.getVersion", json!([format!("token:{SECRET}")])).await;
    assert!(answer["result"]["version"].is_string(), "{answer}");
    // Named rather than left empty, so a client can see this is not an old
    // aria2 with its features switched off.
    assert_eq!(answer["result"]["enabledFeatures"][0], "DivinePulse");
}

#[tokio::test]
async fn a_method_that_is_not_here_says_so() {
    // Including the ones deliberately left out: changing settings is not an
    // unimplemented feature, it is a decision.
    let served = served().await;
    for method in ["aria2.changeGlobalOption", "aria2.shutdown", "aria2.nonsense"] {
        let answer = ask(&served, method, json!([format!("token:{SECRET}")])).await;
        assert_eq!(answer["error"]["code"], -32601, "{method}: {answer}");
    }
}

#[tokio::test]
async fn global_stats_come_back_as_strings_like_aria2s_do() {
    // Every number in aria2's RPC is a string. It looks wrong and it is the
    // contract: clients parse them as strings, and a real integer breaks
    // the ones that do.
    let served = served().await;
    let answer = ask(&served, "aria2.getGlobalStat", json!([format!("token:{SECRET}")])).await;
    let stats = &answer["result"];
    for field in ["downloadSpeed", "numActive", "numWaiting", "numStopped"] {
        assert!(stats[field].is_string(), "{field} should be a string: {answer}");
    }
}

#[tokio::test]
async fn telling_about_nothing_gives_an_empty_list_not_an_error() {
    // A fresh engine has no downloads, and a client polling it should draw
    // an empty table rather than show a failure.
    let served = served().await;
    for method in ["aria2.tellActive", "aria2.tellWaiting", "aria2.tellStopped"] {
        let answer = ask(&served, method, json!([format!("token:{SECRET}"), 0, 100])).await;
        assert_eq!(answer["result"], json!([]), "{method}: {answer}");
    }
}

#[tokio::test]
async fn a_gid_nobody_has_heard_of_is_an_error_in_aria2s_wording() {
    // Clients match on this message to drop a row they were polling.
    let served = served().await;
    let answer =
        ask(&served, "aria2.tellStatus", json!([format!("token:{SECRET}"), "00000000deadbeef"]))
            .await;
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("No such download"), "{answer}");
}

#[tokio::test]
async fn multicall_wraps_each_result_in_its_own_array() {
    // The shape the standard specifies and aria2 follows: a success is a
    // one-element array, a fault is a bare object, and clients tell them
    // apart by that. AriaNg polls active, waiting and stopped in one trip
    // this way, so getting it wrong breaks the commonest client there is.
    let served = served().await;
    let token = format!("token:{SECRET}");
    let answer = ask(
        &served,
        "system.multicall",
        json!([[
            { "methodName": "aria2.tellActive", "params": [token] },
            { "methodName": "aria2.nonsense", "params": [token] },
        ]]),
    )
    .await;

    let results = answer["result"].as_array().expect("one entry per call");
    assert_eq!(results.len(), 2, "{answer}");
    // The good one: wrapped.
    assert_eq!(results[0], json!([[]]), "{answer}");
    // The bad one: not wrapped, and carrying a code.
    assert_eq!(results[1]["code"], -32601, "{answer}");
}

#[tokio::test]
async fn a_batch_is_answered_in_order() {
    // A top-level array is a different thing from system.multicall, and
    // some clients use it. One reply per call, in the order asked.
    let served = served().await;
    let token = format!("token:{SECRET}");
    let answer: Value = client()
        .post(format!("{}/jsonrpc", served.base))
        .json(&json!([
            { "jsonrpc": "2.0", "id": "one", "method": "aria2.getVersion", "params": [token] },
            { "jsonrpc": "2.0", "id": "two", "method": "system.listMethods", "params": [] },
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let replies = answer.as_array().expect("an array of replies");
    assert_eq!(replies.len(), 2, "{answer}");
    assert_eq!(replies[0]["id"], "one");
    assert_eq!(replies[1]["id"], "two");
}

#[tokio::test]
async fn a_route_that_is_not_the_endpoint_is_not_there() {
    // The whole server is two paths. Everything the window talks to, and
    // everything the web interface serves, is absent from this one.
    let served = served().await;
    for path in ["/", "/api/jobs", "/index.html"] {
        let response =
            client().post(format!("{}{path}", served.base)).json(&json!({})).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path} should not exist");
    }
}

#[tokio::test]
async fn rubbish_is_refused_without_taking_the_server_down() {
    // A malformed body is an invalid request, not a panic, and the next
    // call still works.
    let served = served().await;
    let response = client()
        .post(format!("{}/jsonrpc", served.base))
        .body("this is not json")
        .send()
        .await
        .unwrap();
    let answer: Value = response.json().await.unwrap();
    assert_eq!(answer["error"]["code"], -32600, "{answer}");

    let after = ask(&served, "system.listMethods", json!([])).await;
    assert!(after["result"].is_array(), "the server should still be answering");
}
