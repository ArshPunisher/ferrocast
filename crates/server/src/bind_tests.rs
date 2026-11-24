//! Opening the local server's listener on a computer short of network buffers (the owner's
//! Mac, 24 Nov: "The local server couldn't start: No buffer space available (os error 55)",
//! gone after a restart): it waits and tries again, and says plainly what to do if it stays.

use std::io::{Error, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;

use super::{bind_listener, is_transient_bind_error};

/// "No buffer space available" on this OS (ENOBUFS).
fn no_buffers() -> Error {
    #[cfg(target_os = "macos")]
    return Error::from_raw_os_error(55);
    #[cfg(target_os = "windows")]
    return Error::from_raw_os_error(10055);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return Error::from_raw_os_error(105);
}

const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;
const TICK: Duration = Duration::from_millis(1);

async fn free(a: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((a.ip(), 0))).await
}

#[test]
fn no_buffer_space_is_a_wait_not_a_failure() {
    assert!(is_transient_bind_error(&no_buffers()));
    assert!(!is_transient_bind_error(&Error::from(ErrorKind::AddrInUse)));
    assert!(!is_transient_bind_error(&Error::from(ErrorKind::PermissionDenied)));
}

#[tokio::test]
async fn a_computer_short_of_buffers_for_a_while_still_gets_its_server() {
    let calls = AtomicU32::new(0);
    // The first three rounds fail at the first port; the fourth works.
    let l = bind_listener(LOCAL, 7000, 8, TICK, |a| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        async move { if n < 3 { Err(no_buffers()) } else { free(a).await } }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(l.local_addr().is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 4, "it waited and tried again, no more than needed");
}

#[tokio::test]
async fn a_computer_that_stays_short_gets_a_plain_message_with_what_to_do() {
    let calls = AtomicU32::new(0);
    let err = bind_listener(LOCAL, 7000, 4, TICK, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Err(no_buffers()) }
    })
    .await
    .map(drop)
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 4, "one try for each round");
    let said = err.to_string();
    assert!(said.contains("run out of network resources"), "{said}");
    assert!(said.contains("restart your computer"), "what to do: {said}");
}

#[tokio::test]
async fn ports_in_use_still_move_on_and_a_free_port_is_the_last_resort() {
    let calls = AtomicU32::new(0);
    let l = bind_listener(LOCAL, 7000, 8, TICK, |a| {
        calls.fetch_add(1, Ordering::SeqCst);
        async move { if a.port() == 0 { free(a).await } else { Err(Error::from(ErrorKind::AddrInUse)) } }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(l.local_addr().is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 21, "the 20 ports, then any free one");
}

#[tokio::test]
async fn another_failure_ends_it_at_once_without_waiting() {
    let calls = AtomicU32::new(0);
    let err = bind_listener(LOCAL, 7000, 8, Duration::from_secs(30), |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Err(Error::from(ErrorKind::PermissionDenied)) }
    })
    .await
    .map(drop)
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 21, "one round, no retries");
}
