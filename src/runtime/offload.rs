//! Writes command replies through the `on_io_complete` callback; heavy work goes to pool lanes.
//!
//! Two kinds of work go to lanes:
//!
//! * **Search.** The command submits it directly to the least busy lane.
//! * **Graph work.** When a write command (upsert, setmeta, delete) writes to the engine, the
//!   element event raised by that write makes the callback enqueue work on the vkey's lane
//!   (`event`). The command offers a node before writing (`event::offer`) and the callback puts
//!   the work into it. The node carries the connection, so the lane wakes it when done.
//!
//! ```text
//! [worker] execute       allocate a `Node` first; on failure, finish with SERVER_ERROR
//!                        waitfor_io_complete → on_io_complete(wake, node)
//!          write command offer the node, then write to the engine; the callback enqueues it
//!                        if the callback did not claim it (no element changed), withdraw the wait
//!          search        enqueue the node on a lane
//!                        → return true; the core routes to conn_io_callback and parks the conn
//! [lane]   run_node      graph work or search. **No engine writes.**
//!                        store the result in the node, then notify_io_complete
//! [worker] wake          from conn_io_callback: reclaim the node and build the reply via
//!                        response_handler. **No engine writes.**
//! ```
//!
//! Commands with nothing to offload reply through the same callback (registered without
//! `waitfor`, the core invokes it immediately). If an engine write made the connection wait for a
//! replication ack, the reply goes out only after that wait completes -- there is a single reply
//! path.
//!
//! # Why engine writes stay in `execute`
//!
//! Replication hooks writes with **thread-local** state. `ACTION_BEFORE_WRITE` in
//! `default_engine.c` calls `rp_before_check`, which sets `tls_cookie`, and `rp_after_check` only
//! sees it on the same thread. Calling `store` from a pool thread silently bypasses the
//! replication hook. Writing from the callback could require another wait, which
//! `conn_io_callback` does not support.
//!
//! Reads are unrestricted: `ACTION_BEFORE_READ` exists only under `ENABLE_MIGRATION`, which is
//! disabled. So searches run entirely on the pool, while for writes the lane only does the graph
//! work after the worker has written to the engine.
//!
//! # Why holding the cookie is safe
//!
//! A waiting connection is removed from the event loop, so nothing notices if the client
//! disconnects -- the only path to `conn_close` is the state machine, which is paused. The conn
//! therefore stays alive until `notify_io_complete` wakes it, and holding its address is safe.

use std::os::raw::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr;

