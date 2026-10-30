//! Network front end. Like the Java emulator, one port serves HTTP/1.1 (REST + control endpoints)
//! and HTTP/2 cleartext (gRPC). Requests are dispatched on `content-type: application/grpc*`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tonic::body::Body;
use tonic::service::Routes;
use tower::ServiceExt;

use oxide_api::config::StoredJobPolicy;
use oxide_api::persist::{self, Loaded};
use oxide_api::{DatastoreService, RestGateway};
use oxide_engine::Engine;
use oxide_engine::index_files::{self, IndexConfig, IndexFormat, IndexManager};

use crate::cli::{AutoIdPolicy, StartConfig};

/// The composite-index manager for `cfg`, as the Java start action configures it. A missing
/// `--index_file` is created with an empty auto-generated section.
fn index_manager(cfg: &StartConfig) -> Result<IndexManager, String> {
    let app_dir = cfg.directory.clone().unwrap_or_else(|| ".".into());
    if let Some(file) = &cfg.index_file
        && !file.exists()
    {
        let parent_ok = file
            .parent()
            .is_some_and(|p| p.as_os_str().is_empty() || p.is_dir());
        let created = parent_ok && std::fs::write(file, crate::create::INITIAL_INDEX_YAML).is_ok();
        if !created {
            let absolute = std::path::absolute(file).unwrap_or_else(|_| file.clone());
            return Err(format!("{} is not a valid index yaml.", absolute.display()));
        }
    }
    let yaml_by_default = (cfg.store_on_disk()
        || (cfg.store_index_configuration_on_disk() && cfg.index_file.is_some()))
        && index_files::prefers_yaml(&app_dir);
    let yaml_file = cfg
        .index_file
        .as_ref()
        .and_then(|f| f.file_name())
        .is_some_and(|n| n.to_string_lossy().ends_with(".yaml"));
    Ok(IndexManager::new(IndexConfig {
        format: if yaml_by_default || yaml_file {
            IndexFormat::Yaml
        } else {
            IndexFormat::Xml
        },
        app_dir,
        index_file: cfg.index_file.clone(),
        regenerate: cfg.regenerate_indexes,
        store: cfg.store_index_configuration_on_disk(),
        require: cfg.require_indexes,
    }))
}

/// How often unsaved writes are saved (Java's `storeDelayMs`; `OXIDE_STORE_DELAY_MS` overrides it
/// for tests).
const STORE_DELAY: Duration = Duration::from_secs(30);

/// How long in-flight connections get to finish after a shutdown request.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// State shared by every connection.
struct Emulator {
    /// The backing store when storing on disk.
    store: Option<PathBuf>,
    shutdown: watch::Sender<bool>,
    engine: Arc<Engine>,
    grpc: Routes,
    rest: RestGateway,
    /// Serve the superset's extra HTTP pages (the data viewer).
    superset: bool,
}

