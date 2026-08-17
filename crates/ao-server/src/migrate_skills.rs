use std::path::Path;

use ao_protocol::agent::AgentProfile;
use ao_protocol::error::AoError;

const MIGRATED_MARKER: &str = ".migrated-v1";

/// Run the one-shot migration that moves per-agent skill directories into the
/// global user pool. Idempotent: a second call after the marker is written is
/// a no-op.
pub async fn run(data_dir: &Path) -> Result<(), AoError> {
    let skills_root = data_dir.join("skills");
    let marker = skills_root.join(MIGRATED_MARKER);

    if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
        tracing::info!("migrate-skills: .migrated-v1 marker found, nothing to do");
        return Ok(());
    }

    tokio::fs::create_dir_all(&skills_root).await?;

    let agent_homes_dir = data_dir.join("agent_homes");
    if tokio::fs::try_exists(&agent_homes_dir).await.unwrap_or(false) {
        let mut homes = tokio::fs::read_dir(&agent_homes_dir).await?;
        while let Some(entry) = homes.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let agent_id = entry.file_name().to_string_lossy().into_owned();
                if let Err(e) = migrate_agent(data_dir, &agent_id).await {
                    tracing::warn!("migrate-skills: agent {}: error during migration: {}", agent_id, e);
                }
            }
        }
    }

    tokio::fs::write(&marker, b"").await?;
    tracing::info!("migrate-skills: migration complete, .migrated-v1 marker written");
    Ok(())
}

async fn migrate_agent(data_dir: &Path, agent_id: &str) -> Result<(), AoError> {
    let agent_skills_dir = data_dir
        .join("agent_homes")
        .join(agent_id)
        .join("skills");

    if !tokio::fs::try_exists(&agent_skills_dir).await.unwrap_or(false) {
        return Ok(());
    }

    let profile_path = data_dir.join("agents").join(format!("{}.yaml", agent_id));
    if !tokio::fs::try_exists(&profile_path).await.unwrap_or(false) {
        tracing::warn!(
            "migrate-skills: agent {} has skills dir but no profile, skipping",
            agent_id
        );
        return Ok(());
    }

    let yaml = tokio::fs::read_to_string(&profile_path).await?;
    let mut profile: AgentProfile =
        serde_yaml::from_str(&yaml).map_err(|e| AoError::Yaml(e.to_string()))?;

    let agent_id_short = &agent_id[..agent_id.len().min(8)];

    let mut entries = tokio::fs::read_dir(&agent_skills_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let src = entry.path();
        let file_type = entry.file_type().await?;
        let file_name = entry.file_name().to_string_lossy().into_owned();

        if file_type.is_dir() {
            // Plugin shadow bundle: directory that itself contains a skills/ subdir.
            if tokio::fs::try_exists(src.join("skills")).await.unwrap_or(false) {
                tracing::info!(
                    "migrate-skills: agent {}: deleting plugin shadow bundle '{}'",
                    agent_id,
                    file_name
                );
                tokio::fs::remove_dir_all(&src).await?;
                continue;
            }

            // Folder skill: must contain SKILL.md.
            if !tokio::fs::try_exists(src.join("SKILL.md")).await.unwrap_or(false) {
                tracing::warn!(
                    "migrate-skills: agent {}: skipping dir '{}' (no SKILL.md)",
                    agent_id,
                    file_name
                );
                continue;
            }

            let canonical =
                copy_folder_skill(&src, &file_name, agent_id_short, data_dir).await?;
            tracing::info!(
                "migrate-skills: agent {}: migrated folder skill '{}' -> '{}'",
                agent_id,
                file_name,
                canonical
            );
            add_skill_to_profile(&mut profile, &canonical);
            tokio::fs::remove_dir_all(&src).await?;
        } else if file_type.is_file() && file_name.ends_with(".md") {
            let name = file_name.trim_end_matches(".md").to_string();
            let canonical = copy_flat_skill(&src, &name, agent_id_short, data_dir).await?;
            tracing::info!(
                "migrate-skills: agent {}: migrated flat skill '{}' -> '{}'",
                agent_id,
                file_name,
                canonical
            );
            add_skill_to_profile(&mut profile, &canonical);
            tokio::fs::remove_file(&src).await?;
        }
    }

    let updated_yaml =
        serde_yaml::to_string(&profile).map_err(|e| AoError::Yaml(e.to_string()))?;
    tokio::fs::write(&profile_path, updated_yaml).await?;

    Ok(())
}

fn add_skill_to_profile(profile: &mut AgentProfile, name: &str) {
    if !profile.skills.iter().any(|s| s == name) {
        profile.skills.push(name.to_string());
    }
}

async fn copy_folder_skill(
    src: &Path,
    name: &str,
    agent_id_short: &str,
    data_dir: &Path,
) -> Result<String, AoError> {
    let dest = data_dir.join("skills").join(name);
    if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        let renamed = format!("{}-from-{}", name, agent_id_short);
        let dest_renamed = data_dir.join("skills").join(&renamed);
        tracing::info!(
            "migrate-skills: collision for '{}', renaming to '{}'",
            name,
            renamed
        );
        copy_dir_recursive(src, &dest_renamed).await?;
        return Ok(renamed);
    }
    copy_dir_recursive(src, &dest).await?;
    Ok(name.to_string())
}

