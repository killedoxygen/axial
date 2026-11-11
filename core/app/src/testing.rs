//! Shared test composition: production owners over an isolated root, and a
//! scripted loopback HTTP server. No test doubles of application owners.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    instances::{
        create::InstanceService,
        directory::{InstanceDirectories, Registry},
        model::Instance,
    },
    library::{LibraryId, LibraryLifecycle, LibraryOpenOutcome},
    storage::{MetadataStore, Migration, SCHEMA, StorageLimits},
    tasks::{Exclusions, TaskOwner},
};

/// A library, metadata store, instance directories and task owner over one
/// temporary root created under the canonicalized temporary parent.
pub(crate) struct Fixture {
    pub root: tempfile::TempDir,
    pub library: LibraryLifecycle,
    pub storage: Arc<MetadataStore>,
    pub directories: InstanceDirectories,
    pub tasks: TaskOwner,
}

pub(crate) struct Builder {
    tasks: usize,
    schema: &'static [Migration],
    memory: bool,
    limits: StorageLimits,
    prefix: &'static str,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::builder().build()
    }

    pub(crate) fn builder() -> Builder {
        Builder {
            tasks: 16,
            schema: SCHEMA,
            memory: false,
            limits: StorageLimits::default(),
            prefix: ".tmp",
        }
    }

    pub(crate) fn instances(&self) -> InstanceService {
        InstanceService::new(self.directories.clone(), self.tasks.clone())
    }

    pub(crate) async fn create_instance(&self, name: &str) -> Instance {
        crate::instances::create::tests::create(&self.instances(), name).await
    }
}

impl Builder {
    /// Whole-budget fixtures choose their own capacity so unrelated waiters
    /// cannot consume it.
    pub(crate) fn tasks(mut self, capacity: usize) -> Self {
        self.tasks = capacity;
        self
    }

    /// Only for owners that must observe an absent or partial schema.
    pub(crate) fn schema(mut self, migrations: &'static [Migration]) -> Self {
        self.schema = migrations;
        self
    }

    /// Keeps metadata in memory instead of `metadata.sqlite` under the root.
    pub(crate) fn in_memory(mut self) -> Self {
        self.memory = true;
        self
    }

    pub(crate) fn storage_limits(mut self, limits: StorageLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Names the root, e.g. to put spaces or non-ASCII text in every path.
    pub(crate) fn prefix(mut self, prefix: &'static str) -> Self {
        self.prefix = prefix;
        self
    }

    pub(crate) fn build(self) -> Fixture {
        let root = tempfile::Builder::new()
            .prefix(self.prefix)
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let (library, storage, directories, tasks) = self.open(root.path(), LibraryId::new());
        Fixture {
            root,
            library,
            storage,
            directories,
            tasks,
        }
    }

    /// Opens the owners over an existing root, as a restarted application does.
    /// The previous owners must be dropped first.
    pub(crate) fn reopen(self, fixture: Fixture) -> Fixture {
        let Fixture {
            root,
            library,
            storage,
            directories,
            tasks,
        } = fixture;
        let id = library.admit().unwrap().library_id();
        drop((library, storage, directories, tasks));
        let (library, storage, directories, tasks) = self.open(root.path(), id);
        Fixture {
            root,
            library,
            storage,
            directories,
            tasks,
        }
    }

    fn open(
        self,
        root: &Path,
        id: LibraryId,
    ) -> (
        LibraryLifecycle,
        Arc<MetadataStore>,
        InstanceDirectories,
        TaskOwner,
    ) {
        let library = match LibraryLifecycle::open_with_id(root, id) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated test library did not open: {other:?}"),
        };
        let storage = if self.memory {
            MetadataStore::in_memory_with_limits(self.limits)
        } else {
            MetadataStore::open_with_limits(root.join("metadata.sqlite"), self.limits)
        };
        let storage = Arc::new(storage.unwrap());
        storage.migrate(self.schema).unwrap();
        let directories = InstanceDirectories::new(
            Registry::new(storage.clone()),
            library.clone(),
            Exclusions::new(),
        );
        (library, storage, directories, TaskOwner::new(self.tasks).unwrap())
    }
}

