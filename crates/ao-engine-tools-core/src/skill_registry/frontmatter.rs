use serde::Deserialize;
use thiserror::Error;

use super::{ContextMode, SkillArgument, SkillProvenance, SkillRecord, SkillSource};

#[derive(Debug, Error, PartialEq)]
pub enum FrontmatterError {
    #[error("missing required frontmatter field: {field}")]
    MissingRequired { field: String },
    #[error("frontmatter parse error: {reason}")]
    ParseError { reason: String },
}

#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(rename = "allowed-tools", default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    arguments: Vec<RawArgument>,
    #[serde(rename = "when-to-use", default)]
    when_to_use: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "disable-model-invocation", default)]
    disable_model_invocation: bool,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    retired: bool,
    #[serde(rename = "retired-reason", default)]
    retired_reason: Option<String>,
    #[serde(rename = "superseded-by", default)]
    superseded_by: Option<String>,
    #[serde(rename = "distilled-from", default)]
    distilled_from: Vec<String>,
    #[serde(default = "default_skill_version")]
    version: u32,
}

/// Serde default for [`RawFrontmatter::version`]: a skill with no `version`
/// key predates the versioning field and has been written exactly once,
/// so it starts the counter at the same value a brand-new skill would.
fn default_skill_version() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct RawArgument {
    name: String,
    #[serde(default)]
    required: bool,
}

/// Parse YAML frontmatter from `content` and return a [`SkillRecord`].
///
/// The frontmatter must be delimited by `---` lines at the start of content.
/// The body is everything after the closing `---` line. Unknown frontmatter
/// keys are silently ignored for forward-compatibility.
///
/// The `source` field on the returned record defaults to [`SkillSource::User`];
/// the registry loader overrides it based on the actual pool.
pub fn parse_frontmatter(content: &str) -> Result<SkillRecord, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let raw: RawFrontmatter = serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
        reason: e.to_string(),
    })?;

    // `name` and `title` are aliases; `name` takes precedence.
    let name = raw.name.or(raw.title).ok_or_else(|| FrontmatterError::MissingRequired {
        field: "name".to_string(),
    })?;

    let description = raw.description.ok_or_else(|| FrontmatterError::MissingRequired {
        field: "description".to_string(),
    })?;

    let context = match raw.context.as_deref() {
        None | Some("inline") | Some("Inline") => ContextMode::Inline,
        Some("fork") | Some("Fork") => ContextMode::Fork,
        Some(other) => {
            return Err(FrontmatterError::ParseError {
                reason: format!("unknown context mode '{other}'; expected 'inline' or 'fork'"),
            })
        }
    };

    let arguments = raw
        .arguments
        .into_iter()
        .map(|a| SkillArgument { name: a.name, required: a.required })
        .collect();

    let provenance = match raw.origin.as_deref() {
        Some("distilled") => SkillProvenance::Distilled,
        _ => SkillProvenance::UserAuthored,
    };

    Ok(SkillRecord {
        name,
        description,
        context,
        agent: raw.agent,
        allowed_tools: raw.allowed_tools,
        arguments,
        body: body.to_string(),
        source: SkillSource::User,
        when_to_use: raw.when_to_use,
        model: raw.model,
        disable_model_invocation: raw.disable_model_invocation,
        provenance,
        retired: raw.retired,
        retired_reason: raw.retired_reason,
        superseded_by: raw.superseded_by,
        distilled_from: raw.distilled_from,
        version: raw.version,
    })
}