use crate::cmd;
use crate::engine::{Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::ffi::server::{self, Responder, ResponseHandler};
use crate::protocol::Request;
use crate::protocol::request::{Body, Line, SearchKey, SearchVector};
use crate::runtime::pool::{self, Entry};
use crate::trigger::apply::{self, Graph};
use crate::trigger::event;

/// A search in a form a lane can take with it.
///
/// `Request` cannot be passed as is: `Line` borrows the token buffer, which the next command
/// overwrites once `execute` returns. Search requests are already owned types, so they move
/// together with their body buffer.
pub(crate) enum Task {
    SearchKey(SearchKey, Vec<u8>),
    SearchVector(SearchVector, Vec<u8>),
}

impl Task {
    /// Converts the request into an owned task if it is a search to offload.
    ///
    /// Searches traverse a lot; everything else is near constant time, so offloading would cost
    /// more than it saves.
    // `Request` grew large because of requests carrying body segment sizes.
    #[allow(clippy::result_large_err)]
    pub(crate) fn from_request(request: Request) -> std::result::Result<Self, Request> {
        match request {
            Request::Body(Body::SearchKey(spec), bytes) => Ok(Task::SearchKey(spec, bytes)),
            Request::Body(Body::SearchVector(spec), bytes) => Ok(Task::SearchVector(spec, bytes)),
            Request::Line(Line::SearchKey(spec)) => Ok(Task::SearchKey(spec, Vec::new())),
            other => Err(other),
        }
    }

    /// Only **reads** the engine.
    fn work(self, store: &Store) -> Result<Reply> {
        match self {
            Task::SearchKey(spec, body) => cmd::search_key(store, &spec, &body),
            Task::SearchVector(spec, body) => cmd::search_vector(store, &spec, &body),
        }
    }
}

/// One command's unit of work and the slot where its connection waits for the reply. Graph work
/// created by the callback without a connection also lives in one (`cookie` is 0).
///
/// A command's node is created **before touching the engine** ([`begin`]). If that fails, the
/// command replies `SERVER_ERROR` right away. After the write the node is only filled and
/// enqueued, so nothing can fail -- failing to get a slot after the write would leave no way to
/// reply about data already stored.
///
/// A node is linked in two places at once: the lane list (`entry`) and the `cb_data` of
/// `on_io_complete`. Each field has a fixed owner:
///
/// ```text
/// state          [worker] set by the command; for a search, [lane] replaces it with the result
/// graph          set by [callback], taken by [lane]
/// graph_failed   written by [lane]
/// ```
///
/// `wake` runs only once all IO the connection waits for has completed, and the lane's notify is
/// one of them, so `wake` cannot arrive before the lane finishes. The lane does not touch the
/// node after notifying.
#[repr(C)]
pub(crate) struct Node {
    /// Lane list link. As the first field, its address is the node's address.
    entry: Entry,
    /// Connection to reply to. 0 means connectionless graph work; the lane frees the node.
    cookie: usize,
    state: State,
    /// Graph work enqueued by the callback.
    graph: Option<Graph>,
    /// Graph work failed: the element is in the store but not in the graph.
    graph_failed: Option<Error>,
}

enum State {
    /// Nothing yet, or a lane has taken the search and is running it. If `wake` sees this, the
    /// search panicked.
    Empty,
    /// Search for a lane to run.
    Task(Task),
    /// The reply is decided.
    Ready(Result<Reply>),
}

impl Node {
    /// Allocates an empty node, or fails with `SERVER_ERROR`. A null `cookie` makes a
    /// connectionless node.
    fn new(cookie: *const c_void) -> Result<*mut Node> {
        crate::fallible::boxed(Node {
            entry: Entry::new(run_node),
            cookie: cookie as usize,
            state: State::Empty,
            graph: None,
            graph_failed: None,
        })
        .map(Box::into_raw)
    }
}

/// Enqueues graph work on the lane for `hash` (the vkey hash). **Called from the callback, under
/// the cache lock.**
///
/// If `node` is given, it was offered by a write command on this thread, and the lane wakes that
/// connection when done. Otherwise a connectionless node is created. Returns `Err` if no node
/// could be allocated or no lane accepted it; the work (and the index it holds) is then dropped.
///
/// # Safety
///
/// `node` must have been created by [`begin`] and offered via `event::offer`, and not yet
/// enqueued on a lane.
pub(crate) unsafe fn enqueue(
    node: Option<*mut Node>,
    graph: Graph,
    hash: u64,
) -> std::result::Result<(), ()> {
    let node = match node {
        Some(node) => node,
        None => Node::new(ptr::null()).map_err(|_| ())?,
    };
    unsafe { (*node).graph = Some(graph) };
    if unsafe { pool::submit_keyed(hash, &raw const (*node).entry) } {
        return Ok(());
    }
    unsafe {
        (*node).graph = None;
        if (*node).cookie == 0 {
            drop(Box::from_raw(node));
        }
    }
    Err(())
}

/// Runs an entry taken by a lane thread.
///
/// # Safety
///
/// `entry` must be the link of a node created by [`Node::new`].
unsafe fn run_node(entry: *const Entry) {
    let node = entry.cast_mut().cast::<Node>();

    if let Some(graph) = unsafe { (*node).graph.take() } {
        // Wake the connection even on panic; otherwise it waits for a reply that never comes.
        let failed = match std::panic::catch_unwind(AssertUnwindSafe(|| apply::apply(graph))) {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(Error::Index("the graph work panicked".into())),
        };
        let cookie = unsafe { (*node).cookie };
        if cookie == 0 {
            // No connection to reply to; failures are only logged.
            if let Some(e) = failed {
                eprintln!(
                    "ArcVector: a replicated or replayed vector is stored but not indexed: {e}"
                );
            }
            drop(unsafe { Box::from_raw(node) });
            return;
        }
        unsafe { (*node).graph_failed = failed };
        unsafe { server::notify_io_complete(cookie as *const c_void, 0) };
        return;
    }

    // Search. Take the task out first; on panic the node stays `Empty` and `wake` replies with
    // an error.
    let cookie = unsafe { (*node).cookie };
    if let State::Task(task) = std::mem::replace(unsafe { &mut (*node).state }, State::Empty) {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // Read-only, so the background cookie suffices. Not passing the connection's cookie
            // to the engine avoids entangling it with replication's thread assumptions.
            match Store::background() {
                Some(store) => task.work(&store),
                None => Err(Error::Store(StoreError::Unavailable)),
            }
        }));
        if let Ok(outcome) = outcome {
            unsafe { (*node).state = State::Ready(outcome) };
        }
    }
    unsafe { server::notify_io_complete(cookie as *const c_void, 0) };
}

