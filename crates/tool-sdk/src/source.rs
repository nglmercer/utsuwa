//! Tool origins (`ToolSource`), static groups (`ToolPack`), and the
//! per-turn composition root (`ToolCatalog`).
//!
//! A `ToolSource` answers "where do this turn's tools come from" (MCP
//! servers, WASM plugins, desktop backends…); a `ToolPack` is the sync
//! convenience form for builtin groups (system clock, process tools…).
//! `ToolCatalog::snapshot` loads every source, stamps provenance, applies
//! the [`ToolProfile`] visibility policy centrally, and returns a fresh
//! immutable [`ToolRegistry`]. Sources are best-effort: one failing source
//! never fails the turn, and duplicate ids are kept first-wins with a
//! warning (matching the historical attach_* behavior).

use crate::profile::ToolProfile;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tool_core::{Tool, ToolRegistry};

/// Identifies the origin of a tool: `builtin.system`,
/// `builtin.process`, `builtin.filesystem`, `memory`, `desktop`,
/// `mcp.<server>`, `plugin.<id>`, ….
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolSourceId(pub String);

impl ToolSourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Where a registered tool came from. Recorded by the catalog at snapshot
/// time; promoted into `ToolMetadata` once the metadata-literal migration
/// lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProvenance {
    pub source: ToolSourceId,
    pub version: Option<String>,
}

impl ToolProvenance {
    pub fn new(source: ToolSourceId) -> Self {
        Self {
            source,
            version: None,
        }
    }
}

/// Per-turn inputs for sources. Deliberately small: sources that need host
/// facts (OS environment, file context, managers) receive them through
/// their constructors, so this crate never depends on `app-host`.
/// Richer host-owned context moves to a future `host-core` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolLoadContext {
    pub profile: ToolProfile,
}

impl ToolLoadContext {
    pub fn new(profile: ToolProfile) -> Self {
        Self { profile }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolSourceError {
    #[error("tool source '{id}' failed: {message}")]
    Load { id: String, message: String },
}

impl ToolSourceError {
    pub fn load(source: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Load {
            id: source.into(),
            message: message.into(),
        }
    }
}

/// An async collection of tools (MCP servers, WASM plugins, desktop…).
#[async_trait::async_trait]
pub trait ToolSource: Send + Sync {
    fn id(&self) -> &'static str;

    async fn load(&self, ctx: &ToolLoadContext) -> Result<Vec<Arc<dyn Tool>>, ToolSourceError>;
}

/// A static group of native tools. Blanket-adapted into `ToolSource` via
/// [`PackSource`] (re-exported as [`ClosureSource`]-style ergonomics are
/// unnecessary: `ToolCatalog::add_pack` covers it).
pub trait ToolPack: Send + Sync {
    fn id(&self) -> &'static str;

    fn tools(&self, ctx: &ToolLoadContext) -> Vec<Arc<dyn Tool>>;
}

/// Adapts any `ToolPack` into a `ToolSource`.
pub struct PackSource<P: ToolPack>(pub P);

impl<P: ToolPack> PackSource<P> {
    pub fn new(pack: P) -> Self {
        Self(pack)
    }
}

#[async_trait::async_trait]
impl<P: ToolPack> ToolSource for PackSource<P> {
    fn id(&self) -> &'static str {
        self.0.id()
    }

    async fn load(&self, ctx: &ToolLoadContext) -> Result<Vec<Arc<dyn Tool>>, ToolSourceError> {
        Ok(self.0.tools(ctx))
    }
}

type BoxLoadFuture =
    Pin<Box<dyn Future<Output = Result<Vec<Arc<dyn Tool>>, ToolSourceError>> + Send>>;

/// A `ToolSource` built from a closure. Used by the host to adapt legacy
/// per-turn attach logic without new structs per source during migration.
/// The context is passed by value (`ToolLoadContext` is `Copy`), so no
/// higher-ranked lifetimes leak into callers.
pub struct ClosureSource {
    id: &'static str,
    load: Arc<dyn Fn(ToolLoadContext) -> BoxLoadFuture + Send + Sync>,
}

impl ClosureSource {
    pub fn new<F, Fut>(id: &'static str, load: F) -> Self
    where
        F: Fn(ToolLoadContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<Arc<dyn Tool>>, ToolSourceError>> + Send + 'static,
    {
        let load = Arc::new(move |ctx: ToolLoadContext| {
            let fut = load(ctx);
            Box::pin(fut) as BoxLoadFuture
        });
        Self { id, load }
    }
}

#[async_trait::async_trait]
impl ToolSource for ClosureSource {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn load(&self, ctx: &ToolLoadContext) -> Result<Vec<Arc<dyn Tool>>, ToolSourceError> {
        (self.load)(*ctx).await
    }
}

/// Ordered set of sources; the composition root. Built fresh per turn by
/// the host (cheap: a vector of Arcs), then snapshotted once.
///
/// Sources are optional by default (best-effort: a failing MCP server or
/// plugin logs and skips, never fails the turn). Core sources use
/// [`ToolCatalog::with_required`]: their failure fails the snapshot, which
/// preserves the historical fail-fast behavior for the builtin toolset.
#[derive(Default)]
pub struct ToolCatalog {
    sources: Vec<(Arc<dyn ToolSource>, bool)>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Best-effort source: load failures warn and skip.
    pub fn with_source<S: ToolSource + 'static>(mut self, source: S) -> Self {
        self.sources.push((Arc::new(source), false));
        self
    }

    /// Best-effort source from a shared handle.
    pub fn with_arc(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.sources.push((source, false));
        self
    }

    /// Required source: a load failure fails the snapshot.
    pub fn with_required<S: ToolSource + 'static>(mut self, source: S) -> Self {
        self.sources.push((Arc::new(source), true));
        self
    }