impl Emulator {
    fn reset(&self) -> Result<(), &'static str> {
        if self.store.is_some() {
            tracing::warn!(
                "Cannot reset a Datastore Emulator when it is storing data on disk.\n\
                 Re-run the emulator with the --no-store-on-disk flag to start the emulator in-memory."
            );
            return Err("reset failed");
        }
        self.engine.reset();
        Ok(())
    }

    fn persist(&self) -> Result<(), &'static str> {
        if self.store.is_none() {
            return Err("The local datastore does not store on disk, persist is not supported.");
        }
        self.save().map_err(|_| "persist failed")
    }

    /// Saves the store if anything was written since the last save (Java's `persist`).
    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.store else {
            return Ok(());
        };
        let Some(snapshot) = self.engine.take_snapshot() else {
            return Ok(());
        };
        let started = std::time::Instant::now();
        if let Err(e) = persist::save(path, &snapshot) {
            self.engine.mark_dirty();
            tracing::error!("Unable to save the datastore: {e}");
            return Err(e);
        }
        tracing::info!(
            "Time to persist datastore: {} ms",
            started.elapsed().as_millis()
        );
        Ok(())
    }

    /// Reads the backing store at startup (Java's `load`).
    fn load(&self) {
        let Some(path) = &self.store else {
            return;
        };
        let started = std::time::Instant::now();
        match persist::load(path) {
            Ok(Loaded::Missing) => {
                tracing::info!(
                    "The backing store, {}, does not exist. It will be created.",
                    path.display()
                );
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let java_store = path.with_file_name("local_db.bin");
                if java_store.exists() {
                    tracing::warn!(
                        "{} was written by the Java emulator and cannot be read; move its data with \
                         export/import.",
                        java_store.display()
                    );
                }
            }
            Ok(Loaded::Snapshot(snapshot)) => {
                self.engine.restore(snapshot);
                tracing::info!(
                    "Time to load datastore: {} ms",
                    started.elapsed().as_millis()
                );
            }
            Ok(Loaded::Unreadable { error, moved_to }) => tracing::warn!(
                "Failed to load from the backing store, {} ({error}); moved it to {} and starting \
                 empty.",
                path.display(),
                moved_to.display()
            ),
            Err(e) => tracing::warn!(
                "Failed to load from the backing store, {}: {e}",
                path.display()
            ),
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}

/// Runs the `start` action until shut down. Returns the process exit code.
pub async fn run(cfg: StartConfig) -> Result<i32> {
    // Java checks the storage file before the index file.
    if let Some(file) = &cfg.storage_file
        && !file.exists()
        && !file
            .parent()
            .is_some_and(|p| p.as_os_str().is_empty() || p.is_dir())
    {
        let absolute = std::path::absolute(file).unwrap_or_else(|_| file.clone());
        println!(
            "{} is not a valid storage file.\nExiting.",
            absolute.display()
        );
        return Ok(1);
    }
    let indexes = match index_manager(&cfg) {
        Ok(indexes) => indexes,
        Err(message) => {
            println!("{message}\nExiting.");
            return Ok(1);
        }
    };
    if cfg.allow_remote_shutdown {
        tracing::info!(
            "Provided --allow_remote_shutdown to start command which is no longer necessary."
        );
    }
    // Java validates the port when binding and exits with status 0.
    let Ok(port) = u16::try_from(cfg.port) else {
        println!(
            "Exiting due to exception: java.lang.IllegalArgumentException: port out of range:{}",
            cfg.port
        );
        return Ok(0);
    };
    let (product, store_type) = if cfg.firestore_in_datastore_mode {
        ("CLOUD_FIRESTORE_IN_DATASTORE_MODE", "VNext")
    } else {
        ("CLOUD_DATASTORE", "High Replication")
    };
    tracing::info!("Running {product} emulator.");
    tracing::info!(
        "Local Datastore initialized: Type: {store_type}, Storage: {}, Consistency: {}, \
         Index configuration on disk: {}",
        cfg.storage_path()
            .map_or("In-memory".into(), |p| p.display().to_string()),
        cfg.consistency(),
        cfg.store_index_configuration_on_disk(),
    );

    let listeners = match bind(&cfg.host, port).await {
        Ok(l) => l,
        Err(e) => {
            println!("Exiting due to exception: {e:#}");
            return Ok(1);
        }
    };
    let port = listeners[0].local_addr()?.port();

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let firestore = cfg.firestore_in_datastore_mode;
    let stored_policy = StoredJobPolicy {
        // Like Java, the default seed is the startup time in milliseconds.
        random_seed: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
        unapplied_percentage: (1.0 - cfg.stored_consistency()) * 100.0,
        forced: firestore,
    };
    let mut engine = Engine::new(oxide_engine::Config {
        auto_id_policy: match cfg.auto_id_policy {
            AutoIdPolicy::Sequential => oxide_engine::AutoIdPolicy::Sequential,
            AutoIdPolicy::Scattered => oxide_engine::AutoIdPolicy::Scattered,
        },
        // Firestore-in-Datastore mode starts strongly consistent whatever --consistency says.
        job_policy: if firestore {
            oxide_engine::JobPolicy::ForceStrongConsistency
        } else {
            oxide_engine::JobPolicy::Probability {
                random_seed: stored_policy.random_seed,
                unapplied_percentage: stored_policy.unapplied_percentage,
            }
        },
    })
    .with_indexes(indexes);
    if cfg.firestore_in_datastore_mode {
        engine = engine.with_firestore_mode();
    }
    if cfg.superset {
        engine = engine.with_point_in_time();
    }
    if cfg.enable_entity_groups == Some(false) {
        engine = engine.without_entity_groups();
    }
    let engine = Arc::new(engine);
    let emulator = Arc::new(Emulator {
        store: cfg.storage_path(),
        shutdown: shutdown_tx,
        grpc: grpc_routes(&engine, stored_policy, firestore, cfg.superset)?,
        rest: RestGateway::new(cfg.superset)?,
        superset: cfg.superset,
        engine,
    });

    emulator.load();
    if emulator.store.is_some() {
        // Like Java, unsaved writes are saved every 30 seconds.
        let emulator = emulator.clone();
        tokio::spawn(async move {
            let delay = std::env::var("OXIDE_STORE_DELAY_MS")
                .ok()
                .and_then(|ms| ms.parse().ok())
                .map_or(STORE_DELAY, Duration::from_millis);
            let mut ticks = tokio::time::interval(delay);
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let _ = emulator.save();
            }
        });
    }

    print_banner(&cfg.host, port);

    // Each listener feeds accepted sockets to the loop below, which owns the shutdown tracker.
    let (conn_tx, mut conn_rx) = tokio::sync::mpsc::channel(64);
    let accept_tasks: Vec<_> = listeners
        .into_iter()
        .map(|listener| {
            let conn_tx = conn_tx.clone();
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            // Responses go out in several writes (headers, data, trailers);
                            // with Nagle's algorithm on they stall on delayed ACKs (~40 ms).
                            let _ = stream.set_nodelay(true);
                            if conn_tx.send(stream).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("accept failed: {e}"),
                    }
                }
            })
        })
        .collect();

    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http1().auto_date_header(false);
    builder.http2().auto_date_header(false);
    let graceful = GracefulShutdown::new();
    let signal = shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            Some(stream) = conn_rx.recv() => {
                let emulator = emulator.clone();
                let service = service_fn(move |req| dispatch(req, emulator.clone()));
                let conn = builder.serve_connection(TokioIo::new(stream), service).into_owned();
                let conn = graceful.watch(conn);
                tokio::spawn(async move {
                    if let Err(e) = conn.await {
                        tracing::debug!("connection error: {e}");
                    }
                });
            }
            _ = shutdown_rx.wait_for(|stop| *stop) => break,
            _ = &mut signal => break,
        }
    }
    for task in &accept_tasks {
        task.abort();
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::debug!("timed out waiting for connections to close");
    }
    if emulator.store.is_some() {
        // Unapplied jobs are not saved, so Java applies them all before the final save.
        emulator.engine.roll_forward_all();
        if emulator.save().is_err() {
            tracing::error!("Failed to persist datastore");
        }
    }
    Ok(0)
}

