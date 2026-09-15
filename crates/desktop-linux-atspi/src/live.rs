//! Live AT-SPI walk over the session bus.
//!
//! This module talks to the real `org.a11y.atspi.Registry` and builds
//! [`AccessibleSnapshot`] trees from live objects. It never synthesizes
//! nodes: an unreachable bus is [`AtspiError::BackendUnavailable`], a
//! vanished object aborts its own subtree, and every walk is bounded by
//! [`MAX_TREE_DEPTH`] / [`MAX_TREE_NODES`].
//!
//! Element and window ids are `atspi://{bus}/{object-path}` URIs, so any
//! id handed back by the model resolves to exactly one bus object.

use super::snapshot::{
    is_window_role, AccessibleSnapshot, AtspiError, MAX_NAME_CHARS, MAX_TREE_DEPTH, MAX_TREE_NODES,
    MAX_VALUE_CHARS,
};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::zbus::names::BusName;
use atspi::{AccessibilityConnection, CoordType, Role, StateSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tool_desktop::Rect;

const ID_SCHEME: &str = "atspi://";
/// Longest text read from a `Text` object in one walk (bus-provided).
const MAX_TEXT_READ: i32 = 4096;
/// Longest action list kept per object.
const MAX_ACTIONS: usize = 16;
/// Availability discovery is optional. Keep the probe short so a missing
/// session bus never becomes a startup prerequisite.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Cached state of the optional AT-SPI service. `Unknown` and `Probing` are
/// deliberately unavailable to callers: semantic actions fail closed until
/// the host has a positive answer from the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Unknown,
    Probing,
    Available,
    Unavailable,
}

impl Availability {
    fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Probing => 1,
            Self::Available => 2,
            Self::Unavailable => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Probing,
            2 => Self::Available,
            3 => Self::Unavailable,
            _ => Self::Unknown,
        }
    }
}

/// Injectable probe seam. The real implementation is the only code that
/// knows how to construct a Tokio runtime, and it is run exactly once by the
/// service's dedicated probe worker. Tests can inject a counting, delayed, or
/// deterministic implementation without requiring a session bus.
pub trait AvailabilityProbe: Send + Sync {
    fn probe(&self) -> bool;
}

impl<F> AvailabilityProbe for F
where
    F: Fn() -> bool + Send + Sync,
{
    fn probe(&self) -> bool {
        self()
    }
}

struct RealAvailabilityProbe;

impl AvailabilityProbe for RealAvailabilityProbe {
    fn probe(&self) -> bool {
        let Some(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()
        else {
            return false;
        };
        runtime
            .block_on(async {
                tokio::time::timeout(PROBE_TIMEOUT, AccessibilityConnection::new())
                    .await
                    .ok()
                    .and_then(Result::ok)
            })
            .is_some()
    }
}

/// Bound for one registry connection setup. The availability probe has its
/// own (shorter) budget; live operations share this one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for one live registry operation (tree walk, element action). Full
/// desktop walks over D-Bus can take seconds on loaded systems, so this is
/// generous — its job is ruling out infinite hangs, not enforcing speed.
const LIVE_OP_TIMEOUT: Duration = Duration::from_secs(20);

/// One host-owned AT-SPI availability service. Construction starts one
/// bounded background probe; all subsequent `is_available()` calls are
/// atomic reads and never touch D-Bus, spawn a thread, or create a runtime.
///
/// The service also owns the single cached registry connection. AT-SPI
/// clients are expected to hold one persistent connection: creating a fresh
/// `AccessibilityConnection` per call churns D-Bus setup on every status
/// poll, and re-creation after drop has been observed to hang forever
/// (nested `block_on` inside the connection constructor's peer-listener
/// setup), which wedged the host UI thread. Every live operation therefore
/// runs through [`Self::run_live`] on the shared connection with a timeout.
#[derive(Clone)]
pub struct AtspiService {
    state: Arc<AtomicU8>,
    probe_started: Arc<AtomicBool>,
    probe: Arc<dyn AvailabilityProbe>,
    last_probe_ms: Arc<AtomicU64>,
    changes: tokio::sync::watch::Sender<Availability>,
    connection: Arc<tokio::sync::Mutex<Option<AccessibilityConnection>>>,
}

impl std::fmt::Debug for AtspiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtspiService")
            .field("availability", &self.availability())
            .field(
                "last_probe_ms",
                &self
                    .last_probe_duration()
                    .map(|duration| duration.as_millis()),
            )
            .finish()
    }
}

