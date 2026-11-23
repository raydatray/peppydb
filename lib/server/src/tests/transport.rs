//! Socket options of accepted connections.

use futures_util::StreamExt;
use tokio::net::{TcpListener, TcpStream};

use crate::incoming;

/// Accepted connections disable Nagle's algorithm, so small response
/// writes do not wait for the client's delayed ACK.
#[tokio::test]
async fn accepted_connections_set_nodelay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut accepted = incoming(listener);
    let (client, server) = tokio::join!(TcpStream::connect(addr), accepted.next());
    let _client = client.unwrap();
    let server = server.expect("listener stream ended").unwrap();
    assert!(server.nodelay().unwrap());
}