    /// Required source from a shared handle.
    pub fn with_required_arc(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.sources.push((source, true));
        self
    }

    pub fn with_pack<P: ToolPack + 'static>(self, pack: P) -> Self {
        self.with_source(PackSource::new(pack))
    }

    pub fn with_required_pack<P: ToolPack + 'static>(self, pack: P) -> Self {
        self.with_required(PackSource::new(pack))
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Load every source and build the immutable per-turn registry.
    /// Profile filtering is central here; per-source pre-filtering (kept
    /// during migration) is idempotent.
    pub async fn snapshot(&self, ctx: &ToolLoadContext) -> Result<ToolRegistry, ToolSourceError> {
        let (registry, _) = self.snapshot_with_provenance(ctx).await?;
        Ok(registry)
    }

    /// Snapshot plus per-tool provenance (`tool id → source`).
    pub async fn snapshot_with_provenance(
        &self,
        ctx: &ToolLoadContext,
    ) -> Result<(ToolRegistry, HashMap<String, ToolProvenance>), ToolSourceError> {
        let mut registry = ToolRegistry::new();
        let mut provenance = HashMap::new();
        let policy = ctx.profile.policy();
        for (source, required) in &self.sources {
            let tools = match source.load(ctx).await {
                Ok(tools) => tools,
                Err(e) => {
                    if *required {
                        return Err(e);
                    }
                    tracing::warn!(source = source.id(), error = %e, "tool source failed; skipping");
                    continue;
                }
            };
            for tool in tools {
                let id = tool.metadata().id.0.clone();
                if !policy.visible(&id) {
                    continue;
                }
                match registry.register(Arc::clone(&tool)) {
                    Ok(()) => {
                        provenance.insert(id, ToolProvenance::new(ToolSourceId::new(source.id())));
                    }
                    Err(tool_core::ToolError::DuplicateId(_)) => {
                        tracing::warn!(tool = %id, source = source.id(), "duplicate tool id; keeping first");
                    }
                    Err(e) => {
                        tracing::warn!(tool = %id, source = source.id(), error = %e, "tool registration failed; skipping");
                    }
                }
            }
        }
        Ok((registry, provenance))
    }
}