impl AtspiService {
    pub fn new() -> Self {
        let service = Self::with_probe(Arc::new(RealAvailabilityProbe));
        service.start_probe();
        service
    }

    /// Construct a service with an injectable probe. The caller must invoke
    /// [`Self::start_probe`] explicitly so tests can assert the initial
    /// `Unknown` state and deterministic probe count.
    pub fn with_probe(probe: Arc<dyn AvailabilityProbe>) -> Self {
        let (changes, _) = tokio::sync::watch::channel(Availability::Unknown);
        Self {
            state: Arc::new(AtomicU8::new(Availability::Unknown.as_u8())),
            probe_started: Arc::new(AtomicBool::new(false)),
            probe,
            last_probe_ms: Arc::new(AtomicU64::new(0)),
            changes,
            connection: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Shared registry connection, connecting lazily on first use.
    /// Concurrent callers serialize on the setup and share the result;
    /// failures are never cached, so the next call retries.
    pub async fn connection(&self) -> Result<AccessibilityConnection, AtspiError> {
        let mut slot = self.connection.lock().await;
        if let Some(conn) = slot.as_ref() {
            return Ok(conn.clone());
        }
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, AccessibilityConnection::new())
            .await
            .map_err(|_| {
                AtspiError::bus(
                    "registry connect",
                    "timed out setting up the a11y connection",
                )
            })?
            .map_err(|detail| {
                AtspiError::BackendUnavailable(format!("AT-SPI registry unreachable: {detail}"))
            })?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    /// Drop the cached connection so the next operation reconnects. Called
    /// automatically by [`Self::run_live`] on bus failures and timeouts; a
    /// stale cache after a registry restart therefore heals itself.
    pub async fn invalidate_connection(&self) {
        *self.connection.lock().await = None;
    }

    /// Run one live registry operation on the shared connection with a
    /// timeout. Every live operation must go through here: it is the only
    /// path that bounds D-Bus hangs and heals a stale connection.
    pub async fn run_live<F, Fut, R>(
        &self,
        operation: &'static str,
        run: F,
    ) -> Result<R, AtspiError>
    where
        F: FnOnce(AccessibilityConnection) -> Fut,
        Fut: std::future::Future<Output = Result<R, AtspiError>>,
    {
        let conn = self.connection().await?;
        match tokio::time::timeout(LIVE_OP_TIMEOUT, run(conn)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error @ AtspiError::Bus { .. })) => {
                self.invalidate_connection().await;
                Err(error)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.invalidate_connection().await;
                Err(AtspiError::bus(
                    operation,
                    "timed out waiting for the a11y registry",
                ))
            }
        }
    }

    /// Start the one-shot discovery task. Repeated calls are idempotent.
    pub fn start_probe(&self) {
        if self
            .probe_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.state
            .store(Availability::Probing.as_u8(), Ordering::Release);
        let state = Arc::clone(&self.state);
        let probe = Arc::clone(&self.probe);
        let last_probe_ms = Arc::clone(&self.last_probe_ms);
        let changes = self.changes.clone();
        let worker = std::thread::Builder::new()
            .name("utsuwa-atspi-probe".to_string())
            .spawn(move || {
                let started = Instant::now();
                let availability = if probe.probe() {
                    Availability::Available
                } else {
                    Availability::Unavailable
                };
                last_probe_ms.store(
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    Ordering::Release,
                );
                state.store(availability.as_u8(), Ordering::Release);
                let _ = changes.send(availability);
                tracing::debug!(
                    available = availability == Availability::Available,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "AT-SPI availability probe complete"
                );
            });
        if worker.is_err() {
            self.state
                .store(Availability::Unavailable.as_u8(), Ordering::Release);
            let _ = self.changes.send(Availability::Unavailable);
        }
    }