/// Force the `disable-model-invocation` frontmatter key in `content` to
/// `disable`, inserting it if absent. Every other frontmatter key and the
/// body are left untouched.
///
/// This exists for the trust gate (`crate::trust_gate`): the gate's
/// verdict on a self-authored skill has to win over whatever
/// `disable-model-invocation` value the skill's own body claims — the model
/// that authored the body is not a trustworthy judge of its own
/// invocability. Round-tripping through a generic YAML mapping (rather than
/// re-serializing a [`SkillRecord`]) means any frontmatter key this parser
/// does not model yet passes through unchanged.
pub fn set_disable_model_invocation(
    content: &str,
    disable: bool,
) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    mapping.insert(
        serde_yaml::Value::String("disable-model-invocation".to_string()),
        serde_yaml::Value::Bool(disable),
    );

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    // `serde_yaml::to_string` does not emit `---` document delimiters for a
    // bare mapping; re-frame it in the same delimiter shape
    // `split_frontmatter` parses, trimming the trailing newline it adds so
    // the closing `---` lands on its own line exactly once.
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Stamp `content`'s frontmatter with an `origin: distilled` marker,
/// inserting it if absent. Every other frontmatter key and the body are left
/// untouched.
///
/// This is the coarse provenance breadcrumb the distillation trigger leaves
/// on a skill it generalized from a repeated procedure — enough for a human
/// reviewing staged skill candidates to see at a glance that a skill was
/// machine-authored from observed behavior rather than typed by a person.
/// Pair with [`set_distilled_from`] to also record *which* observations fed
/// it (the fine-grained provenance record).
/// `origin` is an unmodeled key in [`RawFrontmatter`], so it round-trips
/// through [`parse_frontmatter`] unchanged today (silently ignored) until a
/// future field promotes it.
pub fn set_distilled_origin(content: &str) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    mapping.insert(
        serde_yaml::Value::String("origin".to_string()),
        serde_yaml::Value::String("distilled".to_string()),
    );

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Stamp `content`'s frontmatter with the fine-grained distillation
/// provenance: the `ReflectionCandidate::id` values of every candidate
/// folded into the group this skill generalizes, inserting the
/// `distilled-from` key if absent. Pairs with [`set_distilled_origin`] —
/// that call marks *that* a skill was machine-authored from observed
/// behavior; this one records *which* observations, so a human reviewing
/// staged skill candidates (or the lifecycle sweeps) can trace a distilled
/// skill back to the concrete turns it generalized. Every other frontmatter
/// key and the body are left untouched.
pub fn set_distilled_from(content: &str, candidate_ids: &[String]) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    let sequence = serde_yaml::Value::Sequence(
        candidate_ids.iter().map(|id| serde_yaml::Value::String(id.clone())).collect(),
    );
    mapping.insert(serde_yaml::Value::String("distilled-from".to_string()), sequence);

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Stamp `content`'s frontmatter with a `version` integer, inserting the
/// key if absent. Every other frontmatter key and the body are left
/// untouched.
///
/// A skill starts at version 1 when first written. This setter is the only
/// path that advances it: `SkillRegister` (`ao_engine_tools_engine::skill`)
/// calls it with `existing_version + 1` whenever a name is re-registered
/// over an existing skill, and the consolidation sweep
/// (`ao_engine_tools_engine::skill::consolidation`) calls it on the winning
/// skill of a merge — a consolidated skill absorbed another skill's
/// duplicate procedure into its own track record, so its version advances
/// even though its body is untouched.
pub fn set_version(content: &str, version: u32) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    mapping.insert(
        serde_yaml::Value::String("version".to_string()),
        serde_yaml::Value::Number(version.into()),
    );

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Stamp `content`'s frontmatter with a lifecycle retirement tombstone:
/// `retired: true` plus `retired-reason: <reason>` and, for a
/// consolidation merge, `superseded-by: <winner skill name>`. Callers always
/// pair this with `set_disable_model_invocation(content, true)` — see
/// `ao_engine_tools_engine::skill::{consolidation, retirement}` — so a
/// retired skill is both inert and durably marked *why*, distinguishing an
/// automated lifecycle decision from a skill still awaiting its first
/// human review.
pub fn set_retired(
    content: &str,
    reason: &str,
    superseded_by: Option<&str>,
) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    mapping.insert(
        serde_yaml::Value::String("retired".to_string()),
        serde_yaml::Value::Bool(true),
    );
    mapping.insert(
        serde_yaml::Value::String("retired-reason".to_string()),
        serde_yaml::Value::String(reason.to_string()),
    );
    match superseded_by {
        Some(name) => {
            mapping.insert(
                serde_yaml::Value::String("superseded-by".to_string()),
                serde_yaml::Value::String(name.to_string()),
            );
        }
        None => {
            mapping.remove("superseded-by");
        }
    }

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Clear a retirement tombstone previously set by [`set_retired`], removing
/// `retired`/`retired-reason`/`superseded-by` and restoring the frontmatter
/// to its pre-retirement shape. Does not touch `disable-model-invocation` —
/// callers pair this with `set_disable_model_invocation(content, false)` to
/// fully reactivate a retired skill (see
/// `ao_engine_tools_engine::skill::retirement::reactivate`), mirroring how
/// [`set_retired`] is always paired with setting that flag `true`.
pub fn clear_retired(content: &str) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    for key in ["retired", "retired-reason", "superseded-by"] {
        mapping.remove(key);
    }

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Stamp `content`'s frontmatter with a new `description`, inserting the key
/// if absent. Every other frontmatter key and the body are left untouched.
///
/// Exists for the skill review surface (a human editing a parked distilled
/// skill's proposed description before accepting it) — the same
/// round-trip-through-a-generic-mapping approach [`set_disable_model_invocation`]
/// uses, so any frontmatter key this parser does not model yet still passes
/// through unchanged.
pub fn set_description(content: &str, description: &str) -> Result<String, FrontmatterError> {
    let (yaml_str, body) = split_frontmatter(content)?;

    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(yaml_str).map_err(|e| FrontmatterError::ParseError {
            reason: e.to_string(),
        })?;
    mapping.insert(
        serde_yaml::Value::String("description".to_string()),
        serde_yaml::Value::String(description.to_string()),
    );

    let rewritten_yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| FrontmatterError::ParseError { reason: e.to_string() })?;
    let rewritten_yaml = rewritten_yaml.trim_end_matches('\n');

    Ok(format!("---\n{rewritten_yaml}\n---\n{body}"))
}

