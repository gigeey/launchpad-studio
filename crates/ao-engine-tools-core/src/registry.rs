use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::policy::{LoadPolicy, LoadPolicyOverride};
use crate::tool::{EngineTool, IoTool};

/// A single entry in the deferred tool catalog, used by ToolSearch.
#[derive(Debug, Clone)]
pub struct DeferredEntry {
    /// Tool name as registered.
    pub name: String,
    /// First sentence of description (or 40-word truncation).
    pub short_description: String,
    /// Lowercase concat of name + full description + input_schema field names,
    /// used for keyword scoring.
    pub search_text: String,
}

/// Searchable index of all deferred tools, built once at registry build time.
#[derive(Debug, Default, Clone)]
pub struct DeferredIndex {
    entries: Vec<DeferredEntry>,
}

impl DeferredIndex {
    pub fn entries(&self) -> &[DeferredEntry] {
        &self.entries
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<&DeferredEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

fn extract_short_description(desc: &str) -> String {
    let words: Vec<&str> = desc.split_whitespace().collect();
    let window: String = words.iter().take(40).copied().collect::<Vec<_>>().join(" ");
    let bytes = window.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?') {
            let rest = &window[i + 1..];
            if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
                return window[..=i].to_string();
            }
        }
    }
    window
}

fn extract_search_text(name: &str, description: &str, schema: &Value) -> String {
    let mut parts = vec![name.to_lowercase(), description.to_lowercase()];
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for key in props.keys() {
            parts.push(key.to_lowercase());
        }
    }
    parts.join(" ")
}

/// Looked-up tool reference. Returned by `Registry::lookup` so callers
/// can dispatch without caring whether a name belongs to the IO or engine
/// surface — but also can branch on category when they need to.
#[derive(Clone)]
pub enum ToolRef {
    Io(Arc<dyn IoTool>),
    Engine(Arc<dyn EngineTool>),
}

impl ToolRef {
    pub fn name(&self) -> &str {
        match self {
            Self::Io(t) => t.name(),
            Self::Engine(t) => t.name(),
        }
    }

    pub fn category(&self) -> ToolCategory {
        match self {
            Self::Io(_) => ToolCategory::Io,
            Self::Engine(_) => ToolCategory::Engine,
        }
    }

    pub fn load_policy(&self) -> LoadPolicy {
        match self {
            Self::Io(t) => t.load_policy(),
            Self::Engine(t) => t.load_policy(),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Io(t) => t.description(),
            Self::Engine(t) => t.description(),
        }
    }

    pub fn input_schema(&self) -> Value {
        match self {
            Self::Io(t) => t.input_schema(),
            Self::Engine(t) => t.input_schema(),
        }
    }

    /// Whether multiple calls to this tool within one turn may execute
    /// concurrently. Delegates to [`IoTool::is_concurrency_safe`] or
    /// [`EngineTool::is_concurrency_safe`].
    pub fn is_concurrency_safe(&self) -> bool {
        match self {
            Self::Io(t) => t.is_concurrency_safe(),
            Self::Engine(t) => t.is_concurrency_safe(),
        }
    }

