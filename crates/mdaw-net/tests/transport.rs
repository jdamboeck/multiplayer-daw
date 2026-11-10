//! Real QUIC over loopback: frames across a pinned connection, and no connection without the
//! right pin.

use mdaw_net::{Identity, JoinInfo, close, connect, listen, read_frame, write_frame};
use mdaw_proto::{ClientMsg, Hello, PROTO_VERSION, RefuseReason, ServerMsg};
use std::net::SocketAddr;

fn hello(name: &str) -> ClientMsg {
    ClientMsg::Hello(Hello {
        proto_version: PROTO_VERSION,
        token: "t".into(),
        user_key: [7; 16],
        name: name.into(),
        resume: None,
    })
}

fn join_info(addr: SocketAddr, fingerprint: [u8; 32]) -> JoinInfo {
    JoinInfo {
        host: addr.ip().to_string(),
        port: addr.port(),
        fingerprint,
        token: "t".into(),
    }
}

/// A server that answers every hello with a refusal naming the protocol version, then ends the
/// stream when the client does.
async fn serve(endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else { return };
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            while let Some(msg) = read_frame::<ClientMsg>(&mut recv).await.unwrap() {
                let ClientMsg::Hello(hello) = msg else {
                    panic!("unexpected message")
                };
                let reply = ServerMsg::Refused(RefuseReason::ProtoVersion(hello.name.len() as u32));
                write_frame(&mut send, &reply).await.unwrap();
            }
            send.finish().unwrap();
            connection.closed().await;
        });
    }
}

#[tokio::test]
async fn frames_cross_a_pinned_connection() {
    let identity = Identity::generate().unwrap();
    let server = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(serve(server.clone()));

    let (endpoint, connection) = connect(&join_info(addr, identity.fingerprint())).await.unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    // A frame far larger than a packet, and small ones behind it.
    for name in ["x".repeat(300_000), "ann".into(), String::new()] {
        write_frame(&mut send, &hello(&name)).await.unwrap();
        let reply: Option<ServerMsg> = read_frame(&mut recv).await.unwrap();
        assert_eq!(
            reply,
            Some(ServerMsg::Refused(RefuseReason::ProtoVersion(name.len() as u32)))
        );
    }
    send.finish().unwrap();
    assert_eq!(
        read_frame::<ServerMsg>(&mut recv).await.unwrap(),
        None,
        "a clean end of stream"
    );
    close(&endpoint, &connection).await;
    server.close(0u32.into(), b"");
}

#[tokio::test]
async fn a_server_with_another_certificate_is_refused() {
    let identity = Identity::generate().unwrap();
    let server = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(serve(server.clone()));

    let mut wrong = identity.fingerprint();
    wrong[0] ^= 1;
    let error = connect(&join_info(addr, wrong))
        .await
        .expect_err("the pin does not match");
    assert!(
        format!("{error:#}").contains("does not match the join string"),
        "{error:#}"
    );

    // Someone else's certificate under the right address fails the same way.
    let other = Identity::generate().unwrap();
    assert!(connect(&join_info(addr, other.fingerprint())).await.is_err());
    assert!(connect(&join_info(addr, identity.fingerprint())).await.is_ok());
}

#[tokio::test]
async fn an_identity_is_created_once_and_then_kept() {
    let dir = tempfile::tempdir().unwrap();
    let first = Identity::load_or_create(dir.path()).unwrap();
    let second = Identity::load_or_create(dir.path()).unwrap();
    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_ne!(first.fingerprint(), Identity::generate().unwrap().fingerprint());

    // The stored identity is usable.
    let server = listen("127.0.0.1:0".parse().unwrap(), &second).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(serve(server.clone()));
    assert!(connect(&join_info(addr, first.fingerprint())).await.is_ok());
}