    pub fn availability(&self) -> Availability {
        Availability::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn is_available(&self) -> bool {
        self.availability() == Availability::Available
    }

    pub fn last_probe_duration(&self) -> Option<Duration> {
        let millis = self.last_probe_ms.load(Ordering::Acquire);
        (millis > 0).then(|| Duration::from_millis(millis))
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Availability> {
        self.changes.subscribe()
    }
}

impl Default for AtspiService {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect()
    }
}

/// Global identity for one bus object.
fn object_id(bus: &str, path: &str) -> String {
    format!("{ID_SCHEME}{bus}/{}", path.trim_start_matches('/'))
}

/// Split an `atspi://` id back into `(bus, path)`.
fn parse_id(id: &str) -> Result<(String, String), AtspiError> {
    let rest = id
        .strip_prefix(ID_SCHEME)
        .ok_or_else(|| AtspiError::Unsupported(format!("not an AT-SPI element id: {id}")))?;
    let (bus, path) = rest
        .split_once('/')
        .ok_or_else(|| AtspiError::Unsupported(format!("malformed AT-SPI element id: {id}")))?;
    if bus.is_empty() || path.is_empty() {
        return Err(AtspiError::Unsupported(format!(
            "malformed AT-SPI element id: {id}"
        )));
    }
    Ok((bus.to_string(), format!("/{path}")))
}

async fn proxy_for<'a>(
    connection: &'a atspi::zbus::Connection,
    bus: &'a str,
    path: &'a str,
) -> Result<AccessibleProxy<'a>, AtspiError> {
    let destination =
        BusName::try_from(bus).map_err(|detail| AtspiError::bus("parse bus name", detail))?;
    AccessibleProxy::builder(connection)
        .destination(destination)
        .map_err(|detail| AtspiError::bus("proxy destination", detail))?
        .path(path)
        .map_err(|detail| AtspiError::bus("proxy path", detail))?
        .build()
        .await
        .map_err(|detail| AtspiError::bus("proxy build", detail))
}

/// Compatibility view for callers that do not yet carry a host service.
/// This uses one process-wide cached service and therefore has the same
/// non-blocking semantics as [`AtspiService::is_available`].
pub fn is_available() -> bool {
    static SERVICE: std::sync::OnceLock<AtspiService> = std::sync::OnceLock::new();
    SERVICE.get_or_init(AtspiService::new).is_available()
}

struct Walker<'a> {
    connection: &'a atspi::zbus::Connection,
    budget: usize,
}