    /// Whether this tool interacts with external or unpredictable systems.
    /// Delegates to [`IoTool::mcp_open_world_hint`] or
    /// [`EngineTool::mcp_open_world_hint`].
    pub fn mcp_open_world_hint(&self) -> bool {
        match self {
            Self::Io(t) => t.mcp_open_world_hint(),
            Self::Engine(t) => t.mcp_open_world_hint(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Io,
    Engine,
}

/// Per-session tool registry ("per-session, with a process-global
/// catalog of factories"). Tools register into a single instance at session
/// start; the runner clones an `Arc<Registry>` into every `RunnerContext`.
///
/// Cloning a `Registry` is cheap — the underlying tool instances are `Arc`-shared,
/// so each clone copies only the `Arc` handles (not the tool objects). Use this to
/// build per-session extensions (e.g. adding autonomous-only tools) without
/// touching the process-wide base registry.
#[derive(Default)]
pub struct Registry {
    io: HashMap<String, Arc<dyn IoTool>>,
    engine: HashMap<String, Arc<dyn EngineTool>>,
    /// Dynamically registered IO tools that can be mutated through a shared reference.
    ///
    /// Provider request builders read list() and lookup() on each turn, so any swap
    /// performed between turns is automatically visible on the next turn without
    /// runner restart.
    runtime_io: std::sync::RwLock<HashMap<String, Arc<dyn IoTool>>>,
    deferred_index: DeferredIndex,
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        let runtime_io_guard = self.runtime_io.read().unwrap();
        Self {
            io: self.io.clone(),
            engine: self.engine.clone(),
            runtime_io: std::sync::RwLock::new(runtime_io_guard.clone()),
            deferred_index: self.deferred_index.clone(),
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_io(&mut self, tool: Arc<dyn IoTool>) {
        self.io.insert(tool.name().to_string(), tool);
    }

    pub fn register_engine(&mut self, tool: Arc<dyn EngineTool>) {
        self.engine.insert(tool.name().to_string(), tool);
    }

    /// Register an IO tool into the runtime-dynamic slot using interior mutability.
    ///
    /// This allows inserting tools through a shared `&self` reference (e.g. from
    /// inside an `Arc<Registry>`). The tool is immediately visible via `lookup`,
    /// `lookup_io`, and `list` on all shared references.
    pub fn register_io_dynamic(&self, tool: Arc<dyn IoTool>) {
        self.runtime_io.write().unwrap().insert(tool.name().to_string(), tool);
    }

    /// Remove all dynamic tools whose names start with `prefix`.
    ///
    /// Returns the list of removed tool names. Static `io` and `engine` tools are
    /// never affected.
    pub fn remove_by_prefix(&self, prefix: &str) -> Vec<String> {
        let mut guard = self.runtime_io.write().unwrap();
        let to_remove: Vec<String> = guard.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        for name in &to_remove {
            guard.remove(name);
        }
        to_remove
    }

    pub fn lookup(&self, name: &str) -> Option<ToolRef> {
        if let Some(t) = self.io.get(name) {
            return Some(ToolRef::Io(t.clone()));
        }
        {
            let guard = self.runtime_io.read().unwrap();
            if let Some(t) = guard.get(name) {
                return Some(ToolRef::Io(t.clone()));
            }
        }
        self.engine.get(name).map(|t| ToolRef::Engine(t.clone()))
    }

    pub fn lookup_io(&self, name: &str) -> Option<Arc<dyn IoTool>> {
        if let Some(t) = self.io.get(name) {
            return Some(t.clone());
        }
        let guard = self.runtime_io.read().unwrap();
        guard.get(name).cloned()
    }

    pub fn lookup_engine(&self, name: &str) -> Option<Arc<dyn EngineTool>> {
        self.engine.get(name).cloned()
    }

    /// Return every registered tool name, sorted, regardless of category.
    pub fn list(&self) -> Vec<String> {
        let runtime_guard = self.runtime_io.read().unwrap();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut names: Vec<String> = Vec::new();
        for k in self.io.keys() {
            if seen.insert(k.as_str()) {
                names.push(k.clone());
            }
        }
        for k in self.engine.keys() {
            if seen.insert(k.as_str()) {
                names.push(k.clone());
            }
        }
        for k in runtime_guard.keys() {
            if seen.insert(k.as_str()) {
                names.push(k.clone());
            }
        }
        names.sort();
        names
    }

    /// Find the registered tool name closest to `query` using a case-insensitive
    /// fuzzy match. Returns `None` if no candidate scores below the threshold.
    ///
    /// Used by dispatch sites to turn a bare "unknown tool" error into a
    /// "did you mean X?" suggestion when the model emits a typo or a
    /// case-folded variant. The suggestion is then composed into a corrective
    /// error message that shows the proper invocation shape.
    ///
    /// Score (lower = better):
    /// - `0` — case-insensitive equality with a registered name (e.g. the
    ///         model wrote `workflowactiondelete` and we have
    ///         `WorkflowActionDelete`).
    /// - `1` — `query` is a substring of a name, or vice versa, with the
    ///         shorter side ≥ 3 chars (avoids over-suggesting on 1–2 char
    ///         queries).
    /// - `n` — lowercase Levenshtein edit distance, otherwise.
    ///
    /// Candidates above edit-distance 3 are rejected. On a tie, the
    /// alphabetically first name wins so callers see stable suggestions.
    pub fn nearest_name(&self, query: &str) -> Option<String> {
        if query.is_empty() {
            return None;
        }
        let q_lower = query.to_lowercase();
        let mut best: Option<(usize, String)> = None;

        for name in self.list() {
            let n_lower = name.to_lowercase();
            let score = if q_lower == n_lower {
                0
            } else if (q_lower.len() >= 3 && n_lower.contains(&q_lower))
                || (n_lower.len() >= 3 && q_lower.contains(&n_lower))
            {
                1
            } else {
                let d = levenshtein(&q_lower, &n_lower);
                if d > 3 {
                    continue;
                }
                d
            };
            let is_better = match &best {
                None => true,
                Some((s, _)) => score < *s,
            };
            if is_better {
                best = Some((score, name));
            }
        }

        best.map(|(_, n)| n)
    }

    pub fn len(&self) -> usize {
        let runtime_guard = self.runtime_io.read().unwrap();
        let dynamic_unique = runtime_guard.keys().filter(|k| !self.io.contains_key(*k)).count();
        self.io.len() + self.engine.len() + dynamic_unique
    }

    pub fn is_empty(&self) -> bool {
        self.io.is_empty() && self.engine.is_empty() && self.runtime_io.read().unwrap().is_empty()
    }

    /// Produce a new `Registry` containing only tools whose `name()` appears
    /// in `allowed`.
    ///
    /// ## Mechanism
    ///
    /// `filter_for` constructs a fresh `Registry` and inserts `Arc`-cloned
    /// references to matching tools from `self`. No tool data is copied —
    /// only the reference counts increment. The returned registry is wholly
    /// independent of `self` after construction.
    ///
    /// This is the chosen filter mechanism for child-registry construction in
    /// `SubagentSpawner::build_child_context` (documented in
    /// `background_agents/mod.rs`).
    ///
    /// Tool names not present in `self` are silently skipped; the caller is
    /// responsible for pre-validating names (e.g. via the loader warning
    /// path).
    pub fn filter_for<S: AsRef<str>>(&self, allowed: &[S]) -> Registry {
        let mut out = Registry::new();
        let runtime_guard = self.runtime_io.read().unwrap();
        for name in allowed {
            let name = name.as_ref();
            if let Some(t) = self.io.get(name) {
                out.io.insert(name.to_string(), t.clone());
            }
            if let Some(t) = self.engine.get(name) {
                out.engine.insert(name.to_string(), t.clone());
            }
            if let Some(t) = runtime_guard.get(name) {
                out.register_io_dynamic(t.clone());
            }
        }
        out
    }

    /// Build the deferred index from all currently registered tools. Call this
    /// once after all tools have been registered (e.g. at the end of
    /// `register_all`). Until called, `deferred_index()` returns an empty index.
    pub fn build_deferred_index(&mut self) {
        let mut entries: Vec<DeferredEntry> = Vec::new();

        for tool in self.io.values() {
            if tool.load_policy() == LoadPolicy::Deferred {
                entries.push(DeferredEntry {
                    name: tool.name().to_string(),
                    short_description: extract_short_description(tool.description()),
                    search_text: extract_search_text(
                        tool.name(),
                        tool.description(),
                        &tool.input_schema(),
                    ),
                });
            }
        }

        for tool in self.engine.values() {
            if tool.load_policy() == LoadPolicy::Deferred {
                entries.push(DeferredEntry {
                    name: tool.name().to_string(),
                    short_description: extract_short_description(tool.description()),
                    search_text: extract_search_text(
                        tool.name(),
                        tool.description(),
                        &tool.input_schema(),
                    ),
                });
            }
        }

        {
            let runtime_guard = self.runtime_io.read().unwrap();
            for tool in runtime_guard.values() {
                if tool.load_policy() == LoadPolicy::Deferred {
                    entries.push(DeferredEntry {
                        name: tool.name().to_string(),
                        short_description: extract_short_description(tool.description()),
                        search_text: extract_search_text(
                            tool.name(),
                            tool.description(),
                            &tool.input_schema(),
                        ),
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.dedup_by(|a, b| a.name == b.name);
        self.deferred_index = DeferredIndex { entries };
    }

    /// Return the deferred tool index built by the last `build_deferred_index()` call.
    pub fn deferred_index(&self) -> &DeferredIndex {
        &self.deferred_index
    }

    /// Compute the set of tool names that resolve to AlwaysLoad for this session,
    /// after applying per-user `overrides`. Unknown names in `overrides` are ignored.
    pub fn resolved_loaded_set(
        &self,
        overrides: &HashMap<String, LoadPolicyOverride>,
    ) -> HashSet<String> {
        let mut set = HashSet::new();

        for (name, tool) in &self.io {
            let include = match overrides.get(name.as_str()) {
                Some(LoadPolicyOverride::ForceAlwaysLoad) => true,
                Some(LoadPolicyOverride::ForceDeferred) => false,
                None => tool.load_policy() == LoadPolicy::AlwaysLoad,
            };
            if include {
                set.insert(name.clone());
            }
        }

        for (name, tool) in &self.engine {
            let include = match overrides.get(name.as_str()) {
                Some(LoadPolicyOverride::ForceAlwaysLoad) => true,
                Some(LoadPolicyOverride::ForceDeferred) => false,
                None => tool.load_policy() == LoadPolicy::AlwaysLoad,
            };
            if include {
                set.insert(name.clone());
            }
        }

        {
            let runtime_guard = self.runtime_io.read().unwrap();
            for (name, tool) in runtime_guard.iter() {
                // Static io takes precedence; skip if already handled
                if self.io.contains_key(name) {
                    continue;
                }
                let include = match overrides.get(name.as_str()) {
                    Some(LoadPolicyOverride::ForceAlwaysLoad) => true,
                    Some(LoadPolicyOverride::ForceDeferred) => false,
                    None => tool.load_policy() == LoadPolicy::AlwaysLoad,
                };
                if include {
                    set.insert(name.clone());
                }
            }
        }

        set
    }

    /// Export the catalog as JSON suitable for handing to a provider's
    /// tool-listing endpoint or logging at session start. Each entry has
    /// `name`, `description`, `input_schema`, `category`, and
    /// `concurrency_safe`. Output is deterministic (sorted by name).
    pub fn schema_export(&self) -> Value {
        let mut entries: Vec<Value> = Vec::with_capacity(self.len());

        // Collect all io names: static + dynamic (dedup, static takes precedence)
        let runtime_guard = self.runtime_io.read().unwrap();
        let mut io_names: Vec<String> = self.io.keys().cloned().collect();
        for k in runtime_guard.keys() {
            if !self.io.contains_key(k) {
                io_names.push(k.clone());
            }
        }
        io_names.sort();

        for name in &io_names {
            // Look up in static io first, then dynamic. We clone the Arc so the
            // borrow from the guards is released before we use the value.
            let arc_tool: Arc<dyn IoTool> = if let Some(t) = self.io.get(name) {
                t.clone()
            } else if let Some(t) = runtime_guard.get(name) {
                t.clone()
            } else {
                continue;
            };
            let t: &dyn IoTool = arc_tool.as_ref();
            entries.push(json!({
                "name": t.name(),
                "description": t.description(),
                "input_schema": t.input_schema(),
                "category": "io",
                "concurrency_safe": t.is_concurrency_safe(),
            }));
        }

        let mut engine_names: Vec<&String> = self.engine.keys().collect();
        engine_names.sort();
        for name in engine_names {
            let t = &self.engine[name];
            entries.push(json!({
                "name": t.name(),
                "description": t.description(),
                "input_schema": t.input_schema(),
                "category": "engine",
                "concurrency_safe": t.is_concurrency_safe(),
            }));
        }
        Value::Array(entries)
    }
}

/// Classic dynamic-programming Levenshtein edit distance.
///
/// Two `O(n)` rolling rows; iterates chars so it works on non-ASCII names
/// (registered tool names are ASCII today, but the helper is cheap to keep
/// general). Returns the number of single-character insertions, deletions, or
/// substitutions to turn `a` into `b`.
fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = std::cmp::min(
                std::cmp::min(curr[j - 1] + 1, prev[j] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RunnerContext;
    use crate::output::ToolOutput;
    use crate::tool::{EngineTool, IoTool};
    use ao_protocol::error::AoError;
    use async_trait::async_trait;
    use serde_json::json;

    struct NoopIo;
    #[async_trait]
    impl IoTool for NoopIo {
        fn name(&self) -> &str {
            "noop_io"
        }
        fn description(&self) -> &str {
            "noop io tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("io ok"))
        }
    }

    struct NoopEngine;
    #[async_trait]
    impl EngineTool for NoopEngine {
        fn name(&self) -> &str {
            "noop_engine"
        }
        fn description(&self) -> &str {
            "noop engine tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("engine ok"))
        }
    }

    /// A deferred engine tool stub for testing the index and resolved_loaded_set.
    struct DeferredEngine;
    #[async_trait]
    impl EngineTool for DeferredEngine {
        fn name(&self) -> &str {
            "deferred_engine"
        }
        fn description(&self) -> &str {
            "A deferred tool. Only loaded on demand via ToolSearch."
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"query": {"type": "string"}}})
        }
        fn load_policy(&self) -> crate::policy::LoadPolicy {
            crate::policy::LoadPolicy::Deferred
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("deferred ok"))
        }
    }

    #[tokio::test]
    async fn register_lookup_invoke_round_trip() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));
        r.register_engine(Arc::new(NoopEngine));

        assert_eq!(r.list(), vec!["noop_engine", "noop_io"]);
        assert_eq!(r.len(), 2);

        let ctx = RunnerContext::new("s", "a").unwrap();

        let io = r.lookup_io("noop_io").unwrap();
        match io.invoke(json!({}), &ctx).await.unwrap() {
            ToolOutput::Text(s) => assert_eq!(s, "io ok"),
            _ => panic!("expected text"),
        }

        let eng = r.lookup_engine("noop_engine").unwrap();
        match eng.invoke(json!({}), &ctx).await.unwrap() {
            ToolOutput::Text(s) => assert_eq!(s, "engine ok"),
            _ => panic!("expected text"),
        }

        match r.lookup("noop_io").unwrap().category() {
            ToolCategory::Io => {}
            _ => panic!("expected io category"),
        }
        match r.lookup("noop_engine").unwrap().category() {
            ToolCategory::Engine => {}
            _ => panic!("expected engine category"),
        }
        assert!(r.lookup("missing").is_none());
    }

    #[test]
    fn schema_export_shape() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));
        r.register_engine(Arc::new(NoopEngine));

        let schema = r.schema_export();
        let arr = schema.as_array().expect("array");
        assert_eq!(arr.len(), 2);

        // Ordering: io entries first (sorted), then engine entries (sorted).
        assert_eq!(arr[0]["name"], "noop_io");
        assert_eq!(arr[0]["category"], "io");
        assert_eq!(arr[0]["concurrency_safe"], true);
        assert_eq!(arr[1]["name"], "noop_engine");
        assert_eq!(arr[1]["category"], "engine");
        assert_eq!(arr[1]["concurrency_safe"], false);
    }

