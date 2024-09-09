#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::module_name_repetitions)]

use futures::StreamExt;
use tower_service::Service;

mod router;

#[tokio::main]
async fn main() {
	std::env::set_var("RUST_LOG", "hyper,hyper_util,http,tokio,tokio_native_tls,tokio_tungstenite,tor_rtcompat,tor_hsservice,tor_client");
	std::env::set_var("RUST_BACKTRACE", "full");
	std::env::set_var("log_level", "debug");
	tracing_subscriber::fmt().init();

	// axum router
	let app = crate::router::router();

	// tls
	let c = include_bytes!("../self_signed_certs/cert.pem");
	let k = include_bytes!("../self_signed_certs/key.pem");
	let identity = native_tls::Identity::from_pkcs8(c, k).unwrap();
	let tls_acceptor = tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::builder(identity).build().unwrap());

	// server
	let mut tor_config = arti_client::config::TorClientConfigBuilder::default();
	tor_config.storage().cache_dir(arti_client::config::CfgPath::new("./temp/arti/cache".to_string()));
	tor_config.storage().state_dir(arti_client::config::CfgPath::new("./temp/arti/data".to_string()));
	tor_config.address_filter().allow_onion_addrs(true);
	let tor_config = tor_config.build().unwrap();

	let runtime = tor_rtcompat::tokio::TokioNativeTlsRuntime::current().unwrap();
	let tor_client = arti_client::TorClient::with_runtime(runtime);
	let tor_client = tor_client.config(tor_config).create_bootstrapped().await.unwrap();
	let nickname = "my_test_websocket_server".parse::<tor_hsservice::HsNickname>().unwrap();
	let service_config = tor_hsservice::config::OnionServiceConfigBuilder::default().nickname(nickname).build().unwrap();
	let (service, request_stream) = tor_client.launch_onion_service(service_config).unwrap();
	let service_name = service.onion_name().unwrap().to_string();
	balens_log::log(balens_log::Level::Info, format!("serving onion address: {service_name}"));
	let stream_requests = tor_hsservice::handle_rend_requests(request_stream);
	tokio::pin!(stream_requests);

	balens_log::log(balens_log::Level::Info, "listening for incoming requests".to_string());

	// requests
	while let Some(stream_request) = stream_requests.next().await {
		let tls_acceptor = tls_acceptor.clone();
		let app = app.clone();

		balens_log::log(balens_log::Level::Info, "handling incoming request".to_string());

		tokio::spawn(async move {
			balens_log::log(balens_log::Level::Debug, "spawned request runtime".to_string());
			let app = app.clone();

			match stream_request.request() {
				tor_proto::stream::IncomingStreamRequest::Begin(_begin) => {
					balens_log::log(balens_log::Level::Info, "found BEGIN request on port 443".to_string());

					let onion_service_stream = stream_request.accept(tor_cell::relaycell::msg::Connected::new_empty()).await.unwrap();
					let connect_info = ConnectionInfo { circuit_id: Some(onion_service_stream.circuit().unique_id().to_string()), socket_addr: None };

					balens_log::log(balens_log::Level::Info, "tor: accepted incoming request".to_string());

					let tls_onion_service_stream = tls_acceptor.accept(onion_service_stream).await.unwrap();
					balens_log::log(balens_log::Level::Info, "tls: accepted incoming request".to_string());

					// accept websocket stream
					//let ws_onion_service_stream = tokio_tungstenite::accept_async(tls_onion_service_stream).await.unwrap();

					//let tls_onion_service_stream = tls_acceptor.accept(tls_onion_service_stream).await.unwrap();
					let stream = hyper_util::rt::tokio::TokioIo::new(tls_onion_service_stream);
					//balens_log::log(balens_log::Level::Info, "tls: accepted incoming request again".to_string());

					let hyper_service = hyper::service::service_fn(move |request| {
						balens_log::log(balens_log::Level::Info, "hyper service spwned".to_string());
						let app = app.clone();
						let connect_info = connect_info.clone();
						std::thread::spawn(move || {
							balens_log::log(balens_log::Level::Debug, "request thread spwned".to_string());
							let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

							#[allow(clippy::async_yields_async)]
							runtime.block_on(async {
								balens_log::log(balens_log::Level::Debug, "request runtime spawned".to_string());
								hyper_service(request, app, connect_info).await
							})
						})
						.join()
						.unwrap()
					});

					balens_log::log(balens_log::Level::Info, "spawing hyper service...".to_string());

					/* ****************************************** */
					/* this line never launches the hyper service */
					/* ****************************************** */
					hyper_util::server::conn::auto::Builder::new(hyper_util::rt::tokio::TokioExecutor::new()).serve_connection_with_upgrades(stream, hyper_service).await.unwrap();

					balens_log::log(balens_log::Level::Info, "closed hyper service...".to_string());
				}
				_ => balens_log::log(balens_log::Level::Info, "ignoring non-http request".to_string()),
			}
		});

		balens_log::log(balens_log::Level::Info, "request handled".to_string());
	}
}

async fn hyper_service(request: hyper::Request<hyper::body::Incoming>, app: axum::Router, connect_info: ConnectionInfo) -> axum::routing::future::RouteFuture<std::convert::Infallible> {
	balens_log::log(balens_log::Level::Info, "lauching axum service...".to_string());

	app.clone().into_make_service_with_connect_info::<ConnectionInfo>().call(connect_info).await.unwrap().call(request)
}

#[derive(Clone, Debug, Default)]
pub struct ConnectionInfo {
	pub circuit_id: Option<String>,
	pub socket_addr: Option<std::net::SocketAddr>,
}

impl axum::extract::connect_info::Connected<http::request::Request<hyper::body::Incoming>> for ConnectionInfo {
	fn connect_info(target: http::request::Request<hyper::body::Incoming>) -> Self {
		Self { circuit_id: target.extensions().get::<Self>().unwrap().circuit_id.clone(), socket_addr: None }
	}
}

impl axum::extract::connect_info::Connected<Self> for ConnectionInfo {
	fn connect_info(target: Self) -> Self {
		target
	}
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_>> for ConnectionInfo {
	fn connect_info(target: axum::serve::IncomingStream<'_>) -> Self {
		Self { circuit_id: None, socket_addr: Some(target.local_addr().unwrap()) }
	}
}
