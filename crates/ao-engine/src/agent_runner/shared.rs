use ao_protocol::attachment::{Attachment, AttachmentType, FileCapability, ImageMode};
use ao_protocol::memory::MemoryEntry;
use ao_protocol::agent::WorkflowBinding;
use ao_protocol::workflow::{WorkflowDefinition, WorkflowSummary};

/// Build a formatted memory block from agent, project, and global memories.
/// Returns None if all three lists are empty.
pub fn build_memory_block(
    agent_memories: &[MemoryEntry],
    project_memories: &[MemoryEntry],
    global_memories: &[MemoryEntry],
) -> Option<String> {
    if agent_memories.is_empty() && project_memories.is_empty() && global_memories.is_empty() {
        return None;
    }

    let mut sections: Vec<String> = Vec::new();

    for (label, entries) in [
        ("[Agent Memories]", agent_memories),
        ("[Project Memories]", project_memories),
        ("[Global Memories]", global_memories),
    ] {
        if entries.is_empty() {
            continue;
        }
        let mut section = String::from(label);
        for m in entries {
            section.push_str(&format!("\n- {}", m.content));
        }
        sections.push(section);
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Build a workflow context block based on the agent's WorkflowBinding.
///
/// - `All` binding: produces a summary table of all workflows (generalist mode).
/// - `List(ids)` binding: produces full definitions for the specified workflows (specialist mode).
/// - `None` or absent: returns None.
pub fn build_workflow_block(
    binding: &Option<WorkflowBinding>,
    summaries: &[&WorkflowSummary],
    definitions: &[&WorkflowDefinition],
) -> Option<String> {
    let binding = binding.as_ref()?;

    match binding {
        WorkflowBinding::All => {
            if summaries.is_empty() {
                return None;
            }
            let mut block = String::from("## Available Workflows\n\n");
            block.push_str("| ID | Name | Description |\n");
            block.push_str("|---|---|---|\n");
            for s in summaries {
                let desc = s.description.as_deref().unwrap_or("");
                block.push_str(&format!("| {} | {} | {} |\n", s.id, s.name, desc));
            }
            block.push_str("\nTo read a workflow's full definition, read the workflow.yaml file from the workflows directory.");
            Some(block)
        }
        WorkflowBinding::List(ids) => {
            if ids.is_empty() {
                return None;
            }
            let mut block = String::from("## Workflows\n\n");
            for def in definitions {
                if !ids.contains(&def.id) {
                    continue;
                }
                block.push_str(&format!("### {} ({})\n", def.name, def.id));
                if let Some(ref desc) = def.description {
                    block.push_str(&format!("{}\n", desc));
                }
                if let Some(ref version) = def.version {
                    block.push_str(&format!("Version: {}\n", version));
                }
                block.push_str(&format!("Phases ({}):\n", def.phases.len()));
                for (i, phase) in def.phases.iter().enumerate() {
                    block.push_str(&format!(
                        "  {}. **{}** (`{}`)",
                        i + 1,
                        phase.name,
                        phase.id
                    ));
                    if let Some(ref intent) = phase.intent {
                        block.push_str(&format!(" — {}", intent));
                    }
                    block.push('\n');
                    if !phase.inputs.is_empty() {
                        block.push_str("     Inputs:");
                        for input in &phase.inputs {
                            block.push_str(&format!(
                                " {}",
                                input.id
                            ));
                            if let Some(ref fp) = input.from_phase {
                                block.push_str(&format!(" (from {}", fp));
                                if let Some(ref fo) = input.from_output {
                                    block.push_str(&format!(".{}", fo));
                                }
                                block.push(')');
                            }
                            block.push(',');
                        }
                        block.push('\n');
                    }
                    if !phase.outputs.is_empty() {
                        block.push_str("     Outputs:");
                        for output in &phase.outputs {
                            block.push_str(&format!(" {}", output.id));
                            if let Some(ref desc) = output.description {
                                block.push_str(&format!(" ({})", desc));
                            }
                            block.push(',');
                        }
                        block.push('\n');
                    }
                }
                block.push('\n');
            }
            Some(block)
        }
        WorkflowBinding::None => None,
    }
}

/// Augment a user prompt with file/image/folder attachment references using the FileReference strategy.
///
/// If the agent has configured `file_capabilities.image_mode`, uses the instruction_template from there.
/// Otherwise, uses default templates per attachment type.
///
/// Returns the original prompt unchanged if there are no attachments.
pub fn augment_prompt_with_attachments(
    prompt: &str,
    attachments: &[Attachment],
    file_capabilities: Option<&FileCapability>,
) -> String {
    if attachments.is_empty() {
        return prompt.to_string();
    }

    // Extract the custom template from file_capabilities if present
    let custom_template = file_capabilities.and_then(|fc| match &fc.image_mode {
        ImageMode::FileReference {
            instruction_template,
        } => {
            if instruction_template.is_empty() {
                None
            } else {
                Some(instruction_template.as_str())
            }
        }
    });

    let mut references: Vec<String> = Vec::new();
    for attachment in attachments {
        let reference = if let Some(template) = custom_template {
            template
                .replace("{path}", &attachment.file_path)
                .replace("{mime_type}", &attachment.mime_type)
                .replace("{filename}", &attachment.original_filename)
        } else {
            // Use default templates based on attachment type
            match attachment.attachment_type {
                AttachmentType::Image => {
                    format!(
                        "[Attached image: {}]\nPlease view and analyze this image.",
                        attachment.file_path
                    )
                }
                AttachmentType::Folder => {
                    format!(
                        "[Attached folder: {}]\nPlease explore this directory and work with its contents.",
                        attachment.file_path
                    )
                }
                _ => {
                    format!(
                        "[Attached file ({}): {}]\nPlease read and analyze this file.",
                        attachment.mime_type, attachment.file_path
                    )
                }
            }
        };
        references.push(reference);
    }

    format!("{}\n\n{}", references.join("\n"), prompt)
}