const MAX_REQUEST: usize = 2 * 1024 * 1024;
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
const HOLD_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Reply {
    status: u16,
    headers: String,
    body: Vec<u8>,
    hold: bool,
    may_disconnect: bool,
}

impl Reply {
    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self::status(200, body)
    }

    pub(crate) fn json(value: serde_json::Value) -> Self {
        Self::ok(value.to_string()).header("Content-Type", "application/json")
    }

    pub(crate) fn status(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: String::new(),
            body: body.into(),
            hold: false,
            may_disconnect: false,
        }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push_str(&format!("{name}: {value}\r\n"));
        self
    }

    /// Sends the head, reports the request, then waits for `Server::release`
    /// before writing the body.
    pub(crate) fn held(mut self) -> Self {
        self.hold = true;
        self
    }

    /// Tolerates a client that departs before the body is written.
    pub(crate) fn may_disconnect(mut self) -> Self {
        self.may_disconnect = true;
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub line: String,
    pub head: String,
    pub body: Vec<u8>,
}

impl Request {
    pub(crate) fn path(&self) -> &str {
        self.line.split(' ').nth(1).unwrap_or_default()
    }
}

/// Answers one connection per scripted reply, in order, on its own thread so
/// paused or blocked test runtimes cannot stall it. Every request is captured.
pub(crate) struct Server {
    pub origin: String,
    requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
    release: mpsc::Sender<()>,
    thread: JoinHandle<Served>,
}

pub(crate) struct Served {
    pub requests: Vec<Request>,
    /// False when a `may_disconnect` client departed before its body.
    pub delivered: bool,
}

impl Server {
    pub(crate) fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (observe, requests) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut served = Served {
                requests: Vec::new(),
                delivered: true,
            };
            for reply in replies {
                let mut stream = accept(&listener);
                let request = read_request(&mut stream);
                served.requests.push(request.clone());
                served.delivered &= respond(&mut stream, reply, &released, || {
                    let _ = observe.send(request);
                });
            }
            served
        });
        Self {
            origin,
            requests,
            release,
            thread,
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    pub(crate) fn origins(&self) -> crate::network::OriginPolicy {
        crate::network::OriginPolicy::loopback_for_tests([&self.origin], 0).unwrap()
    }

    /// The next request, observed after its reply head was written. The
    /// server's real-time accept bound applies even under paused test time.
    pub(crate) async fn request(&mut self) -> Request {
        self.requests.recv().await.expect("scripted server stopped")
    }

    /// `request` for a thread outside any async runtime.
    pub(crate) fn blocking_request(&mut self) -> Request {
        self.requests
            .blocking_recv()
            .expect("scripted server stopped")
    }

    pub(crate) fn release(&self) {
        self.release.send(()).unwrap();
    }

    /// Joins the server once every scripted reply was served. Call it after the
    /// client finished: it blocks the calling thread.
    pub(crate) fn finish(self) -> Served {
        self.thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(ACCEPT_TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(ACCEPT_TIMEOUT)).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "scripted request was not received");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("scripted accept failed: {error}"),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "request ended before it was complete");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() <= MAX_REQUEST, "request exceeds its bound");
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            return Request {
                line: head.lines().next().unwrap_or_default().to_owned(),
                body: bytes[end + 4..end + 4 + length].to_vec(),
                head,
            };
        }
    }
}

fn respond(
    stream: &mut TcpStream,
    reply: Reply,
    released: &mpsc::Receiver<()>,
    observed: impl FnOnce(),
) -> bool {
    let head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        reply.status,
        reply.body.len(),
        reply.headers
    );
    let written = stream.write_all(head.as_bytes()).and_then(|()| {
        observed();
        if reply.hold {
            released
                .recv_timeout(HOLD_TIMEOUT)
                .expect("held body was not released");
        }
        stream.write_all(&reply.body)
    });
    match written {
        Ok(()) => true,
        Err(error)
            if reply.may_disconnect
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) =>
        {
            false
        }
        Err(error) => panic!("scripted reply failed: {error}"),
    }
}