impl Walker<'_> {
    fn take_budget(&mut self) -> Result<(), AtspiError> {
        if self.budget == 0 {
            return Err(AtspiError::TreeTooLarge {
                depth: MAX_TREE_DEPTH,
                nodes: MAX_TREE_NODES,
            });
        }
        self.budget -= 1;
        Ok(())
    }

    async fn snapshot_object(
        &mut self,
        proxy: &AccessibleProxy<'_>,
        bus: &str,
        path: &str,
        app: &str,
        depth: usize,
    ) -> Result<AccessibleSnapshot, AtspiError> {
        self.take_budget()?;
        let role = proxy
            .get_role()
            .await
            .map_err(|detail| AtspiError::bus("get role", detail))?;
        let name = proxy
            .name()
            .await
            .map(|name| truncate(&name, MAX_NAME_CHARS))
            .unwrap_or_default();
        let description = proxy
            .description()
            .await
            .map(|description| truncate(&description, MAX_NAME_CHARS))
            .unwrap_or_default();
        let states = proxy
            .get_state()
            .await
            .unwrap_or_else(|_| StateSet::empty());
        let interfaces = proxy
            .get_interfaces()
            .await
            .map(|set| {
                set.iter()
                    .map(|interface| format!("{interface:?}"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Actions and text are best-effort: a racing app may drop an
        // interface between `get_interfaces` and the follow-up call.
        let actions = match proxy.proxies().await {
            Ok(proxies) => match proxies.action().await {
                Ok(action) => action
                    .get_actions()
                    .await
                    .map(|actions| {
                        actions
                            .into_iter()
                            .take(MAX_ACTIONS)
                            .map(|action| truncate(&action.name, MAX_NAME_CHARS))
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };

        let value_text = if role == Role::PasswordText {
            None
        } else if interfaces.iter().any(|name| name == "Text") {
            match proxy.proxies().await {
                Ok(proxies) => match proxies.text().await {
                    Ok(text) => {
                        let count = text.character_count().await.unwrap_or(0).max(0);
                        let end = count.min(MAX_TEXT_READ);
                        text.get_text(0, end)
                            .await
                            .ok()
                            .map(|value| truncate(&value, MAX_VALUE_CHARS))
                            .filter(|value| !value.is_empty())
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            }
        } else {
            None
        };

        let bounds = match proxy.proxies().await {
            Ok(proxies) => match proxies.component().await {
                Ok(component) => component.get_extents(CoordType::Screen).await.ok().map(
                    |(x, y, width, height)| Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                ),
                Err(_) => None,
            },
            Err(_) => None,
        };

        let mut children = Vec::new();
        if depth < MAX_TREE_DEPTH {
            let child_refs = proxy.get_children().await.unwrap_or_default();
            for child_ref in child_refs {
                let Some(child_bus) = child_ref.name_as_str().map(|name| name.to_owned()) else {
                    continue;
                };
                let child_path = child_ref.path_as_str().to_string();
                let Ok(child_proxy) = proxy_for(self.connection, &child_bus, &child_path).await
                else {
                    continue;
                };
                match Box::pin(self.snapshot_object(
                    &child_proxy,
                    &child_bus,
                    &child_path,
                    app,
                    depth + 1,
                ))
                .await
                {
                    Ok(child) => children.push(child),
                    Err(AtspiError::TreeTooLarge { .. }) => {
                        return Err(AtspiError::TreeTooLarge {
                            depth,
                            nodes: MAX_TREE_NODES,
                        });
                    }
                    // The child vanished mid-walk; its siblings still count.
                    Err(_) => continue,
                }
                if self.budget == 0 {
                    break;
                }
            }
        }

        Ok(AccessibleSnapshot {
            id: object_id(bus, path),
            app: app.to_string(),
            role,
            name,
            description,
            states,
            interfaces,
            actions,
            value_text,
            bounds,
            children,
        })
    }
}

/// One snapshot tree per top-level window, across all applications.
///
/// Takes the service's shared connection: callers must go through
/// [`AtspiService::run_live`], never construct a connection per call.
pub async fn collect_window_trees(
    accessible: AccessibilityConnection,
) -> Result<Vec<AccessibleSnapshot>, AtspiError> {
    let connection = accessible.connection().clone();
    let root = accessible
        .root_accessible_on_registry()
        .await
        .map_err(|detail| AtspiError::bus("registry root", detail))?;
    tracing::trace!("desktop.atspi.collect.root.ok");
    let mut walker = Walker {
        connection: &connection,
        budget: MAX_TREE_NODES,
    };
    let mut windows = Vec::new();
    let apps = root
        .get_children()
        .await
        .map_err(|detail| AtspiError::bus("list applications", detail))?;
    tracing::trace!(count = apps.len(), "desktop.atspi.collect.apps.ok");
    for app_ref in apps {
        let Some(app_bus) = app_ref.name_as_str().map(|name| name.to_owned()) else {
            continue;
        };
        let app_path = app_ref.path_as_str().to_string();
        let Ok(app_proxy) = proxy_for(&connection, &app_bus, &app_path).await else {
            continue;
        };
        let app_name = app_proxy.name().await.unwrap_or_default();
        let app_name = if app_name.is_empty() {
            app_bus.clone()
        } else {
            truncate(&app_name, MAX_NAME_CHARS)
        };
        let tops = app_proxy.get_children().await.unwrap_or_default();
        for top_ref in tops {
            let Some(top_bus) = top_ref.name_as_str().map(|name| name.to_owned()) else {
                continue;
            };
            let top_path = top_ref.path_as_str().to_string();
            let Ok(top_proxy) = proxy_for(&connection, &top_bus, &top_path).await else {
                continue;
            };
            let role = top_proxy
                .get_role()
                .await
                .map_err(|detail| AtspiError::bus("get role", detail))?;
            if !is_window_role(&role) {
                continue;
            }
            match walker
                .snapshot_object(&top_proxy, &top_bus, &top_path, &app_name, 0)
                .await
            {
                Ok(tree) => windows.push(tree),
                Err(AtspiError::TreeTooLarge { .. }) => return Ok(windows),
                Err(_) => continue,
            }
        }
    }
    Ok(windows)
}

/// Bind an element id to owned `(bus, path)` parts. Call sites keep the
/// pair alive while the proxy borrows it (the builder borrows both, so
/// they cannot be hidden inside a helper that returns the proxy).
fn resolve(id: &str) -> Result<(String, String), AtspiError> {
    parse_id(id)
}

/// Invoke the element's default action (first `click`-like action, else
/// the first action the object advertises).
pub async fn invoke_element(
    accessible: AccessibilityConnection,
    element_id: &str,
) -> Result<(), AtspiError> {
    let connection = accessible.connection().clone();
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let action = proxies
        .action()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Action interface")))?;
    let actions = action
        .get_actions()
        .await
        .map_err(|detail| AtspiError::bus("list actions", detail))?;
    if actions.is_empty() {
        return Err(AtspiError::Unsupported(format!(
            "{element_id} advertises no actions"
        )));
    }
    let preferred = ["click", "press", "toggle", "activate", "jump", "open"];
    let index = actions
        .iter()
        .position(|action| {
            let name = action.name.to_lowercase();
            preferred.iter().any(|want| name.contains(want))
        })
        .unwrap_or(0) as i32;
    let done = action
        .do_action(index)
        .await
        .map_err(|detail| AtspiError::bus("do action", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "do action",
            detail: format!("object rejected action index {index}"),
        })
    }
}

/// Move keyboard focus to the element.
pub async fn focus_element(
    accessible: AccessibilityConnection,
    element_id: &str,
) -> Result<(), AtspiError> {
    let connection = accessible.connection().clone();
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let component = proxies
        .component()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Component interface")))?;
    let done = component
        .grab_focus()
        .await
        .map_err(|detail| AtspiError::bus("grab focus", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "grab focus",
            detail: "object refused focus".to_string(),
        })
    }
}

/// Set an editable element's text (or numeric value for `Value` objects).
pub async fn set_value(
    accessible: AccessibilityConnection,
    element_id: &str,
    value: &str,
) -> Result<(), AtspiError> {
    let connection = accessible.connection().clone();
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    if let Ok(editable) = proxies.editable_text().await {
        let done = editable
            .set_text_contents(value)
            .await
            .map_err(|detail| AtspiError::bus("set text", detail))?;
        return if done {
            Ok(())
        } else {
            Err(AtspiError::ActionFailed {
                operation: "set text",
                detail: "object rejected the new contents".to_string(),
            })
        };
    }
    if let Ok(number) = proxies.value().await {
        let parsed: f64 = value.trim().parse().map_err(|_| {
            AtspiError::Unsupported(format!(
                "{element_id} takes a numeric value, got non-numeric text"
            ))
        })?;
        number
            .set_current_value(parsed)
            .await
            .map_err(|detail| AtspiError::bus("set value", detail))?;
        return Ok(());
    }
    Err(AtspiError::Unsupported(format!(
        "{element_id} is not editable (no EditableText or Value interface)"
    )))
}

/// Select the element through its parent's `Selection` interface.
pub async fn select_element(
    accessible: AccessibilityConnection,
    element_id: &str,
) -> Result<(), AtspiError> {
    let connection = accessible.connection().clone();
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let parent_ref = proxy
        .parent()
        .await
        .map_err(|detail| AtspiError::bus("get parent", detail))?;
    let Some(parent_bus) = parent_ref.name_as_str().map(|name| name.to_owned()) else {
        return Err(AtspiError::Unsupported(
            "element has no addressable parent".to_string(),
        ));
    };
    let parent_path = parent_ref.path_as_str().to_string();
    let parent = proxy_for(&connection, &parent_bus, &parent_path).await?;
    let proxies = parent
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let selection = proxies
        .selection()
        .await
        .map_err(|_| AtspiError::Unsupported("parent has no Selection interface".to_string()))?;
    let siblings = parent
        .get_children()
        .await
        .map_err(|detail| AtspiError::bus("list siblings", detail))?;
    let (_, wanted_path) = parse_id(element_id)?;
    let index = siblings
        .iter()
        .position(|sibling| sibling.path_as_str() == wanted_path)
        .ok_or_else(|| {
            AtspiError::Unsupported("element is no longer among its parent's children".to_string())
        })? as i32;
    let done = selection
        .select_child(index)
        .await
        .map_err(|detail| AtspiError::bus("select child", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "select child",
            detail: "parent rejected the selection".to_string(),
        })
    }
}

/// Run an `expand`- or `collapse`-named action on the element.
pub async fn expand_element(
    accessible: AccessibilityConnection,
    element_id: &str,
    expand: bool,
) -> Result<(), AtspiError> {
    let wanted = if expand { "expand" } else { "collapse" };
    let connection = accessible.connection().clone();
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let action = proxies
        .action()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Action interface")))?;
    let actions = action
        .get_actions()
        .await
        .map_err(|detail| AtspiError::bus("list actions", detail))?;
    let index = actions
        .iter()
        .position(|action| action.name.to_lowercase().contains(wanted))
        .ok_or_else(|| AtspiError::Unsupported(format!("{element_id} has no {wanted} action")))?
        as i32;
    let done = action
        .do_action(index)
        .await
        .map_err(|detail| AtspiError::bus("do action", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "do action",
            detail: format!("object rejected the {wanted} action"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = object_id(":1.42", "/org/a11y/atspi/accessible/1");
        assert_eq!(id, "atspi://:1.42/org/a11y/atspi/accessible/1");
        let (bus, path) = parse_id(&id).unwrap();
        assert_eq!(
            (bus.as_str(), path.as_str()),
            (":1.42", "/org/a11y/atspi/accessible/1")
        );
    }

    #[test]
    fn foreign_ids_are_rejected_not_resolved() {
        assert!(parse_id("0x3400012").is_err());
        assert!(parse_id("atspi://").is_err());
        assert!(parse_id("atspi:///path-only").is_err());
    }

    #[test]
    fn unavailable_service_does_not_block_construction() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = Arc::clone(&calls);
        let probe = Arc::new(move || {
            probe_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2_000));
            false
        });
        let service = AtspiService::with_probe(probe);
        let started = Instant::now();
        service.start_probe();
        // Construction + probe kickoff return immediately; the slow probe
        // runs on the background worker, never on the caller.
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(service.availability(), Availability::Probing);
        assert!(!service.is_available());
    }

    #[test]
    fn repeated_availability_checks_probe_exactly_once() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = Arc::clone(&calls);
        let probe = Arc::new(move || {
            probe_calls.fetch_add(1, Ordering::SeqCst);
            true
        });
        let service = AtspiService::with_probe(probe);
        service.start_probe();
        service.start_probe();
        let deadline = Instant::now() + Duration::from_secs(5);
        while service.availability() == Availability::Probing && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(service.is_available());
        for _ in 0..100 {
            let _ = service.is_available();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failing_probe_is_cached_unavailable() {
        let service = AtspiService::with_probe(Arc::new(|| false));
        service.start_probe();
        let deadline = Instant::now() + Duration::from_secs(5);
        while service.availability() == Availability::Probing && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(service.availability(), Availability::Unavailable);
        assert!(!service.is_available());
    }

    /// Live-session regression test: repeated window-tree collection through
    /// the shared service connection (the host's status-snapshot path) must
    /// complete every time. A fresh `AccessibilityConnection` per call hung
    /// forever on re-creation and wedged the UI thread; the shared
    /// connection plus timeouts rule that out. Skips without a registry.
    #[test]
    fn repeated_window_tree_collection_completes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let service = AtspiService::new();
        // The probe runs on a background worker; wait for it before judging
        // availability so slow sessions skip instead of failing.
        let deadline = Instant::now() + Duration::from_secs(10);
        while service.availability() == Availability::Probing && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !service.is_available() {
            eprintln!("SKIP live AT-SPI test: no registry");
            return;
        }
        for round in 1..=3 {
            let outcome = runtime.block_on(service.run_live("test_collect", collect_window_trees));
            assert!(
                outcome.is_ok(),
                "collect_window_trees round {round} failed: {outcome:?}"
            );
        }
    }
}