    #[test]
    fn deferred_index_contains_exactly_deferred_tools() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));
        r.register_engine(Arc::new(NoopEngine));
        r.register_engine(Arc::new(DeferredEngine));
        r.build_deferred_index();

        let idx = r.deferred_index();
        assert_eq!(idx.entries().len(), 1);
        assert_eq!(idx.entries()[0].name, "deferred_engine");
        assert!(idx.lookup_by_name("deferred_engine").is_some());
        assert!(idx.lookup_by_name("noop_engine").is_none());
    }

    #[test]
    fn resolved_loaded_set_no_overrides_equals_always_load_set() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));
        r.register_engine(Arc::new(NoopEngine));
        r.register_engine(Arc::new(DeferredEngine));

        let set = r.resolved_loaded_set(&HashMap::new());
        assert!(set.contains("noop_io"));
        assert!(set.contains("noop_engine"));
        assert!(!set.contains("deferred_engine"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn resolved_loaded_set_force_always_load_adds_deferred_tool() {
        let mut r = Registry::new();
        r.register_engine(Arc::new(DeferredEngine));

        let overrides = HashMap::from([(
            "deferred_engine".to_string(),
            LoadPolicyOverride::ForceAlwaysLoad,
        )]);
        let set = r.resolved_loaded_set(&overrides);
        assert!(set.contains("deferred_engine"));
    }

    #[test]
    fn resolved_loaded_set_force_deferred_removes_always_load_tool() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));

        let overrides = HashMap::from([(
            "noop_io".to_string(),
            LoadPolicyOverride::ForceDeferred,
        )]);
        let set = r.resolved_loaded_set(&overrides);
        assert!(!set.contains("noop_io"));
        assert!(set.is_empty());
    }

    #[test]
    fn resolved_loaded_set_unknown_override_names_ignored() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NoopIo));

        let overrides = HashMap::from([(
            "no_such_tool".to_string(),
            LoadPolicyOverride::ForceAlwaysLoad,
        )]);
        let set = r.resolved_loaded_set(&overrides);
        assert_eq!(set.len(), 1);
        assert!(set.contains("noop_io"));
    }

    // ── nearest_name / levenshtein ───────────────────────────────────────────

    /// Per-tool stub with a configurable name. Lets us register a richer
    /// alphabet of tool names for fuzzy-match tests without growing the
    /// unrelated NoopIo / NoopEngine surface.
    struct NamedIo(&'static str);

    #[async_trait]
    impl IoTool for NamedIo {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "named io tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn registry_with_names(names: &[&'static str]) -> Registry {
        let mut r = Registry::new();
        for n in names {
            r.register_io(Arc::new(NamedIo(n)));
        }
        r
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        // single substitution
        assert_eq!(levenshtein("abc", "abd"), 1);
        // single insertion
        assert_eq!(levenshtein("abc", "abxc"), 1);
        // single deletion
        assert_eq!(levenshtein("abxc", "abc"), 1);
        // two ops
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn nearest_name_empty_query_returns_none() {
        let r = registry_with_names(&["WorkflowActionDelete"]);
        assert_eq!(r.nearest_name(""), None);
    }

    #[test]
    fn nearest_name_empty_registry_returns_none() {
        let r = Registry::new();
        assert_eq!(r.nearest_name("WorkflowActionDelete"), None);
    }

    #[test]
    fn nearest_name_case_insensitive_exact_match() {
        // Model writes the name in a different casing — perfect match.
        let r = registry_with_names(&["WorkflowActionDelete"]);
        assert_eq!(
            r.nearest_name("workflowactiondelete"),
            Some("WorkflowActionDelete".to_string())
        );
        assert_eq!(
            r.nearest_name("WORKFLOWACTIONDELETE"),
            Some("WorkflowActionDelete".to_string())
        );
    }

    #[test]
    fn nearest_name_single_typo_within_threshold() {
        let r = registry_with_names(&["WorkflowActionDelete"]);
        // 1 extra char
        assert_eq!(
            r.nearest_name("WorkflowActionDeletee"),
            Some("WorkflowActionDelete".to_string())
        );
        // 1 substitution
        assert_eq!(
            r.nearest_name("WorkflowActionDelxte"),
            Some("WorkflowActionDelete".to_string())
        );
    }

    #[test]
    fn nearest_name_far_typo_returns_none() {
        let r = registry_with_names(&["WorkflowActionDelete"]);
        // 5+ char diff — over threshold 3
        assert_eq!(r.nearest_name("CompletelyDifferentToolName"), None);
        assert_eq!(r.nearest_name("Bash"), None);
    }

    #[test]
    fn nearest_name_substring_match_when_query_long_enough() {
        let r = registry_with_names(&["WorkflowActionDelete", "AssignmentDelete"]);
        // "Delete" is a substring of two tool names — should pick alphabetically
        // first on tie (both score 1 via substring).
        assert_eq!(
            r.nearest_name("Delete"),
            Some("AssignmentDelete".to_string())
        );
    }

    #[test]
    fn nearest_name_does_not_substring_match_for_short_queries() {
        // 1-2 char queries must not substring-match. Otherwise "X" would suggest
        // any tool name containing "x" — useless and noisy.
        let r = registry_with_names(&["WorkflowActionExtract"]);
        assert_eq!(r.nearest_name("X"), None);
        assert_eq!(r.nearest_name("Xc"), None);
    }

    #[test]
    fn nearest_name_picks_lowest_distance_on_competition() {
        // Two candidates: one a typo (distance 1), one a substring (score 1).
        // Tie at score 1 — alphabetically first wins.
        let r = registry_with_names(&["WorkflowActionDelete", "AbcWorkflow"]);
        // Query "WorkflowActionDelte" (typo of WorkflowActionDelete): distance 1.
        // Query also substrings "Workflow" of "AbcWorkflow": score 1 each.
        // For this specific query, only WorkflowActionDelete has distance 1.
        // The substring direction matters: "WorkflowActionDelte" contains
        // "Workflow" but "AbcWorkflow" does not contain "WorkflowActionDelte".
        assert_eq!(
            r.nearest_name("WorkflowActionDelte"),
            Some("WorkflowActionDelete".to_string())
        );
    }

    #[test]
    fn nearest_name_ignores_unknown_completely_unrelated_query() {
        let r = registry_with_names(&[
            "WorkflowActionDelete",
            "AssignmentCreate",
            "DateTime",
        ]);
        // "asdfqwer" is unrelated to any registered name.
        assert_eq!(r.nearest_name("asdfqwer"), None);
    }

    // ── runtime_io (dynamic registration) tests ───────────────────────────────

    #[test]
    fn register_io_dynamic_visible_via_lookup() {
        let r = Registry::new();
        r.register_io_dynamic(Arc::new(NamedIo("dynamic_tool")));
        assert!(r.lookup("dynamic_tool").is_some());
        assert!(r.lookup_io("dynamic_tool").is_some());
    }

    #[test]
    fn remove_by_prefix_removes_dynamic_tools_only() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NamedIo("static_tool")));
        r.register_io_dynamic(Arc::new(NamedIo("mcp__srv__alpha")));
        r.register_io_dynamic(Arc::new(NamedIo("mcp__srv__beta")));
        r.register_io_dynamic(Arc::new(NamedIo("other__tool")));

        let removed = r.remove_by_prefix("mcp__srv__");
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"mcp__srv__alpha".to_string()));
        assert!(removed.contains(&"mcp__srv__beta".to_string()));

        // static tool untouched
        assert!(r.lookup("static_tool").is_some());
        // other dynamic tool untouched
        assert!(r.lookup("other__tool").is_some());
        // removed tools gone
        assert!(r.lookup("mcp__srv__alpha").is_none());
        assert!(r.lookup("mcp__srv__beta").is_none());
    }

    #[test]
    fn list_includes_dynamic_tools() {
        let mut r = Registry::new();
        r.register_io(Arc::new(NamedIo("aaa_static")));
        r.register_io_dynamic(Arc::new(NamedIo("bbb_dynamic")));

        let list = r.list();
        assert!(list.contains(&"aaa_static".to_string()));
        assert!(list.contains(&"bbb_dynamic".to_string()));
        // sorted
        assert!(list.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn clone_includes_dynamic_tools() {
        let r = Registry::new();
        r.register_io_dynamic(Arc::new(NamedIo("cloned_dynamic")));
        let r2 = r.clone();
        assert!(r2.lookup("cloned_dynamic").is_some());
    }

    #[test]
    fn dynamic_tool_appears_in_len() {
        let r = Registry::new();
        assert_eq!(r.len(), 0);
        r.register_io_dynamic(Arc::new(NamedIo("dyn1")));
        assert_eq!(r.len(), 1);
        r.register_io_dynamic(Arc::new(NamedIo("dyn2")));
        assert_eq!(r.len(), 2);
    }
}
