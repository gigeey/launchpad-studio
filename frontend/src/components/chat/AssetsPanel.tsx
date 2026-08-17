import { useEffect, useState, useCallback, useRef, useMemo } from "react";
import {
  Loader2,
  Trash2,
  Paperclip,
  ImageOff,
  AlertTriangle,
  Download,
  HardDrive,
  Box,
  Code2,
} from "lucide-react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { Artifact, ArtifactKind, Attachment } from "../../types/api";
import * as api from "../../lib/api";
import { getAttachmentIcon, truncateFilename } from "./AttachmentPill";
import { useMediaPreviewStore } from "../../stores/mediaPreviewStore";
import { useArtifactStore } from "../../stores/artifactStore";
import { ArtifactPreview } from "../artifacts/ArtifactRenderer";
import { openArtifactWindow } from "../../lib/windows";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

/** Label shown on an artifact row. `"html"` reads as "HTML"; every other
 *  `ArtifactKind` (including the forward-compat `"unknown"` catch-all)
 *  title-cases. */
function artifactKindLabel(kind: ArtifactKind): string {
  if (kind === "html") return "HTML";
  return kind.charAt(0).toUpperCase() + kind.slice(1);
}

/** `html` artifacts get the code-brackets glyph; every typed renderer (and
 *  the unknown-kind fallback) shares the generic box glyph — this is a
 *  list-row icon, not the renderer registry, so it doesn't need one icon
 *  per typed kind. */
