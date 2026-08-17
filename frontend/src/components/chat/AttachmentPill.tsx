import { FileText, FileSpreadsheet, FolderOpen, FileCode, File } from "lucide-react";
import { AttachmentType } from "../../types/api";

/** Map attachment type to icon component */
export function getAttachmentIcon(type: AttachmentType | undefined) {
  switch (type) {
    case "document": return FileText;
    case "spreadsheet": return FileSpreadsheet;
    case "folder": return FolderOpen;
    case "code": return FileCode;
    default: return File;
  }
}

/** Map attachment type to pill color classes for user messages (on accent bg) */
export function getUserPillColors(type: AttachmentType | undefined): string {
  switch (type) {
    case "document": return "bg-white/15 border-white/25 text-white/90";
    case "spreadsheet": return "bg-white/15 border-white/25 text-white/90";
    case "folder": return "bg-white/15 border-white/25 text-white/90";
    case "code": return "bg-white/15 border-white/25 text-white/90";
    default: return "bg-white/15 border-white/25 text-white/90";
  }
}

/** Map attachment type to pill color classes for agent messages */
export function getAgentPillColors(type: AttachmentType | undefined): string {
  switch (type) {
    case "document": return "bg-blue-500/15 border-blue-500/30 text-blue-600 dark:text-blue-300";
    case "spreadsheet": return "bg-green-500/15 border-green-500/30 text-green-600 dark:text-green-300";
    case "folder": return "bg-amber-500/15 border-amber-500/30 text-amber-600 dark:text-amber-300";
    case "code": return "bg-gray-500/15 border-gray-500/30 text-gray-600 dark:text-gray-300";
    default: return "bg-[var(--bg-hover)] border-[var(--border-primary)] text-[var(--text-secondary)]";
  }
}

/** Truncate a filename to max chars with ellipsis, preserving extension */
export function truncateFilename(name: string, max = 20): string {
  if (name.length <= max) return name;
  const ext = name.lastIndexOf(".");
  if (ext > 0 && name.length - ext <= 6) {
    const extStr = name.slice(ext);
    const base = name.slice(0, max - extStr.length - 1);
    return `${base}…${extStr}`;
  }
  return name.slice(0, max - 1) + "…";
}
