import { useState, useEffect } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { FolderOpen, Loader2, ArrowLeft } from "lucide-react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useWorkflowStore } from "../stores/workflowStore";

export function NewTaskView() {
    const navigate = useNavigate();
    const [searchParams] = useSearchParams();
    const workflowId = searchParams.get("workflow");

    const workflows = useWorkflowStore((s) => s.workflows);
    const fetchWorkflows = useWorkflowStore((s) => s.fetchWorkflows);

    const [taskName, setTaskName] = useState("");
    const [description, setDescription] = useState("");
    const [workingDirectory, setWorkingDirectory] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // Fetch workflows if not loaded
    useEffect(() => {
        if (workflows.length === 0) {
            fetchWorkflows();
        }
    }, [workflows.length, fetchWorkflows]);

    // Redirect if no workflow specified
    useEffect(() => {
        if (!workflowId) {
            navigate("/tasks/board", { replace: true });
        }
    }, [workflowId, navigate]);

    const workflow = workflows.find((w) => w.id === workflowId);

    const handleBrowse = async () => {
        try {
            const selected = await tauriOpen({ directory: true, multiple: false });
            if (!selected) return;
            setWorkingDirectory(selected as string);
        } catch {
            // Dialog cancelled or failed — no-op
        }
    };

    const isValid = taskName.trim().length > 0 && workingDirectory.trim().length > 0;

    const handleSubmit = async () => {
        if (!isValid || !workflowId) return;
        setSubmitting(true);
        setError(null);
        try {
            const createTask = useWorkflowStore.getState().createTask;
            const taskId = await createTask(
                workflowId,
                taskName.trim(),
                workingDirectory.trim() || undefined,
                description.trim() || undefined,
            );
            navigate(`/tasks/${taskId}/detail`);
        } catch (err) {
            setError(err instanceof Error ? err.message : "Failed to create task");
            setSubmitting(false);
        }
    };

    if (!workflowId) return null;

    return (
        <div className="flex-1 flex flex-col items-center justify-start overflow-y-auto py-12 px-4">
            <div className="w-full max-w-[560px]">
                {/* Back button */}
                <button
                    type="button"
                    onClick={() => navigate(-1)}
                    className="flex items-center gap-1.5 mb-6 text-[14px] text-[var(--text-secondary)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
                >
                    <ArrowLeft size={16} />
                    Back
                </button>

                {/* Header */}
                <h1 className="text-[28px] font-bold text-[var(--text-primary)] tracking-tight mb-1">
                    Create a task
                </h1>
                {(workflow || workflowId) && (
                    <p className="text-[14px] text-[var(--text-secondary)] mb-8">
                        Using workflow <strong>{workflow?.name ?? workflowId}</strong>
                    </p>
                )}

                {/* Form */}
                <div className="flex flex-col gap-6">
                    {/* Task Name */}
                    <div>
                        <label className="block text-[15px] font-bold text-[var(--text-primary)] mb-2">
                            Name
                        </label>
                        <div className="relative flex items-center w-full">
                            <div className="absolute left-3.5 text-[15px] text-[var(--text-secondary)] font-medium pointer-events-none">
                                #
                            </div>
                            <input
                                type="text"
                                value={taskName}
                                onChange={(e) => setTaskName(e.target.value)}
                                placeholder="e.g. data-processing"
                                autoFocus
                                className="w-full h-[40px] pl-[30px] pr-[12px] bg-[var(--bg-primary)] border border-[var(--border-primary)] rounded-[10px] text-[15px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[#1164A3] focus:shadow-[0_0_0_1px_#1164A3] transition-colors"
                                required
                            />
                        </div>
                        <p className="text-[14px] text-[var(--text-secondary)] mt-2">
                            Tasks run your workflows. Give it a name so you can find it later.
                        </p>
                    </div>

                    {/* Description */}
                    <div>
                        <label className="block text-[15px] font-bold text-[var(--text-primary)] mb-2">
                            Description{" "}
                            <span className="text-[var(--text-secondary)] font-normal">(optional)</span>
                        </label>
                        <textarea
                            value={description}
                            onChange={(e) => setDescription(e.target.value)}
                            placeholder="Describe what this task should accomplish..."
                            rows={3}
                            className="w-full py-2 px-3 bg-[var(--bg-primary)] border border-[var(--border-primary)] rounded-[10px] text-[15px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[#1164A3] focus:shadow-[0_0_0_1px_#1164A3] transition-colors resize-none leading-relaxed"
                        />
                        <p className="text-[14px] text-[var(--text-secondary)] mt-2">
                            Optional context passed to the workflow.
                        </p>
                    </div>

                    {/* Working Directory */}
                    <div>
                        <label className="block text-[15px] font-bold text-[var(--text-primary)] mb-2">
                            Working Directory
                        </label>
                        <div className="flex gap-2">
                            <div className="flex-1 relative">
                                <input
                                    type="text"
                                    value={workingDirectory}
                                    onChange={(e) => setWorkingDirectory(e.target.value)}
                                    placeholder="/path/to/project"
                                    className="w-full h-[40px] px-[12px] font-mono bg-[var(--bg-primary)] border border-[var(--border-primary)] rounded-[10px] text-[14px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[#1164A3] focus:shadow-[0_0_0_1px_#1164A3] transition-colors"
                                    required
                                />
                            </div>
                            <button
                                type="button"
                                onClick={handleBrowse}
                                className="flex items-center justify-center gap-1.5 px-4 h-[40px] rounded-[10px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] text-[15px] font-bold text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                            >
                                <FolderOpen size={16} />
                                Browse
                            </button>
                        </div>
                        <p className="text-[14px] text-[var(--text-secondary)] mt-2">
                            The directory where this task will operate.
                        </p>
                    </div>

                    {/* Error */}
                    {error && (
                        <p className="text-[14px] text-[#E01E5A] font-medium">{error}</p>
                    )}

                    {/* Submit */}
                    <div className="flex justify-end pt-2">
                        <button
                            onClick={handleSubmit}
                            disabled={!isValid || submitting}
                            className="flex items-center justify-center gap-2 px-6 py-2 h-[40px] rounded-[8px] bg-[#007A5A] text-white text-[15px] font-bold hover:bg-[#148567] transition-all disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                        >
                            {submitting ? (
                                <Loader2 size={16} className="animate-spin" />
                            ) : (
                                "Create Task"
                            )}
                        </button>
                    </div>
                </div>
            </div>
        </div>
    );
}
