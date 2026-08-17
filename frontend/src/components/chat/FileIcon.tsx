import { twMerge } from "tailwind-merge";

/** SVG document shape with corner fold — fills container width, flush to bottom */
const FileImage = ({
  mainColor,
  cutOutColor,
  opacity = "1",
}: {
  mainColor: string;
  cutOutColor: string;
  opacity?: string;
}) => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    viewBox="0 0 280 120"
    fill={mainColor}
    className="w-full h-auto block"
    preserveAspectRatio="xMidYMax meet"
  >
    <path d="M 20 20 C 20 11.716 26.716 5 35 5 L 215 5 Q 220 5 223 8 L 272 57 Q 275 60 275 65 L 275 120 L 20 120 L 20 20 Z" />
    <path d="M 220 8 L 220 60 L 272 60 Z" fill={cutOutColor} opacity={opacity} />
  </svg>
);

/** SVG folder shape — fills container width, flush to bottom */
const FolderImage = ({ mainColor }: { mainColor: string }) => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    viewBox="0 0 280 120"
    fill={mainColor}
    className="w-full h-auto block"
    preserveAspectRatio="xMidYMax meet"
  >
    {/* Folder tab */}
    <path d="M 20 20 C 20 11.716 26.716 5 35 5 L 110 5 L 130 25 L 260 25 C 268.284 25 275 31.716 275 40 L 275 120 L 20 120 Z" />
    {/* Tab top edge highlight */}
    <path d="M 35 5 C 26.716 5 20 11.716 20 20 L 20 30 L 130 30 L 110 10 L 35 10 C 30 10 25 14 25 20 L 25 25 L 20 25 L 20 20 C 20 11.716 26.716 5 35 5 Z" opacity="0.15" fill="#fff" />
  </svg>
);

interface IconTheme {
  iconBackgroundColor: string;
  textBackgroundColor: string;
  textColor: string;
  iconColor: string;
  cutOutColor: string;
}

/** Color themes per file type */
function getIconTheme(fileType: string): IconTheme {
  switch (fileType) {
    case "pdf":
    case "document":
      return {
        iconBackgroundColor: "#E19B05",
        textBackgroundColor: "#FCF2D9",
        textColor: "#E19B05",
        iconColor: "#FCF2D9",
        cutOutColor: "#000",
      };
    case "code":
      return {
        iconBackgroundColor: "#0EA5E9",
        textBackgroundColor: "#E0F2FE",
        textColor: "#0EA5E9",
        iconColor: "#E0F2FE",
        cutOutColor: "#000",
      };
    case "spreadsheet":
      return {
        iconBackgroundColor: "#10B981",
        textBackgroundColor: "#D1FAE5",
        textColor: "#10B981",
        iconColor: "#D1FAE5",
        cutOutColor: "#000",
      };
    case "folder":
      return {
        iconBackgroundColor: "#F59E0B",
        textBackgroundColor: "#FEF3C7",
        textColor: "#F59E0B",
        iconColor: "#FEF3C7",
        cutOutColor: "#000",
      };
    default:
      return {
        iconBackgroundColor: "#2840A9",
        textBackgroundColor: "#DCE3FA",
        textColor: "#2840A9",
        iconColor: "#DCE3FA",
        cutOutColor: "#000",
      };
  }
}

/** Get short extension label from filename */
function getExtLabel(fileName: string, fileType?: string): string {
  if (fileType === "folder") return "DIR";
  const dot = fileName.lastIndexOf(".");
  if (dot > 0) {
    const ext = fileName.slice(dot + 1).toUpperCase();
    return ext.length <= 4 ? ext : ext.slice(0, 3);
  }
  if (fileType) return fileType.slice(0, 3).toUpperCase();
  return "FILE";
}

interface FileIconProps {
  fileName: string;
  fileType?: string;
  previewUrl?: string;
  className?: string;
}

/**
 * Square file icon tile — 44×44, matches image thumbnail size.
 * Shows image preview if available, otherwise document shape with extension label.
 */
export const FileIcon = ({ fileName, fileType, previewUrl, className }: FileIconProps) => {
  const theme = getIconTheme(fileType ?? "other");
  const extLabel = getExtLabel(fileName, fileType);

  if (previewUrl) {
    return (
      <div
        className={twMerge(
          "w-[44px] h-[44px] overflow-hidden rounded-lg flex flex-col",
          className
        )}
      >
        <img
          src={previewUrl}
          alt={fileName}
          className="w-full h-full object-cover"
        />
      </div>
    );
  }

  return (
    <div
      className={twMerge(
        "w-[44px] h-[44px] overflow-hidden border border-black/10 dark:border-white/10 rounded-lg flex flex-col font-semibold shadow-sm",
        className
      )}
      style={{ backgroundColor: theme.iconBackgroundColor }}
    >
      {/* Shape — centered with breathing room, flush against bottom label */}
      <div className="flex-1 flex items-end justify-center overflow-hidden px-[6px]">
        {fileType === "folder" ? (
          <FolderImage mainColor={theme.iconColor} />
        ) : (
          <FileImage
            mainColor={theme.iconColor}
            cutOutColor={theme.cutOutColor}
            opacity="0.3"
          />
        )}
      </div>
      {/* Extension label strip */}
      <div
        className="text-[10px] font-bold w-full flex items-center justify-center uppercase py-[2px] tracking-wide leading-none"
        style={{
          backgroundColor: theme.textBackgroundColor,
          color: theme.textColor,
          borderTop: `1px solid ${theme.textColor}30`,
        }}
      >
        {extLabel}
      </div>
    </div>
  );
};