/// Replace `content`'s body (everything after the closing frontmatter `---`)
/// with `new_body`, leaving the frontmatter block byte-for-byte untouched.
///
/// Exists for the skill review surface (a human rewriting a parked distilled
/// skill's body before accepting it). Unlike the `set_*` frontmatter helpers
/// above, this never round-trips the YAML through `serde_yaml` — there is
/// nothing in the frontmatter to change, so re-serializing it would only
/// risk reformatting keys the human never touched.
pub fn set_body(content: &str, new_body: &str) -> Result<String, FrontmatterError> {
    let (yaml_str, _old_body) = split_frontmatter(content)?;
    Ok(format!("---\n{yaml_str}\n---\n{new_body}"))
}

/// Split `content` into `(yaml_str, body)` at the frontmatter delimiters.
///
/// Content must begin with `---\n` or `---\r\n`. The body is everything
/// after the closing `---` line (may be empty).
fn split_frontmatter(content: &str) -> Result<(&str, &str), FrontmatterError> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| FrontmatterError::ParseError {
            reason: "content does not start with a frontmatter '---' delimiter".to_string(),
        })?;

    // Prefer Unix line endings, then Windows, then no trailing newline after closing ---.
    if let Some(pos) = rest.find("\n---\n") {
        return Ok((&rest[..pos], &rest[pos + 5..]));
    }
    if let Some(pos) = rest.find("\n---\r\n") {
        return Ok((&rest[..pos], &rest[pos + 6..]));
    }
    if let Some(yaml_str) = rest.strip_suffix("\n---") {
        return Ok((yaml_str, ""));
    }

    Err(FrontmatterError::ParseError {
        reason: "missing closing frontmatter delimiter '---'".to_string(),
    })
}