/// Drain a populated registry back into a tool vector. Migration helper
/// for adapting legacy `attach_*(&mut ToolRegistry)` logic into
/// `ToolSource::load` without rewriting collection code.
pub fn collect_tools(registry: &ToolRegistry) -> Vec<Arc<dyn Tool>> {
    registry
        .list()
        .iter()
        .filter_map(|metadata| registry.resolve(&metadata.id.0).ok())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::ToolId;
    use tool_core::{ToolContext, ToolError, ToolMetadata, ToolOutput};

    struct StaticTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for StaticTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: ToolId::new(self.0),
                description: "test".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                effects: vec![tool_core::ToolEffect::ReadOnly],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new(args))
        }
    }

    struct StaticPack(Vec<&'static str>);

    impl ToolPack for StaticPack {
        fn id(&self) -> &'static str {
            "test.static"
        }

        fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn Tool>> {
            self.0
                .iter()
                .map(|id| Arc::new(StaticTool(id)) as Arc<dyn Tool>)
                .collect()
        }
    }

    struct FailingSource;

    #[async_trait::async_trait]
    impl ToolSource for FailingSource {
        fn id(&self) -> &'static str {
            "test.failing"
        }

        async fn load(
            &self,
            _ctx: &ToolLoadContext,
        ) -> Result<Vec<Arc<dyn Tool>>, ToolSourceError> {
            Err(ToolSourceError::load(self.id(), "boom"))
        }
    }

    #[tokio::test]
    async fn snapshot_collects_packs_and_tolerates_failures() {
        let catalog = ToolCatalog::new()
            .with_pack(StaticPack(vec!["a.x", "a.y"]))
            .with_source(FailingSource)
            .with_pack(StaticPack(vec!["b.z"]));
        let registry = catalog
            .snapshot(&ToolLoadContext::new(ToolProfile::Full))
            .await
            .unwrap();
        let ids: Vec<String> = registry.list().into_iter().map(|m| m.id.0).collect();
        assert_eq!(ids, vec!["a.x", "a.y", "b.z"]);
    }

    #[tokio::test]
    async fn snapshot_applies_simple_profile_centrally() {
        let catalog = ToolCatalog::new().with_pack(StaticPack(vec![
            "filesystem.read",
            "filesystem.patch",
            "system.time",
        ]));
        let registry = catalog
            .snapshot(&ToolLoadContext::new(ToolProfile::Simple))
            .await
            .unwrap();
        let ids: Vec<String> = registry.list().into_iter().map(|m| m.id.0).collect();
        assert_eq!(ids, vec!["filesystem.read", "system.time"]);
    }

    #[tokio::test]
    async fn snapshot_keeps_first_duplicate_and_records_provenance() {
        let catalog = ToolCatalog::new()
            .with_pack(StaticPack(vec!["dup.x"]))
            .with_pack(StaticPack(vec!["dup.x", "other.y"]));
        let (registry, provenance) = catalog
            .snapshot_with_provenance(&ToolLoadContext::new(ToolProfile::Full))
            .await
            .unwrap();
        assert_eq!(registry.list().len(), 2);
        assert_eq!(
            provenance.get("dup.x").unwrap().source,
            ToolSourceId::new("test.static")
        );
        assert!(provenance.contains_key("other.y"));
    }

    #[tokio::test]
    async fn closure_source_adapts_async_logic() {
        let catalog = ToolCatalog::new().with_arc(Arc::new(ClosureSource::new(
            "test.closure",
            |_ctx| async move { Ok(vec![Arc::new(StaticTool("c.q")) as Arc<dyn Tool>]) },
        )));
        let registry = catalog
            .snapshot(&ToolLoadContext::new(ToolProfile::Full))
            .await
            .unwrap();
        assert_eq!(registry.list().len(), 1);
    }

    struct AlwaysFail;

    #[async_trait::async_trait]
    impl ToolSource for AlwaysFail {
        fn id(&self) -> &'static str {
            "test.required-fail"
        }

        async fn load(
            &self,
            _ctx: &ToolLoadContext,
        ) -> Result<Vec<Arc<dyn Tool>>, ToolSourceError> {
            Err(ToolSourceError::load(self.id(), "core exploded"))
        }
    }

    #[tokio::test]
    async fn required_source_failure_fails_snapshot() {
        let catalog = ToolCatalog::new()
            .with_pack(StaticPack(vec!["ok.x"]))
            .with_required(AlwaysFail);
        let err = catalog
            .snapshot(&ToolLoadContext::new(ToolProfile::Full))
            .await
            .err()
            .expect("required source failure must fail the snapshot");
        assert!(err.to_string().contains("test.required-fail"));
    }

    #[test]
    fn collect_tools_drains_registry() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(StaticTool("z.a")) as Arc<dyn Tool>)
            .unwrap();
        registry
            .register(Arc::new(StaticTool("z.b")) as Arc<dyn Tool>)
            .unwrap();
        let tools = collect_tools(&registry);
        assert_eq!(tools.len(), 2);
    }
}
