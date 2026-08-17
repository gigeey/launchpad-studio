import { useState, useEffect } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { useNavigate } from "react-router-dom";
import { X, FolderOpen, Loader2 } from "lucide-react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useWorkflowStore } from "../stores/workflowStore";
import { useTaskCreateModalStore } from "../stores/taskCreateModalStore";

export function TaskCreateModal() {
    const navigate = useNavigate();
    const { workflowId, close } = useTaskCreateModalStore();
    const workflows = useWorkflowStore((s) => s.workflows);
    const fetchWorkflows = useWorkflowStore((s) => s.fetchWorkflows);

    const [taskName, setTaskName] = useState("");
    const [description, setDescription] = useState("");
    const [workingDirectory, setWorkingDirectory] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // Fetch workflows if not loaded
    useEffect(() => {
        if (workflows.length === 0 && workflowId) {
            fetchWorkflows();
        }
    }, [workflows.length, fetchWorkflows, workflowId]);

    // Reset form when opened
    useEffect(() => {
        if (workflowId) {
            setTaskName("");
            setDescription("");
            setWorkingDirectory("");
            setSubmitting(false);
            setError(null);
        }
    }, [workflowId]);

    // Close on Escape
    useEffect(() => {
        if (!workflowId) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === "Escape" && !submitting) close();
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [workflowId, close, submitting]);

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
            close();
            useWorkflowStore.getState().fetchTasks();
            navigate(`/tasks/${taskId}/detail`);
        } catch (err) {
            setError(err instanceof Error ? err.message : "Failed to create task");
            setSubmitting(false);
        }
    };

    return (
        <AnimatePresence>
            {workflowId && (
                <div className="fixed inset-0 z-[300] flex items-center justify-center p-4">
                    {/* Backdrop */}
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        transition={{ duration: 0.15 }}
                        className="absolute inset-0 bg-black/40 backdrop-blur-[1px]"
                        onClick={() => !submitting && close()}
                    />

                    {/* Modal */}
                    <motion.div
                        initial={{ opacity: 0, scale: 0.96 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        className="relative w-full max-w-[560px] bg-[var(--modal-bg)] rounded-[8px] flex flex-col overflow-hidden"
                        style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
                    >
                        {/* Header */}
                        <div className="flex flex-col px-7 py-6 pb-2 relative">
                            <h2 className="text-[28px] font-bold text-[var(--modal-text-primary)] tracking-tight">Create a task</h2>
                            <button
                                type="button"
                                onClick={() => !submitting && close()}
                                className="absolute top-5 right-5 p-2 rounded-md text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                            >
                                <X strokeWidth={2} size={22} />
                            </button>
                        </div>

                        {/* Form Content */}
                        <div className="px-7 py-4 overflow-y-auto max-h-[calc(85vh-140px)]">
                            <div className="flex flex-col gap-6">
                                {/* Task Name */}
                                <div>
                                    <label className="block text-[15px] font-bold text-[var(--modal-text-primary)] mb-2">Name</label>
                                    <div className="relative flex items-center w-full">
                                        <div className="absolute left-3.5 text-[15px] text-[var(--modal-text-secondary)] font-medium pointer-events-none">#</div>
                                        <input
                                            type="text"
                                            value={taskName}
                                            onChange={(e) => setTaskName(e.target.value)}
                                            placeholder="e.g. data-processing"
                                            className="w-full h-[40px] pl-[30px] pr-[12px] bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[10px] text-[15px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors"
                                            required
                                        />
                                    </div>
                                    <p className="text-[14px] text-[var(--modal-text-secondary)] mt-2">
                                        Tasks run your workflows. Give it a name so you can find it later.
                                    </p>
                                </div>

                                {/* Description */}
                                <div>
                                    <label className="block text-[15px] font-bold text-[var(--modal-text-primary)] mb-2">
                                        Description <span className="text-[var(--modal-text-secondary)] font-normal">(optional)</span>
                                    </label>
                                    <textarea
                                        value={description}
                                        onChange={(e) => setDescription(e.target.value)}
                                        placeholder="Describe what this task should accomplish..."
                                        rows={3}
                                        className="w-full py-2 px-3 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[10px] text-[15px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors resize-none leading-relaxed"
                                    />
                                    <p className="text-[14px] text-[var(--modal-text-secondary)] mt-2">
                                        Optional context passed to the workflow.
                                    </p>
                                </div>

                                {/* Working Directory */}
                                <div>
                                    <label className="block text-[15px] font-bold text-[var(--modal-text-primary)] mb-2">Working Directory</label>
                                    <div className="flex gap-2">
                                        <div className="flex-1 relative">
                                            <input
                                                type="text"
                                                value={workingDirectory}
                                                onChange={(e) => setWorkingDirectory(e.target.value)}
                                                placeholder="/path/to/project"
                                                className="w-full h-[40px] px-[12px] font-mono bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[10px] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors"
                                                required
                                            />
                                        </div>
                                        <button
                                            type="button"
                                            onClick={handleBrowse}
                                            className="flex items-center justify-center gap-1.5 px-4 h-[40px] rounded-[10px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] text-[15px] font-bold text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                                        >
                                            <FolderOpen size={16} />
                                            Browse
                                        </button>
                                    </div>
                                    <p className="text-[14px] text-[var(--modal-text-secondary)] mt-2">
                                        The directory where this task will operate.
                                    </p>
                                </div>

                                {/* Error */}
                                {error && (
                                    <div className="rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] px-[10px] py-[8px] text-[14px] text-[var(--error)] font-medium">{error}</div>
                                )}
                            </div>
                        </div>

                        {/* Footer */}
                        <div className="px-7 py-6 pt-4 flex gap-3 justify-between items-center mt-auto">
                            <div className="text-[14px] text-[var(--modal-text-secondary)]">
                                {workflow ? (
                                    <span>Using workflow <strong>{workflow.name}</strong></span>
                                ) : (
                                    <span>Using workflow <strong>{workflowId}</strong></span>
                                )}
                            </div>
                            <button
                                onClick={handleSubmit}
                                disabled={!isValid || submitting}
                                className="flex flex-shrink-0 items-center justify-center gap-2 px-6 py-2 h-[40px] rounded-[8px] bg-[var(--success)] text-white text-[15px] font-bold hover:brightness-110 transition-all disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                            >
                                {submitting ? <Loader2 size={16} className="animate-spin" /> : "Next"}
                            </button>
                        </div>
                    </motion.div>
                </div>
            )}
        </AnimatePresence>
    );
}
