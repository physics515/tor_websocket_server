use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;

use super::ConnectionInfo;

pub fn router() -> axum::Router {
	axum::Router::new()
		.route("/ws", axum::routing::get(ws_handler))
		.route("/", axum::routing::post(echo))
		// logging so we can see whats going on
		.layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(tower_http::trace::DefaultMakeSpan::default().include_headers(true)))
}

#[axum::debug_handler]
async fn ws_handler(ws: axum::extract::ws::WebSocketUpgrade, axum::extract::connect_info::ConnectInfo(connection_info): axum::extract::connect_info::ConnectInfo<ConnectionInfo>) -> impl axum::response::IntoResponse {
	let connection_id = if let Some(circuit_id) = connection_info.circuit_id {
		circuit_id
	} else if let Some(socket_addr) = connection_info.socket_addr {
		socket_addr.to_string()
	} else {
		"unknown".to_string()
	};

	balens_log::log(balens_log::Level::Info, format!("`{connection_id}` connected."));

	// finalize the upgrade process by returning upgrade callback.
	// we can customize the callback by sending additional info such as address.
	ws.on_upgrade(move |socket| handle_socket(socket, connection_id))
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(mut socket: axum::extract::ws::WebSocket, who: String) {
	// send a ping (unsupported by some browsers) just to kick things off and get a response
	if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
		balens_log::log(balens_log::Level::Info, format!("Pinged {who}..."));
	} else {
		balens_log::log(balens_log::Level::Info, format!("Could not send ping {who}!"));
		// no Error here since the only thing we can do is to close the connection.
		// If we can not send messages, there is no way to salvage the statemachine anyway.
		return;
	}

	// receive single message from a client (we can either receive or send with socket).
	// this will likely be the Pong for our Ping or a hello message from client.
	// waiting for message from a client will block this task, but will not block other client's
	// connections.
	if let Some(msg) = socket.recv().await {
		if let Ok(msg) = msg {
			if process_message(msg, &who).is_break() {
				return;
			}
		} else {
			balens_log::log(balens_log::Level::Info, format!("client {who} abruptly disconnected"));
			return;
		}
	}

	// Since each client gets individual statemachine, we can pause handling
	// when necessary to wait for some external event (in this case illustrated by sleeping).
	// Waiting for this client to finish getting its greetings does not prevent other clients from
	// connecting to server and receiving their greetings.
	for i in 1..5 {
		if socket.send(axum::extract::ws::Message::Text(format!("Hi {i} times!"))).await.is_err() {
			balens_log::log(balens_log::Level::Info, format!("client {who} abruptly disconnected"));
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}

	// By splitting socket we can send and receive at the same time. In this example we will send
	// unsolicited messages to client based on some sort of server's internal event (i.e .timer).
	let (mut sender, mut receiver) = socket.split();
	let who1 = who.to_string();

	// Spawn a task that will push several messages to the client (does not matter what client does)
	let mut send_task = tokio::spawn(async move {
		let who = who1.clone();
		let n_msg = 20;
		for i in 0..n_msg {
			// In case of any websocket error, we exit.
			if sender.send(axum::extract::ws::Message::Text(format!("Server message {i} ..."))).await.is_err() {
				return i;
			}

			tokio::time::sleep(std::time::Duration::from_millis(300)).await;
		}

		println!("Sending close to {who}...");
		if let Err(e) = sender.send(axum::extract::ws::Message::Close(Some(axum::extract::ws::CloseFrame { code: axum::extract::ws::close_code::NORMAL, reason: std::borrow::Cow::from("Goodbye") }))).await {
			balens_log::log(balens_log::Level::Error, format!("Could not send Close due to {e}, probably it is ok?"));
		}
		n_msg
	});

	let who2 = who.clone();
	// This second task will receive messages from client and print them on server console
	let mut recv_task = tokio::spawn(async move {
		let who = who2.clone();
		let mut cnt = 0;
		while let Some(Ok(msg)) = receiver.next().await {
			let who = who.clone();
			cnt += 1;
			// print message and break if instructed to do so
			if process_message(msg, &who).is_break() {
				break;
			}
		}
		cnt
	});

	// If any one of the tasks exit, abort the other.
	tokio::select! {
		rv_a = (&mut send_task) => {
			match rv_a {
				Ok(a) => balens_log::log(balens_log::Level::Info, format!("{a} messages sent to {who}")),
				Err(a) => balens_log::log(balens_log::Level::Error, format!("Error sending messages {a:?}")),
			}
			recv_task.abort();
		},
		rv_b = (&mut recv_task) => {
			match rv_b {
				Ok(b) => balens_log::log(balens_log::Level::Info, format!("Received {b} messages")),
				Err(b) => balens_log::log(balens_log::Level::Error, format!("Error receiving messages {b:?}")),
			}
			send_task.abort();
		}
	}

	// returning from the handler closes the websocket connection
	balens_log::log(balens_log::Level::Info, format!("Websocket context {who} destroyed"));
}

/// helper to print contents of messages to stdout. Has special treatment for Close.
fn process_message(msg: axum::extract::ws::Message, who: &str) -> std::ops::ControlFlow<(), ()> {
	match msg {
		axum::extract::ws::Message::Text(t) => {
			println!(">>> {who} sent str: {t:?}");
		}
		axum::extract::ws::Message::Binary(d) => {
			println!(">>> {} sent {} bytes: {:?}", who, d.len(), d);
		}
		axum::extract::ws::Message::Close(c) => {
			if let Some(cf) = c {
				println!(">>> {} sent close with code {} and reason `{}`", who, cf.code, cf.reason);
			} else {
				println!(">>> {who} somehow sent close message without CloseFrame");
			}
			return std::ops::ControlFlow::Break(());
		}

		axum::extract::ws::Message::Pong(v) => {
			println!(">>> {who} sent pong with {v:?}");
		}
		// You should never need to manually handle Message::Ping, as axum's websocket library
		// will do so for you automagically by replying with Pong and copying the v according to
		// spec. But if you need the contents of the pings you can see them here.
		axum::extract::ws::Message::Ping(v) => {
			println!(">>> {who} sent ping with {v:?}");
		}
	}
	std::ops::ControlFlow::Continue(())
}

// echo

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Req {
	pub headers: String,
	pub body: String,
	pub method: String,
	pub version: String,
}

#[axum::debug_handler]
async fn echo(request: axum::extract::Request) -> Result<axum::extract::Json<serde_json::Value>, String> {
	balens_log::log(balens_log::Level::Info, "echo requst recieved.".to_string());

	let mut headers = Vec::new();
	let req_headers = request.headers().clone();
	for (key, value) in &req_headers {
		let str = format!("{key}: {value:?}");
		headers.push(str);
	}
	let headers: String = headers.join(",");
	let method = request.method().to_string();
	let version = format!("{:?}", request.version());
	let buf = request.collect().await.unwrap().to_bytes();
	let body = String::from_utf8(buf.to_vec()).unwrap();

	let req = Req { headers, body, method, version };

	let json = serde_json::json!(req);
	Ok(axum::extract::Json(json))
}