/// Binds every address `host` resolves to (e.g. both 127.0.0.1 and ::1 for localhost), all on the
/// same port. Only the first address is required to bind.
/// A listener on `[::]:port` that also accepts IPv4 connections.
fn dual_stack(port: u16) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
    socket.set_only_v6(false)?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

async fn bind(host: &str, port: u16) -> Result<Vec<TcpListener>> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((bare, port))
        .await
        .with_context(|| format!("resolving {host}"))?
        .collect();
    addrs.dedup();
    let (first, rest) = addrs
        .split_first()
        .with_context(|| format!("{host} did not resolve"))?;

    // Like Java, the IPv4 wildcard listens dual-stack (`[::]` accepting IPv4 too) where IPv6
    // exists; some port forwarders (rootless podman's) try IPv6 first.
    let primary = match first.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => dual_stack(first.port())
            .or_else(|_| std::net::TcpListener::bind(first))
            .and_then(|l| {
                l.set_nonblocking(true)?;
                TcpListener::from_std(l)
            }),
        _ => TcpListener::bind(first).await,
    }
    .with_context(|| format!("binding {first}"))?;
    let port = primary.local_addr()?.port();
    let mut listeners = vec![primary];
    for addr in rest {
        let addr = SocketAddr::new(addr.ip(), port);
        match TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) => tracing::debug!("not listening on {addr}: {e}"),
        }
    }
    Ok(listeners)
}