async fn copy_flat_skill(
    src: &Path,
    name: &str,
    agent_id_short: &str,
    data_dir: &Path,
) -> Result<String, AoError> {
    let dest_dir = data_dir.join("skills").join(name);
    if tokio::fs::try_exists(&dest_dir).await.unwrap_or(false) {
        let renamed = format!("{}-from-{}", name, agent_id_short);
        let dest_dir_renamed = data_dir.join("skills").join(&renamed);
        tracing::info!(
            "migrate-skills: collision for '{}', renaming to '{}'",
            name,
            renamed
        );
        tokio::fs::create_dir_all(&dest_dir_renamed).await?;
        let content = tokio::fs::read(src).await?;
        tokio::fs::write(dest_dir_renamed.join("SKILL.md"), content).await?;
        return Ok(renamed);
    }
    tokio::fs::create_dir_all(&dest_dir).await?;
    let content = tokio::fs::read(src).await?;
    tokio::fs::write(dest_dir.join("SKILL.md"), content).await?;
    Ok(name.to_string())
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), AoError> {
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || copy_dir_recursive_sync(&src, &dst))
        .await
        .map_err(|e| AoError::Internal(format!("join error in migration: {}", e)))?
        .map_err(AoError::Io)
}

fn copy_dir_recursive_sync(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_sync(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(base: &Path, rel: &str, content: &str) {
        let path = base.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn write_profile(data_dir: &Path, agent_id: &str) {
        let yaml = format!(
            "id: {id}\nname: Agent {id}\ndescription: test agent\nprovider:\n  type: Cli\n  command: claude\nmodel: null\n",
            id = agent_id
        );
        std::fs::create_dir_all(data_dir.join("agents")).unwrap();
        std::fs::write(
            data_dir.join("agents").join(format!("{}.yaml", agent_id)),
            yaml,
        )
        .unwrap();
    }

    fn read_profile(data_dir: &Path, agent_id: &str) -> AgentProfile {
        let yaml = std::fs::read_to_string(
            data_dir.join("agents").join(format!("{}.yaml", agent_id)),
        )
        .unwrap();
        serde_yaml::from_str(&yaml).unwrap()
    }

    const SKILL_BODY: &str = "---\nname: my-skill\ndescription: A test skill\n---\nDo the thing.\n";
    const SKILL_BODY2: &str = "---\nname: other-skill\ndescription: Another test skill\n---\nDo the other thing.\n";

    #[tokio::test]
    async fn migrates_folder_skill() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let agent_a = "agent-a";
        write_profile(data_dir, agent_a);

        write_file(
            data_dir,
            &format!("agent_homes/{}/skills/my-skill/SKILL.md", agent_a),
            SKILL_BODY,
        );

        run(data_dir).await.unwrap();

        assert!(data_dir.join("skills/my-skill/SKILL.md").exists());
        assert!(!data_dir
            .join(format!("agent_homes/{}/skills/my-skill", agent_a))
            .exists());
        let p = read_profile(data_dir, agent_a);
        assert!(p.skills.contains(&"my-skill".to_string()));
        assert!(data_dir.join("skills/.migrated-v1").exists());
    }

    #[tokio::test]
    async fn migrates_flat_skill() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let agent_a = "agent-b";
        write_profile(data_dir, agent_a);

        write_file(
            data_dir,
            &format!("agent_homes/{}/skills/flat-skill.md", agent_a),
            SKILL_BODY2,
        );

        run(data_dir).await.unwrap();

        assert!(data_dir.join("skills/flat-skill/SKILL.md").exists());
        assert!(!data_dir
            .join(format!("agent_homes/{}/skills/flat-skill.md", agent_a))
            .exists());
        let p = read_profile(data_dir, agent_a);
        assert!(p.skills.contains(&"flat-skill".to_string()));
    }

    #[tokio::test]
    async fn collision_renames_with_agent_suffix() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let agent_a = "agent-ccc";
        write_profile(data_dir, agent_a);
        write_file(
            data_dir,
            &format!("agent_homes/{}/skills/shared-skill/SKILL.md", agent_a),
            SKILL_BODY,
        );

        // Pre-populate user pool to trigger collision
        std::fs::create_dir_all(data_dir.join("skills/shared-skill")).unwrap();
        std::fs::write(data_dir.join("skills/shared-skill/SKILL.md"), SKILL_BODY2).unwrap();

        run(data_dir).await.unwrap();

        assert!(data_dir.join("skills/shared-skill/SKILL.md").exists());
        let short = &agent_a[..agent_a.len().min(8)];
        let renamed = format!("skills/shared-skill-from-{}/SKILL.md", short);
        assert!(data_dir.join(&renamed).exists(), "expected {}", renamed);
        let p = read_profile(data_dir, agent_a);
        assert!(p.skills.contains(&format!("shared-skill-from-{}", short)));
    }

    #[tokio::test]
    async fn deletes_plugin_shadow_bundle() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let agent_a = "agent-d";
        write_profile(data_dir, agent_a);

        let shadow = data_dir.join(format!("agent_homes/{}/skills/my-plugin", agent_a));
        std::fs::create_dir_all(shadow.join("skills")).unwrap();
        std::fs::write(shadow.join("skills").join("README.md"), "plugin skills").unwrap();

        run(data_dir).await.unwrap();

        assert!(!shadow.exists());
        assert!(!data_dir.join("skills/my-plugin").exists());
        let p = read_profile(data_dir, agent_a);
        assert!(p.skills.is_empty());
    }

    #[tokio::test]
    async fn idempotent_second_run() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let agent_a = "agent-e";
        write_profile(data_dir, agent_a);
        write_file(
            data_dir,
            &format!("agent_homes/{}/skills/my-skill/SKILL.md", agent_a),
            SKILL_BODY,
        );

        run(data_dir).await.unwrap();
        assert!(data_dir.join("skills/.migrated-v1").exists());

        // Second run: no-op
        run(data_dir).await.unwrap();
        assert!(data_dir.join("skills/my-skill/SKILL.md").exists());
    }
}
