use std;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio_service::Service as TokioService;

use jsonrpc::{MetaIoHandler, Metadata, Middleware, NoopMiddleware};
use jsonrpc::futures::{future, Future, Stream, Sink};
use jsonrpc::futures::sync::{mpsc, oneshot};
use server_utils::{reactor, tokio_core};
use server_utils::tokio_io::AsyncRead;

use dispatch::{Dispatcher, SenderChannels, PeerMessageQueue};
use line_codec::LineCodec;
use meta::{MetaExtractor, RequestContext, NoopExtractor};
use service::Service;

/// TCP server builder
pub struct ServerBuilder<M: Metadata = (), S: Middleware<M> = NoopMiddleware> {
	remote: reactor::UninitializedRemote,
	handler: Arc<MetaIoHandler<M, S>>,
	meta_extractor: Arc<MetaExtractor<M>>,
	channels: Arc<SenderChannels>,
}

impl<M: Metadata, S: Middleware<M> + 'static> ServerBuilder<M, S> {
	/// Creates new `SeverBuilder` wih given `IoHandler`
	pub fn new<T>(handler: T) -> Self where
		T: Into<MetaIoHandler<M, S>>
	{
		ServerBuilder {
			remote: reactor::UninitializedRemote::Unspawned,
			handler: Arc::new(handler.into()),
			meta_extractor: Arc::new(NoopExtractor),
			channels: Default::default(),
		}
	}

	/// Utilize existing event loop remote.
	pub fn event_loop_remote(mut self, remote: tokio_core::reactor::Remote) -> Self {
		self.remote = reactor::UninitializedRemote::Shared(remote);
		self
	}

	/// Sets session meta extractor
	pub fn session_meta_extractor<T: MetaExtractor<M> + 'static>(mut self, meta_extractor: T) -> Self {
		self.meta_extractor = Arc::new(meta_extractor);
		self
	}

	/// Starts a new server
	pub fn start(self, addr: &SocketAddr) -> std::io::Result<Server> {
		let meta_extractor = self.meta_extractor.clone();
		let rpc_handler = self.handler.clone();
		let channels = self.channels.clone();
		let address = addr.to_owned();
		let (tx, rx) = std::sync::mpsc::channel();
		let (signal, stop) = oneshot::channel();

		let remote = self.remote.initialize()?;

		remote.remote().spawn(move |handle| {
			let start = move || {
				let listener = tokio_core::net::TcpListener::bind(&address, handle)?;
				let connections = listener.incoming();
				let remote = handle.remote().clone();
				let server = connections.for_each(move |(socket, peer_addr)| {
					trace!(target: "tcp", "Accepted incoming connection from {}", &peer_addr);
					let (sender, receiver) = mpsc::channel(65536);

					let context = RequestContext {
						peer_addr: peer_addr,
						sender: sender.clone(),
					};

					let meta = meta_extractor.extract(&context);
					let service = Service::new(peer_addr, rpc_handler.clone(), meta);
					let (writer, reader) = socket.framed(LineCodec).split();

					let responses = reader.and_then(
						move |req| service.call(req).then(|response| match response {
							Err(e) => {
								warn!(target: "tcp", "Error while processing request: {:?}", e);
								future::ok(String::new())
							},
							Ok(None) => {
								trace!(target: "tcp", "JSON RPC request produced no response");
								future::ok(String::new())
							},
							Ok(Some(response_data)) => {
								trace!(target: "tcp", "Sent response: {}", &response_data);
								future::ok(response_data)
							}
						})
					);

					let peer_message_queue = {
						let mut channels = channels.lock();
						channels.insert(peer_addr.clone(), sender.clone());

						PeerMessageQueue::new(
							responses,
							receiver,
							peer_addr.clone(),
						)
					};

					let shared_channels = channels.clone();
					let writer = writer.send_all(peer_message_queue).then(move |_| {
						trace!(target: "tcp", "Peer {}: service finished", peer_addr);
						let mut channels = shared_channels.lock();
						channels.remove(&peer_addr);
						Ok(())
					});

					remote.spawn(|_| writer);

					Ok(())
				});

				Ok(server)
			};

			let stop = stop.map_err(|_| std::io::ErrorKind::Interrupted.into());
			match start() {
				Ok(server) => {
					tx.send(Ok(())).expect("Rx is blocking parent thread.");
					future::Either::A(server.select(stop)
						.map(|_| ())
						.map_err(|(e, _)| {
							error!("Error while executing the server: {:?}", e);
						}))
				},
				Err(e) => {
					tx.send(Err(e)).expect("Rx is blocking parent thread.");
					future::Either::B(stop
						.map_err(|e| {
							error!("Error while executing the server: {:?}", e);
						}))
				},
			}
		});

		let res = rx.recv().expect("Response is always sent before tx is dropped.");

		res.map(|_| Server {
			remote: Some(remote),
			stop: Some(signal),
		})
	}

	/// Returns dispatcher
	pub fn dispatcher(&self) -> Dispatcher {
		Dispatcher::new(self.channels.clone())
	}
}

/// TCP Server handle
pub struct Server {
	remote: Option<reactor::Remote>,
	stop: Option<oneshot::Sender<()>>,
}

impl Server {
	/// Closes the server (waits for finish)
	pub fn close(mut self) {
		let _ = self.stop.take().map(|sg| sg.send(()));
		self.remote.take().unwrap().close();
	}

	/// Wait for the server to finish
	pub fn wait(mut self) {
		self.remote.take().unwrap().close();
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.stop.take().map(|sg| sg.send(()));
		self.remote.take().map(|remote| remote.close());
	}
}