/// The startup text the Java emulator prints. gcloud and test harnesses parse these lines.
fn print_banner(host: &str, port: u16) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = write!(
        out,
        "API endpoint: http://{host}:{port}\n\
         If you are using a library that supports the DATASTORE_EMULATOR_HOST environment variable, run:\n\
         \n\
         \x20 export DATASTORE_EMULATOR_HOST={host}:{port}\n\
         \n\
         Dev App Server is now running.\n\
         \n\
         The previous line was printed for backwards compatibility only.\n\
         If your tests rely on it to confirm emulator startup,\n\
         please migrate to the emulator health check endpoint (/). Thank you!\n\
         The health check endpoint for this emulator instance is http://{host}:{port}/\n"
    );
    let _ = out.flush();
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn grpc_routes(
    engine: &Arc<Engine>,
    stored_policy: StoredJobPolicy,
    firestore: bool,
    superset: bool,
) -> Result<Routes> {
    // The superset keeps the Admin API's operations for the Operations service.
    let operations = superset.then(oxide_api::operations::OperationStore::default);
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(oxide_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let reflection_alpha = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(oxide_proto::FILE_DESCRIPTOR_SET)
        .build_v1alpha()?;
    let routes = Routes::new(reflection)
        .add_service(reflection_alpha)
        .add_service(
            DatastoreService::new(engine.clone())
                .with_superset(superset)
                .into_server(),
        )
        .add_service(
            oxide_api::config::ConfigService::new(engine.clone(), stored_policy, firestore)
                .into_server(),
        )
        .add_service({
            let admin = oxide_api::admin::AdminService::new(engine.clone());
            match &operations {
                Some(store) => admin.with_operations(store.clone()),
                None => admin,
            }
            .into_server()
        })
        .add_service(
            oxide_api::appengine::Service::new(engine.clone())
                .with_superset(superset)
                .with_query_lifetime(
                    std::env::var("OXIDE_QUERY_LIFETIME_MS")
                        .ok()
                        .and_then(|ms| ms.parse().ok())
                        .map_or(oxide_api::appengine::QUERY_LIFETIME, Duration::from_millis),
                )
                .into_server(),
        );
    let routes = match operations {
        Some(store) => {
            routes.add_service(oxide_api::operations::OperationsService::new(store).into_server())
        }
        None => routes,
    };
    Ok(routes.prepare())
}

async fn dispatch(
    req: Request<Incoming>,
    emulator: Arc<Emulator>,
) -> Result<Response<Body>, Infallible> {
    if is_grpc(&req) {
        return emulator.grpc.clone().oneshot(req.map(Body::new)).await;
    }
    let cors = cors_headers(&req);
    let mut resp = match handle_control(&req, &emulator) {
        // Java's failing control endpoints (reset or persist refused) skip the CORS headers.
        Some(resp) if resp.status() == StatusCode::INTERNAL_SERVER_ERROR => return Ok(resp),
        Some(resp) => resp,
        None => emulator.rest.handle(req, emulator.grpc.clone()).await,
    };
    resp.headers_mut().extend(cors);
    Ok(resp)
}

fn is_grpc<B>(req: &Request<B>) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

/// Control endpoints, with the exact status codes and bodies of the Java emulator. Anything else
/// goes to the REST gateway (which answers 404 for unknown paths).
fn handle_control<B>(req: &Request<B>, emulator: &Emulator) -> Option<Response<Body>> {
    let method = req.method();
    if emulator.superset && req.uri().path().starts_with(crate::viewer::PATH) {
        let viewer = crate::viewer::handle(
            method,
            req.uri().path(),
            req.uri().query().unwrap_or(""),
            &emulator.engine,
        );
        if viewer.is_some() {
            return viewer;
        }
    }
    Some(match (method, req.uri().path()) {
        (&Method::OPTIONS, _) => text(StatusCode::OK, ""),
        (&Method::GET, "/") => text(StatusCode::OK, "Ok\n"),
        (&Method::POST, "/reset") => match emulator.reset() {
            Ok(()) => text(StatusCode::OK, "Resetting...\n"),
            Err(msg) => text(StatusCode::INTERNAL_SERVER_ERROR, msg),
        },
        (&Method::POST, "/persist") => match emulator.persist() {
            Ok(()) => text(StatusCode::OK, "Persisting...\n"),
            Err(msg) => text(StatusCode::INTERNAL_SERVER_ERROR, msg),
        },
        (&Method::POST, "/shutdown" | "/_ah/admin/quit") => {
            emulator.request_shutdown();
            text(StatusCode::OK, "Shutting down...\n")
        }
        _ => return None,
    })
}

/// CORS headers Java adds to every HTTP (not gRPC) response of a request with an `Origin`: the
/// origin echoed, fixed methods, credentials, and the requested headers if any.
fn cors_headers<B>(req: &Request<B>) -> HeaderMap {
    let mut h = HeaderMap::new();
    let Some(origin) = req.headers().get(header::ORIGIN) else {
        return h;
    };
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("DELETE,GET,HEAD,PATCH,POST,PUT"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    if let Some(requested) = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    h
}

fn text(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut resp = Response::new(Body::new(Full::new(Bytes::from_static(body.as_bytes()))));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    resp
}