/// Stores the decided reply in the node and registers `wake` to write it.
///
/// If the core refuses the callback, reclaims the node and replies here directly.
unsafe fn answer(node: *mut Node, outcome: Result<Reply>, handler: ResponseHandler) {
    let cookie = unsafe { (*node).cookie } as *const c_void;
    unsafe { (*node).state = State::Ready(outcome) };
    if unsafe { server::on_io_complete(cookie, wake, node.cast::<c_void>()) } {
        return;
    }
    let node = unsafe { Box::from_raw(node) };
    if let State::Ready(outcome) = node.state {
        Responder::new(handler, cookie).reply(outcome);
    }
}

/// Writes an already decided reply through the callback.
///
/// If no node can be allocated, replies via `handler` directly, which is fine since `execute`
/// has not returned yet.
pub(crate) unsafe fn reply(
    cookie: *const c_void,
    outcome: Result<Reply>,
    handler: ResponseHandler,
) {
    match Node::new(cookie) {
        Ok(node) => unsafe { answer(node, outcome, handler) },
        Err(_) => {
            Responder::new(handler, cookie).reply(outcome);
        }
    }
}

/// Makes the connection wait and registers `wake`. On failure, withdraws the wait and returns
/// `false`.
unsafe fn wait(node: *mut Node) -> bool {
    let cookie = unsafe { (*node).cookie } as *const c_void;
    // **Before enqueueing on a lane**, so the core counts the notify even if the lane finishes
    // first.
    unsafe { server::waitfor_io_complete(cookie) };
    if unsafe { server::on_io_complete(cookie, wake, node.cast::<c_void>()) } {
        return true;
    }
    unsafe { server::notify_io_complete(cookie, 0) };
    false
}

/// Hands a search to a lane and registers the callback that receives the reply.
///
/// If no lane accepts it, runs it to completion here (on the worker) and leaves the reply for
/// the same callback -- failing to offload is no reason to fail the command.
unsafe fn submit_search(node: *mut Node, task: Task, handler: ResponseHandler) {
    let cookie = unsafe { (*node).cookie } as *const c_void;
    if !unsafe { wait(node) } {
        // Could not register the callback: run here and reply immediately.
        drop(unsafe { Box::from_raw(node) });
        let outcome = unsafe { run_here(task, cookie) };
        Responder::new(handler, cookie).reply(outcome);
        return;
    }

    unsafe { (*node).state = State::Task(task) };
    if unsafe { pool::submit(&raw const (*node).entry) } {
        return;
    }

    // No lane accepted it (no thread could be started). Withdraw the wait and run here.
    unsafe { server::notify_io_complete(cookie, 0) };
    let outcome = match std::mem::replace(unsafe { &mut (*node).state }, State::Empty) {
        State::Task(task) => unsafe { run_here(task, cookie) },
        _ => Err(Error::Store(StoreError::Unavailable)),
    };
    // The callback runs only after `execute` returns; until then the node belongs to this thread.
    unsafe { (*node).state = State::Ready(outcome) };
}