function artifactKindIcon(kind: ArtifactKind) {
  return kind === "html" ? Code2 : Box;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

interface AssetsPanelProps {
  agentId: string;
}

const ONE_GB = 1024 * 1024 * 1024;

export function AssetsPanel({ agentId }: AssetsPanelProps) {
  const [assets, setAssets] = useState<Attachment[]>([]);
  const [loading, setLoading] = useState(true);
  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);
  const [deleteAllConfirm, setDeleteAllConfirm] = useState(false);
  const [deleting, setDeleting] = useState<Set<string>>(new Set());
  const [globalStorageBytes, setGlobalStorageBytes] = useState<number | null>(null);
  const [cleaningUp, setCleaningUp] = useState(false);

  // Artifacts — a distinct record from Attachment, backed
  // by its own per-agent cache (`artifactStore.ts`) shared with the thread
  // bubble's inline lookup, rather than fetched locally like attachments.
  const artifacts = useArtifactStore((s) => s.byAgent.get(agentId)?.artifacts ?? []);
  const loadArtifacts = useArtifactStore((s) => s.loadArtifacts);
  const [selectedArtifactId, setSelectedArtifactId] = useState<string | null>(null);

  useEffect(() => {
    loadArtifacts(agentId);
  }, [agentId, loadArtifacts]);

  const fetchAssets = useCallback(async () => {
    setLoading(true);
    try {
      const list = await api.listAttachments(agentId);
      setAssets(list);
    } catch {
      // silently fail — user sees empty state
    } finally {
      setLoading(false);
    }
  }, [agentId]);

  // Fetch global storage info to check against 1GB threshold
  useEffect(() => {
    api.getStorageInfo().then((info) => {
      setGlobalStorageBytes(info.total_size_bytes);
    }).catch(() => {});
  }, [agentId]);

  const handleCleanup = async () => {
    setCleaningUp(true);
    try {
      await api.triggerCleanup();
      // Refresh assets and storage info
      await fetchAssets();
      const info = await api.getStorageInfo();
      setGlobalStorageBytes(info.total_size_bytes);
    } catch {
      // silently fail
    } finally {
      setCleaningUp(false);
    }
  };

  useEffect(() => {
    fetchAssets();
  }, [fetchAssets]);

  const totalSize = assets.reduce((sum, a) => sum + a.size_bytes, 0);
  const images = assets.filter((a) => a.attachment_type === "image");
  const nonImages = assets.filter((a) => a.attachment_type !== "image");

  const handleDelete = async (asset: Attachment) => {
    setDeleting((prev) => new Set(prev).add(asset.id));
    try {
      await api.deleteAttachment(agentId, asset.id);
      setAssets((prev) => prev.filter((a) => a.id !== asset.id));
    } catch {
      // silently fail
    } finally {
      setDeleting((prev) => {
        const next = new Set(prev);
        next.delete(asset.id);
        return next;
      });
      setDeleteConfirm(null);
    }
  };

  const handleDeleteAll = async () => {
    const ids = assets.map((a) => a.id);
    setDeleting(new Set(ids));
    try {
      await Promise.allSettled(
        ids.map((id) => api.deleteAttachment(agentId, id)),
      );
      setAssets([]);
    } finally {
      setDeleting(new Set());
      setDeleteAllConfirm(false);
    }
  };

  const handleImageClick = (asset: Attachment) => {
    const imageList = images.map((img) => ({
      content: api.getAttachmentUrl(agentId, img.id),
      contentType: "image" as const,
      filename: img.original_filename,
    }));
    const currentIndex = images.findIndex((img) => img.id === asset.id);
    useMediaPreviewStore.getState().openPreview({
      content: api.getAttachmentUrl(agentId, asset.id),
      contentType: "image",
      filename: asset.original_filename,
      imageList,
      currentIndex: currentIndex >= 0 ? currentIndex : 0,
    });
  };

  const handleDownload = (asset: Attachment) => {
    const a = document.createElement("a");
    a.href = api.getAttachmentUrl(agentId, asset.id);
    a.download = asset.original_filename;
    a.click();
  };

  return (
    <div className="relative flex flex-col h-full overflow-hidden">
      {/* Header with storage info */}
      <div className="px-[16px] pt-[16px] pb-[8px]">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-[8px]">
            <span className="text-[14px] font-semibold text-[var(--text-primary)]">
              Assets
            </span>
            {assets.length > 0 && (
              <span className="text-[11px] font-bold text-[var(--text-secondary)] bg-[var(--bg-hover)] px-[6px] py-[1px] rounded-[4px]">
                {assets.length}
              </span>
            )}
          </div>
          {assets.length > 0 && (
            <span className="text-[11px] text-[var(--text-tertiary)]">
              {formatFileSize(totalSize)}
            </span>
          )}
        </div>
      </div>

      {/* Storage warning banner */}
      {globalStorageBytes !== null && globalStorageBytes > ONE_GB && (
        <div className="mx-[16px] mb-[8px] p-[10px] rounded-[8px] bg-amber-500/10 border border-amber-500/20 flex items-start gap-[8px]">
          <HardDrive className="w-[14px] h-[14px] text-amber-500 flex-shrink-0 mt-[1px]" />
          <div className="flex-1 min-w-0">
            <div className="text-[12px] text-amber-600 dark:text-amber-400 font-medium">
              Storage usage: {formatFileSize(globalStorageBytes)}
            </div>
            <div className="text-[11px] text-[var(--text-tertiary)] mt-[2px]">
              Total storage exceeds 1 GB. Consider cleaning up unused files.
            </div>
          </div>
          <button
            onClick={handleCleanup}
            disabled={cleaningUp}
            className="text-[11px] px-[8px] py-[3px] rounded-[6px] bg-amber-500/15 text-amber-600 dark:text-amber-400 hover:bg-amber-500/25 transition-colors cursor-pointer flex-shrink-0 disabled:opacity-50"
          >
            {cleaningUp ? (
              <Loader2 className="w-[12px] h-[12px] animate-spin" />
            ) : (
              "Clean up"
            )}
          </button>
        </div>
      )}

      {/* Content */}
      <VirtualizedAssetList
        loading={loading}
        images={images}
        nonImages={nonImages}
        artifacts={artifacts}
        agentId={agentId}
        deleting={deleting}
        deleteConfirm={deleteConfirm}
        onRequestDelete={(id) => setDeleteConfirm(id)}
        onConfirmDelete={handleDelete}
        onCancelDelete={() => setDeleteConfirm(null)}
        onImageClick={handleImageClick}
        onDownload={handleDownload}
        onArtifactClick={(artifact) => setSelectedArtifactId(artifact.id)}
      />

      {/* Delete All button at bottom */}
      {!loading && assets.length > 0 && (
        <div className="px-[16px] py-[12px] border-t border-[var(--border-secondary)]">
          {deleteAllConfirm ? (
            <div className="flex items-center gap-[8px]">
              <AlertTriangle className="w-[14px] h-[14px] text-[var(--error)] flex-shrink-0" />
              <span className="text-[12px] text-[var(--text-secondary)] flex-1">
                Delete all {assets.length} files?
              </span>
              <button
                onClick={() => setDeleteAllConfirm(false)}
                className="text-[12px] px-[8px] py-[4px] rounded-[6px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                Cancel
              </button>
              <button
                onClick={handleDeleteAll}
                className="text-[12px] px-[8px] py-[4px] rounded-[6px] bg-[var(--error)] text-white hover:opacity-90 transition-colors cursor-pointer"
              >
                Delete all
              </button>
            </div>
          ) : (
            <button
              onClick={() => setDeleteAllConfirm(true)}
              className="w-full text-[12px] py-[6px] rounded-[8px] text-[var(--text-secondary)] hover:text-[var(--error)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer flex items-center justify-center gap-[6px]"
            >
              <Trash2 className="w-[12px] h-[12px]" />
              Delete all assets
            </button>
          )}
        </div>
      )}

      {/* Artifact detail overlay — fills this panel via
         the `relative` root above, same absolute-overlay shape the image
         preview uses via `useMediaPreviewStore`, but scoped locally here
         instead of a global store since only this panel opens it. Reuses
         the shared renderer verbatim; `onPopOut` wires the pop-out trigger
         into its header. */}
      <ArtifactPreview
        agentId={agentId}
        artifactId={selectedArtifactId}
        onClose={() => setSelectedArtifactId(null)}
        onPopOut={(aId, artId) => {
          openArtifactWindow(aId, artId);
        }}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Virtualized asset list
// ---------------------------------------------------------------------------

type VRow =
  | { type: "image-header" }
  | { type: "image-row"; assets: Attachment[] }
  | { type: "file-header" }
  | { type: "file"; asset: Attachment }
  | { type: "artifact-header" }
  | { type: "artifact"; artifact: Artifact };

const IMAGE_ROW_HEIGHT = 130; // estimate – dynamic measurement overrides
const IMAGE_HEADER_HEIGHT = 30;
const FILE_HEADER_HEIGHT = 38; // includes top margin when after images
const FILE_ROW_HEIGHT = 48;
const ARTIFACT_HEADER_HEIGHT = 38; // includes top margin when after files/images
const ARTIFACT_ROW_HEIGHT = 52;

function VirtualizedAssetList({
  loading,
  images,
  nonImages,
  artifacts,
  agentId,
  deleting,
  deleteConfirm,
  onRequestDelete,
  onConfirmDelete,
  onCancelDelete,
  onImageClick,
  onDownload,
  onArtifactClick,
}: {
  loading: boolean;
  images: Attachment[];
  nonImages: Attachment[];
  artifacts: Artifact[];
  agentId: string;
  deleting: Set<string>;
  deleteConfirm: string | null;
  onRequestDelete: (id: string) => void;
  onConfirmDelete: (asset: Attachment) => void;
  onCancelDelete: () => void;
  onImageClick: (asset: Attachment) => void;
  onDownload: (asset: Attachment) => void;
  onArtifactClick: (artifact: Artifact) => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);

  const rows = useMemo<VRow[]>(() => {
    const r: VRow[] = [];
    if (images.length > 0) {
      r.push({ type: "image-header" });
      // chunk images into rows of 3
      for (let i = 0; i < images.length; i += 3) {
        r.push({ type: "image-row", assets: images.slice(i, i + 3) });
      }
    }
    if (nonImages.length > 0) {
      r.push({ type: "file-header" });
      for (const asset of nonImages) {
        r.push({ type: "file", asset });
      }
    }
    if (artifacts.length > 0) {
      r.push({ type: "artifact-header" });
      for (const artifact of artifacts) {
        r.push({ type: "artifact", artifact });
      }
    }
    return r;
  }, [images, nonImages, artifacts]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (index) => {
      const row = rows[index];
      switch (row.type) {
        case "image-header":
          return IMAGE_HEADER_HEIGHT;
        case "image-row":
          return IMAGE_ROW_HEIGHT;
        case "file-header":
          return FILE_HEADER_HEIGHT;
        case "file":
          return FILE_ROW_HEIGHT;
        case "artifact-header":
          return ARTIFACT_HEADER_HEIGHT;
        case "artifact":
          return ARTIFACT_ROW_HEIGHT;
      }
    },
    overscan: 5,
    measureElement: (el) => el.getBoundingClientRect().height,
  });

  if (loading) {
    return (
      <div className="flex-1 flex items-center justify-center py-[48px]">
        <Loader2 className="w-[20px] h-[20px] text-[var(--text-secondary)] animate-spin" />
      </div>
    );
  }

  if (images.length === 0 && nonImages.length === 0 && artifacts.length === 0) {
    return (
      <div className="flex-1 py-[48px] text-center text-[13px] text-[var(--text-secondary)] leading-relaxed flex flex-col items-center justify-center gap-3">
        <Paperclip className="w-[48px] h-[48px] text-[var(--text-tertiary)]" />
        <span>No files uploaded yet</span>
      </div>
    );
  }

  return (
    <div
      ref={scrollRef}
      className="flex-1 overflow-y-auto px-[16px] py-[8px] custom-scrollbar"
    >
      <div
        style={{
          height: virtualizer.getTotalSize(),
          width: "100%",
          position: "relative",
        }}
      >
        {virtualizer.getVirtualItems().map((vItem) => {
          const row = rows[vItem.index];
          return (
            <div
              key={vItem.index}
              data-index={vItem.index}
              ref={virtualizer.measureElement}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${vItem.start}px)`,
              }}
            >
              {row.type === "image-header" && (
                <div className="text-[11px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider pb-[8px]">
                  Images
                </div>
              )}
              {row.type === "image-row" && (
                <div className="grid grid-cols-3 gap-[10px] pb-[10px]">
                  {row.assets.map((asset) => (
                    <ImageAssetItem
                      key={asset.id}
                      asset={asset}
                      agentId={agentId}
                      isDeleting={deleting.has(asset.id)}
                      showConfirm={deleteConfirm === asset.id}
                      onRequestDelete={() => onRequestDelete(asset.id)}
                      onConfirmDelete={() => onConfirmDelete(asset)}
                      onCancelDelete={onCancelDelete}
                      onClick={() => onImageClick(asset)}
                    />
                  ))}
                </div>
              )}
              {row.type === "file-header" && (
                <div className="text-[11px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider pt-[8px] pb-[8px]">
                  Files
                </div>
              )}
              {row.type === "file" && (
                <FileAssetItem
                  asset={row.asset}
                  isDeleting={deleting.has(row.asset.id)}
                  showConfirm={deleteConfirm === row.asset.id}
                  onRequestDelete={() => onRequestDelete(row.asset.id)}
                  onConfirmDelete={() => onConfirmDelete(row.asset)}
                  onCancelDelete={onCancelDelete}
                  onDownload={() => onDownload(row.asset)}
                />
              )}
              {row.type === "artifact-header" && (
                <div className="text-[11px] font-semibold text-[var(--text-tertiary)] uppercase tracking-wider pt-[8px] pb-[8px]">
                  Artifacts
                </div>
              )}
              {row.type === "artifact" && (
                <ArtifactAssetItem
                  artifact={row.artifact}
                  onClick={() => onArtifactClick(row.artifact)}
                />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Image asset item (grid thumbnail)
// ---------------------------------------------------------------------------

function ImageAssetItem({
  asset,
  agentId,
  isDeleting,
  showConfirm,
  onRequestDelete,
  onConfirmDelete,
  onCancelDelete,
  onClick,
}: {
  asset: Attachment;
  agentId: string;
  isDeleting: boolean;
  showConfirm: boolean;
  onRequestDelete: () => void;
  onConfirmDelete: () => void;
  onCancelDelete: () => void;
  onClick: () => void;
}) {
  const [imgError, setImgError] = useState(false);
  const [imgLoaded, setImgLoaded] = useState(false);

  return (
    <div className="relative group aspect-square">
      {showConfirm ? (
        <div className="absolute inset-0 rounded-[8px] bg-[var(--bg-tertiary)] border border-[var(--error)]/30 flex flex-col items-center justify-center gap-[4px] z-10 p-[4px]">
          <AlertTriangle className="w-[14px] h-[14px] text-[var(--error)]" />
          <span className="text-[10px] text-[var(--text-secondary)] text-center leading-tight">
            Delete?
          </span>
          <div className="flex gap-[4px]">
            <button
              onClick={onCancelDelete}
              className="text-[10px] px-[6px] py-[2px] rounded-[4px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] cursor-pointer"
            >
              No
            </button>
            <button
              onClick={onConfirmDelete}
              className="text-[10px] px-[6px] py-[2px] rounded-[4px] bg-[var(--error)] text-white cursor-pointer"
            >
              Yes
            </button>
          </div>
        </div>
      ) : (
        <>
          <div
            className="w-full h-full rounded-[8px] overflow-hidden bg-[var(--bg-tertiary)] cursor-pointer"
            onClick={onClick}
          >
            {imgError ? (
              <div className="w-full h-full flex items-center justify-center">
                <ImageOff className="w-[20px] h-[20px] text-[var(--text-tertiary)]" />
              </div>
            ) : (
              <>
                {!imgLoaded && (
                  <div className="absolute inset-0 flex items-center justify-center">
                    <Loader2 className="w-[16px] h-[16px] text-[var(--text-tertiary)] animate-spin" />
                  </div>
                )}
                <img
                  src={api.getAttachmentUrl(agentId, asset.id)}
                  alt={asset.original_filename}
                  className={`w-full h-full object-cover transition-opacity ${imgLoaded ? "opacity-100" : "opacity-0"}`}
                  onLoad={() => setImgLoaded(true)}
                  onError={() => setImgError(true)}
                />
              </>
            )}
          </div>

          {/* Delete button overlay */}
          <button
            onClick={(e) => {
              e.stopPropagation();
              onRequestDelete();
            }}
            className="absolute top-[4px] right-[4px] w-[22px] h-[22px] rounded-[6px] bg-black/50 flex items-center justify-center text-white opacity-0 group-hover:opacity-100 transition-opacity cursor-pointer"
          >
            {isDeleting ? (
              <Loader2 className="w-[12px] h-[12px] animate-spin" />
            ) : (
              <Trash2 className="w-[12px] h-[12px]" />
            )}
          </button>

          {/* Filename tooltip on hover */}
          <div className="absolute bottom-0 left-0 right-0 px-[4px] py-[2px] bg-black/60 text-white text-[10px] truncate rounded-b-[8px] opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none">
            {truncateFilename(asset.original_filename, 25)}
          </div>
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// File asset item (list row)
// ---------------------------------------------------------------------------

function FileAssetItem({
  asset,
  isDeleting,
  showConfirm,
  onRequestDelete,
  onConfirmDelete,
  onCancelDelete,
  onDownload,
}: {
  asset: Attachment;
  isDeleting: boolean;
  showConfirm: boolean;
  onRequestDelete: () => void;
  onConfirmDelete: () => void;
  onCancelDelete: () => void;
  onDownload: () => void;
}) {
  const Icon = getAttachmentIcon(asset.attachment_type);
  const isFolder = asset.attachment_type === "folder";

  if (showConfirm) {
    return (
      <div className="flex items-center gap-[8px] p-[8px] rounded-[8px] bg-[var(--bg-tertiary)] border border-[var(--error)]/30">
        <AlertTriangle className="w-[14px] h-[14px] text-[var(--error)] flex-shrink-0" />
        <span className="text-[12px] text-[var(--text-secondary)] flex-1">
          Delete this file?
        </span>
        <button
          onClick={onCancelDelete}
          className="text-[11px] px-[6px] py-[2px] rounded-[4px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] cursor-pointer"
        >
          Cancel
        </button>
        <button
          onClick={onConfirmDelete}
          className="text-[11px] px-[6px] py-[2px] rounded-[4px] bg-[var(--error)] text-white cursor-pointer"
        >
          Delete
        </button>
      </div>
    );
  }

  return (
    <div className="group flex items-center gap-[8px] p-[8px] rounded-[8px] hover:bg-[var(--bg-tertiary)] transition-colors">
      {/* Icon */}
      <div className="w-[32px] h-[32px] rounded-[8px] bg-[var(--bg-tertiary)] flex items-center justify-center flex-shrink-0">
        <Icon className="w-[16px] h-[16px] text-[var(--text-secondary)]" />
      </div>

      {/* Info */}
      <div
        className={`flex-1 min-w-0 ${isFolder ? "" : "cursor-pointer"}`}
        onClick={isFolder ? undefined : onDownload}
        title={isFolder ? asset.file_path : asset.original_filename}
      >
        <div className="text-[13px] text-[var(--text-primary)] truncate leading-tight">
          {truncateFilename(asset.original_filename, 28)}
        </div>
        <div className="text-[11px] text-[var(--text-tertiary)] leading-tight mt-[2px]">
          {formatFileSize(asset.size_bytes)}
          {isFolder && (
            <span className="ml-[4px] text-[var(--text-tertiary)]">
              (folder)
            </span>
          )}
        </div>
      </div>

      {/* Actions */}
      <div className="flex items-center gap-[2px] opacity-0 group-hover:opacity-100 transition-opacity flex-shrink-0">
        {!isFolder && (
          <button
            onClick={onDownload}
            className="w-[24px] h-[24px] rounded-[6px] flex items-center justify-center text-[var(--text-tertiary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
            title="Download"
          >
            <Download className="w-[14px] h-[14px]" />
          </button>
        )}
        <button
          onClick={onRequestDelete}
          className="w-[24px] h-[24px] rounded-[6px] flex items-center justify-center text-[var(--text-tertiary)] hover:text-[var(--error)] transition-colors cursor-pointer"
          title="Delete"
        >
          {isDeleting ? (
            <Loader2 className="w-[14px] h-[14px] animate-spin" />
          ) : (
            <Trash2 className="w-[14px] h-[14px]" />
          )}
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Artifact asset item (list row)
// ---------------------------------------------------------------------------

function ArtifactAssetItem({
  artifact,
  onClick,
}: {
  artifact: Artifact;
  onClick: () => void;
}) {
  const Icon = artifactKindIcon(artifact.kind);

  return (
    <div
      className="group flex items-center gap-[8px] p-[8px] rounded-[8px] hover:bg-[var(--bg-tertiary)] transition-colors cursor-pointer"
      onClick={onClick}
      title={artifact.title}
      data-testid="artifact-asset-item"
    >
      {/* Icon */}
      <div className="w-[32px] h-[32px] rounded-[8px] bg-[var(--bg-tertiary)] flex items-center justify-center flex-shrink-0">
        <Icon className="w-[16px] h-[16px] text-[var(--text-secondary)]" />
      </div>

      {/* Info */}
      <div className="flex-1 min-w-0">
        <div className="text-[13px] text-[var(--text-primary)] truncate leading-tight">
          {truncateFilename(artifact.title, 28)}
        </div>
        <div className="text-[11px] text-[var(--text-tertiary)] leading-tight mt-[2px]">
          {artifactKindLabel(artifact.kind)} · {formatFileSize(artifact.size_bytes)}
        </div>
      </div>
    </div>
  );
}