/// Runs one write command: offers the node, then performs `write` (the engine write).
///
/// If the write changed an element, the callback enqueued the node on that vkey's lane, and the
/// lane wakes this connection after updating the graph. If no element changed (write failed,
/// unknown vkey), the node is still here and the wait is withdrawn here.
unsafe fn write_command(
    node: *mut Node,
    handler: ResponseHandler,
    write: impl FnOnce(&Store) -> Result<Reply>,
) {
    let cookie = unsafe { (*node).cookie } as *const c_void;
    let store = unsafe { Store::for_cookie(cookie) };

    if !unsafe { wait(node) } {
        // Could not register the callback. Write without offering the node and reply
        // immediately -- the callback enqueues the graph work separately, connectionless.
        drop(unsafe { Box::from_raw(node) });
        let outcome = match &store {
            Some(store) => write(store),
            None => Err(Error::Store(StoreError::Unavailable)),
        };
        Responder::new(handler, cookie).reply(outcome);
        return;
    }

    /// Withdraws the offered node from this thread even if the write panics. Left in place, an
    /// unrelated event, rather than this thread's next write, could claim it.
    struct Offered;
    impl Drop for Offered {
        fn drop(&mut self) {
            event::withdraw();
        }
    }

    event::offer(node);
    let offered = Offered;
    let outcome = match &store {
        Some(store) => write(store),
        None => Err(Error::Store(StoreError::Unavailable)),
    };
    let unclaimed = event::withdraw();
    std::mem::forget(offered);

    // If the callback claimed the node it is also on a lane, but the lane does not touch `state`
    // and `wake` runs only after this function returns.
    unsafe { (*node).state = State::Ready(outcome) };
    if unclaimed.is_some() {
        // Nothing was enqueued on a lane; withdraw the wait.
        unsafe { server::notify_io_complete(cookie, 0) };
    }
}

/// Entry point where `execute` hands over one command. The reply is always written by [`wake`].
///
/// **Runs on the worker thread** -- all engine writes (create, upsert, setmeta, delete, drop)
/// must happen here. Since the callback writes the reply, it goes out after any replication ack
/// the write waits for.
pub(crate) unsafe fn begin(cookie: *const c_void, request: Request, handler: ResponseHandler) {
    // Allocate the node **before** touching the engine: failing after the write would leave no
    // way to reply about stored data. Failing here replies `SERVER_ERROR` with nothing written.
    let node = match Node::new(cookie) {
        Ok(node) => node,
        Err(e) => {
            Responder::new(handler, cookie).reply(Err(e));
            return;
        }
    };

    match &request {
        Request::Body(Body::Upsert(spec), bytes) => {
            return unsafe {
                write_command(node, handler, |store| {
                    spec.segments
                        .split(bytes)
                        .and_then(|parts| cmd::upsert_write(store, spec, parts))
                })
            };
        }
        Request::Body(Body::SetMeta(spec), bytes) => {
            return unsafe {
                write_command(node, handler, |store| {
                    spec.segments
                        .split(bytes)
                        .and_then(|parts| cmd::setmeta_write(store, spec, parts))
                })
            };
        }
        Request::Line(Line::Delete(spec)) => {
            return unsafe { write_command(node, handler, |store| cmd::delete(store, spec)) };
        }
        _ => {}
    }

    match Task::from_request(request) {
        // Light commands run in place; offloading costs more than the work.
        Err(request) => unsafe { answer(node, cmd::run(cookie, request), handler) },
        Ok(task) => unsafe { submit_search(node, task, handler) },
    }
}

/// **Runs in `conn_io_callback`, on the worker thread**, after all IO the connection waited for
/// has been notified.
///
/// Reclaims the node registered by `execute`, builds the reply via `response_handler`, and frees
/// the node. Does no engine writes. Failures are sent as replies too, so this normally returns
/// `true`; it returns `false` (closing the connection) only if no reply buffer could be obtained.
unsafe extern "C" fn wake(
    cookie: *const c_void,
    data: *mut c_void,
    handler: ResponseHandler,
) -> bool {
    if data.is_null() {
        return true;
    }
    let node = unsafe { Box::from_raw(data.cast::<Node>()) };

    let outcome = match node.graph_failed {
        // Stored in the engine but not in the graph. Do not write the engine again.
        Some(e) => {
            eprintln!("ArcVector: a write is stored but not indexed: {e}");
            Err(e)
        }
        None => match node.state {
            State::Ready(outcome) => outcome,
            // The lane took the search but never stored a result (panic).
            State::Empty | State::Task(_) => Err(Error::Store(StoreError::Unavailable)),
        },
    };

    // If the reply cannot be written, close the connection; otherwise the client would wait for
    // a reply that never comes.
    Responder::new(handler, cookie).reply(outcome)
}

/// Runs a search to completion on this thread (`execute`) when it could not be offloaded.
pub(crate) unsafe fn run_here(task: Task, cookie: *const c_void) -> Result<Reply> {
    let store =
        unsafe { Store::for_cookie(cookie) }.ok_or(Error::Store(StoreError::Unavailable))?;
    task.work(&store)
}
