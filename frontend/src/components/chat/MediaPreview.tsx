import { useEffect, useRef, useState, useCallback } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { X, ZoomIn, ZoomOut, RotateCcw, Download, Copy, Check, ChevronDown, ChevronLeft, ChevronRight, ImageOff } from "lucide-react";
import { toBlob } from "html-to-image";
import { save } from "@tauri-apps/plugin-dialog";
import { writeFile } from "@tauri-apps/plugin-fs";
import { invoke } from "@tauri-apps/api/core";
import { useMediaPreviewStore } from "../../stores/mediaPreviewStore";

export function MediaPreview() {
    const { isOpen, content, contentType, filename, closePreview, imageList, currentIndex, navigateNext, navigatePrev, hasNext, hasPrev } =
        useMediaPreviewStore();
    const [isHovered, setIsHovered] = useState(false);
    const [toast, setToast] = useState<string | null>(null);
    const [showDownloadMenu, setShowDownloadMenu] = useState(false);
    const [imageError, setImageError] = useState(false);
    const toastTimerRef = useRef<ReturnType<typeof setTimeout>>(null);
    const downloadMenuRef = useRef<HTMLDivElement>(null);

    const [scale, setScale] = useState(1);
    const [translateX, setTranslateX] = useState(0);
    const [translateY, setTranslateY] = useState(0);
    const [isDragging, setIsDragging] = useState(false);
    const [isResetting, setIsResetting] = useState(false);

    const containerRef = useRef<HTMLDivElement>(null);
    const svgContentRef = useRef<HTMLDivElement>(null);
    const dragStartRef = useRef({ x: 0, y: 0, translateX: 0, translateY: 0 });

    // Reset zoom/pan and image error when modal opens or closes
    useEffect(() => {
        if (!isOpen) {
            setScale(1);
            setTranslateX(0);
            setTranslateY(0);
            setImageError(false);
        }
    }, [isOpen]);

    // Reset zoom/pan when navigating between images
    useEffect(() => {
        setScale(1);
        setTranslateX(0);
        setTranslateY(0);
        setImageError(false);
    }, [currentIndex]);

    useEffect(() => {
        if (!isOpen) return;
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape") closePreview();
            if (e.key === "ArrowRight" && imageList) navigateNext();
            if (e.key === "ArrowLeft" && imageList) navigatePrev();
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [isOpen, closePreview, imageList, navigateNext, navigatePrev]);

    const handleWheel = useCallback(
        (e: WheelEvent) => {
            e.preventDefault();
            const container = containerRef.current;
            if (!container) return;

            const rect = container.getBoundingClientRect();
            const mouseX = e.clientX - rect.left;
            const mouseY = e.clientY - rect.top;

            setScale((prevScale) => {
                const zoomFactor = e.deltaY > 0 ? 0.92 : 1.08;
                const newScale = Math.min(5.0, Math.max(0.25, prevScale * zoomFactor));
                const ratio = newScale / prevScale;

                setTranslateX((prev) => mouseX - (mouseX - prev) * ratio);
                setTranslateY((prev) => mouseY - (mouseY - prev) * ratio);

                return newScale;
            });
        },
        []
    );

    useEffect(() => {
        const container = containerRef.current;
        if (!container || !isOpen) return;
        container.addEventListener("wheel", handleWheel, { passive: false });
        return () => container.removeEventListener("wheel", handleWheel);
    }, [isOpen, handleWheel]);

    const handleMouseDown = useCallback(
        (e: React.MouseEvent) => {
            if (e.button !== 0) return;
            setIsDragging(true);
            dragStartRef.current = {
                x: e.clientX,
                y: e.clientY,
                translateX,
                translateY,
            };
        },
        [translateX, translateY]
    );

    const handleMouseMove = useCallback(
        (e: React.MouseEvent) => {
            if (!isDragging) return;
            const dx = e.clientX - dragStartRef.current.x;
            const dy = e.clientY - dragStartRef.current.y;
            setTranslateX(dragStartRef.current.translateX + dx);
            setTranslateY(dragStartRef.current.translateY + dy);
        },
        [isDragging]
    );

    const handleMouseUp = useCallback(() => {
        setIsDragging(false);
    }, []);

    const handleDoubleClick = useCallback(() => {
        setIsResetting(true);
        setScale(1);
        setTranslateX(0);
        setTranslateY(0);
        setTimeout(() => setIsResetting(false), 200);
    }, []);

    const handleZoomIn = useCallback(() => {
        setScale((prev) => Math.min(5.0, prev + 0.25));
    }, []);

    const handleZoomOut = useCallback(() => {
        setScale((prev) => Math.max(0.25, prev - 0.25));
    }, []);

    const handleResetZoom = useCallback(() => {
        setIsResetting(true);
        setScale(1);
        setTranslateX(0);
        setTranslateY(0);
        setTimeout(() => setIsResetting(false), 200);
    }, []);

    const showToast = useCallback((message: string) => {
        if (toastTimerRef.current) clearTimeout(toastTimerRef.current);
        setToast(message);
        toastTimerRef.current = setTimeout(() => setToast(null), 2000);
    }, []);

    const sanitizeSvgString = useCallback((svgString: string): string => {
        // Sanitize HTML void elements inside foreignObject for valid XML
        return svgString.replace(
            /<(br|hr|img|input|col|area|base|link|meta|source|track|wbr)(\s[^>]*?)?\s*>/gi,
            "<$1$2/>"
        );
    }, []);

    const captureNodeAsBlob = useCallback(async (): Promise<Blob> => {
        const node = svgContentRef.current;
        if (!node) throw new Error("SVG content node not found");
        const blob = await toBlob(node, { pixelRatio: 2, skipFonts: true });
        if (!blob) throw new Error("Failed to capture node as PNG");
        return blob;
    }, []);

    const triggerDownload = useCallback((blob: Blob, name: string) => {
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = name;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        URL.revokeObjectURL(url);
    }, []);

    const uniqueName = useCallback(
        (ext: string) => {
            const base = (filename || "diagram").replace(/\.\w+$/, "");
            const id = crypto.randomUUID().slice(0, 8);
            return `${base}-${id}.${ext}`;
        },
        [filename]
    );

    const handleDownloadSvg = useCallback(() => {
        const sanitized = sanitizeSvgString(content);
        const blob = new Blob([sanitized], { type: "image/svg+xml" });
        triggerDownload(blob, uniqueName("svg"));
        showToast("Downloaded as SVG");
        setShowDownloadMenu(false);
    }, [content, uniqueName, triggerDownload, sanitizeSvgString, showToast]);

    const handleDownloadPng = useCallback(async () => {
        try {
            const blob = await captureNodeAsBlob();
            triggerDownload(blob, uniqueName("png"));
            showToast("Downloaded as PNG");
        } catch {
            showToast("Download failed");
        }
        setShowDownloadMenu(false);
    }, [uniqueName, triggerDownload, captureNodeAsBlob, showToast]);

    const handleDownload = useCallback(async () => {
        if (contentType === "svg") {
            // For SVGs, show format picker
            setShowDownloadMenu((prev) => !prev);
            return;
        }

        try {
            const res = await fetch(content);
            if (!res.ok) throw new Error(`Fetch failed: ${res.status}`);
            const blob = await res.blob();

            // Derive extension from response content-type or filename
            const mimeToExt: Record<string, string> = {
                "image/png": "png",
                "image/jpeg": "jpg",
                "image/webp": "webp",
                "image/gif": "gif",
            };
            const ext = mimeToExt[blob.type] || "png";
            const defaultName = filename || `image-${crypto.randomUUID().slice(0, 8)}.${ext}`;

            const savePath = await save({
                defaultPath: defaultName,
                filters: [{ name: "Image", extensions: [ext] }],
            });
            if (!savePath) return; // user cancelled

            const arrayBuffer = await blob.arrayBuffer();
            await writeFile(savePath, new Uint8Array(arrayBuffer));
            showToast("Downloaded");
        } catch {
            showToast("Download failed");
        }
    }, [content, contentType, filename, showToast]);

    const convertToPngBlob = useCallback(async (src: string): Promise<Blob> => {
        return new Promise((resolve, reject) => {
            const img = new Image();
            img.crossOrigin = "anonymous";
            img.onload = () => {
                const canvas = document.createElement("canvas");
                canvas.width = img.naturalWidth;
                canvas.height = img.naturalHeight;
                const ctx = canvas.getContext("2d");
                if (!ctx) { reject(new Error("Canvas context unavailable")); return; }
                ctx.drawImage(img, 0, 0);
                canvas.toBlob((blob) => {
                    if (blob) resolve(blob);
                    else reject(new Error("Canvas toBlob returned null"));
                }, "image/png");
            };
            img.onerror = () => reject(new Error("Failed to load image for conversion"));
            img.src = src;
        });
    }, []);

    const handleCopyToClipboard = useCallback(async () => {
        try {
            let pngBlob: Blob;
            if (contentType === "svg") {
                pngBlob = await captureNodeAsBlob();
            } else {
                const res = await fetch(content);
                const blob = await res.blob();
                if (blob.type === "image/png") {
                    pngBlob = blob;
                } else {
                    // Convert non-PNG images to PNG via canvas
                    const objectUrl = URL.createObjectURL(blob);
                    try {
                        pngBlob = await convertToPngBlob(objectUrl);
                    } finally {
                        URL.revokeObjectURL(objectUrl);
                    }
                }
            }
            // Use native Tauri command — navigator.clipboard.write doesn't work in WKWebView
            const arrayBuffer = await pngBlob.arrayBuffer();
            await invoke("copy_image_to_clipboard", {
                pngData: Array.from(new Uint8Array(arrayBuffer)),
            });
            showToast("Copied to clipboard");
        } catch (err) {
            console.error("Copy to clipboard failed:", err);
            showToast("Copy failed");
        }
    }, [content, contentType, captureNodeAsBlob, convertToPngBlob, showToast]);

    // Close download menu on outside click
    useEffect(() => {
        if (!showDownloadMenu) return;
        const handleClick = (e: MouseEvent) => {
            if (downloadMenuRef.current && !downloadMenuRef.current.contains(e.target as Node)) {
                setShowDownloadMenu(false);
            }
        };
        document.addEventListener("mousedown", handleClick);
        return () => document.removeEventListener("mousedown", handleClick);
    }, [showDownloadMenu]);

    return (
        <AnimatePresence>
            {isOpen && (
                <motion.div
                    initial={{ opacity: 0 }}
                    animate={{ opacity: 1 }}
                    exit={{ opacity: 0 }}
                    transition={{ duration: 0.2, ease: "easeOut" }}
                    className="fixed inset-0 z-[100] flex items-center justify-center bg-black/80 backdrop-blur-sm"
                    onMouseEnter={() => setIsHovered(true)}
                    onMouseMove={() => setIsHovered(true)}
                    onMouseLeave={() => setIsHovered(false)}
                    onClick={(e) => {
                        if (e.target === e.currentTarget) closePreview();
                    }}
                >
                    {/* Top bar */}
                    <div
                        className="absolute top-0 left-0 right-0 z-10 flex items-center justify-end gap-2 px-4 py-3 border-b border-white/10 bg-black/90 transition-opacity duration-200 ease-in-out"
                        style={{
                            opacity: isHovered ? 1 : 0,
                            pointerEvents: isHovered ? "auto" : "none",
                        }}
                    >
                        <div className="flex items-center gap-2 min-w-0">
                            <span className="text-white text-sm truncate max-w-[50vw]">
                                {filename || ""}
                            </span>
                            {imageList && (
                                <span className="text-white/60 text-sm whitespace-nowrap">
                                    {currentIndex + 1} / {imageList.length}
                                </span>
                            )}
                        </div>
                        <button
                            onClick={closePreview}
                            className="p-2 rounded hover:bg-white/10 text-white transition-colors"
                        >
                            <X size={20} />
                        </button>
                    </div>

                    {/* Content */}
                    <motion.div
                        initial={{ opacity: 0, scale: 0.95 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.95 }}
                        transition={{ duration: 0.2, ease: "easeOut" }}
                        className="max-w-[90vw] max-h-[85vh] flex items-center justify-center"
                    >
                        <div
                            ref={containerRef}
                            className="max-w-[90vw] max-h-[85vh]"
                            style={{
                                cursor: isDragging ? "grabbing" : "grab",
                            }}
                            onMouseDown={handleMouseDown}
                            onMouseMove={handleMouseMove}
                            onMouseUp={handleMouseUp}
                            onMouseLeave={handleMouseUp}
                            onDoubleClick={handleDoubleClick}
                        >
                            <div
                                style={{
                                    transform: `translate(${translateX}px, ${translateY}px) scale(${scale})`,
                                    transformOrigin: "0 0",
                                    willChange: "transform",
                                    transition: isDragging
                                        ? "none"
                                        : isResetting
                                            ? "transform 0.3s ease-out"
                                            : "transform 0.15s ease-out",
                                }}
                            >
                                {contentType === "svg" ? (
                                    <div
                                        ref={svgContentRef}
                                        dangerouslySetInnerHTML={{
                                            __html: content,
                                        }}
                                        className="bg-[var(--bg-tertiary)] rounded-lg p-4 [&>svg]:w-[80vw] [&>svg]:h-auto [&>svg]:max-h-[80vh]"
                                    />
                                ) : imageError ? (
                                    <div className="flex flex-col items-center justify-center gap-3 p-8 bg-[var(--bg-tertiary)] rounded-lg min-w-[200px] min-h-[150px]">
                                        <ImageOff size={48} className="text-white/40" />
                                        <span className="text-white/60 text-sm">Image not available</span>
                                    </div>
                                ) : (
                                    <img
                                        src={content}
                                        alt={filename || "Preview"}
                                        className="max-w-[90vw] max-h-[85vh] object-contain"
                                        draggable={false}
                                        onError={() => setImageError(true)}
                                    />
                                )}
                            </div>
                        </div>
                    </motion.div>

                    {/* Navigation arrows */}
                    {imageList && hasPrev && (
                        <button
                            onClick={(e) => { e.stopPropagation(); navigatePrev(); }}
                            className="absolute left-4 top-1/2 -translate-y-1/2 z-10 p-2 rounded-full bg-black/50 hover:bg-black/70 text-white transition-all duration-200 cursor-pointer"
                            style={{
                                opacity: isHovered ? 1 : 0,
                                pointerEvents: isHovered ? "auto" : "none",
                            }}
                        >
                            <ChevronLeft size={24} />
                        </button>
                    )}
                    {imageList && hasNext && (
                        <button
                            onClick={(e) => { e.stopPropagation(); navigateNext(); }}
                            className="absolute right-4 top-1/2 -translate-y-1/2 z-10 p-2 rounded-full bg-black/50 hover:bg-black/70 text-white transition-all duration-200 cursor-pointer"
                            style={{
                                opacity: isHovered ? 1 : 0,
                                pointerEvents: isHovered ? "auto" : "none",
                            }}
                        >
                            <ChevronRight size={24} />
                        </button>
                    )}

                    {/* Bottom bar */}
                    <div
                        className="absolute bottom-0 left-0 right-0 z-10 flex items-center justify-center gap-2 px-4 py-3 border-t border-white/10 bg-black/90 transition-opacity duration-200 ease-in-out"
                        style={{
                            opacity: isHovered ? 1 : 0,
                            pointerEvents: isHovered ? "auto" : "none",
                        }}
                    >
                        <button
                            onClick={handleZoomOut}
                            className="p-2 rounded hover:bg-white/10 text-white transition-colors"
                        >
                            <ZoomOut size={20} />
                        </button>
                        <span className="text-white text-sm min-w-[3.5rem] text-center select-none">
                            {Math.round(scale * 100)}%
                        </span>
                        <button
                            onClick={handleZoomIn}
                            className="p-2 rounded hover:bg-white/10 text-white transition-colors"
                        >
                            <ZoomIn size={20} />
                        </button>
                        <button
                            onClick={handleResetZoom}
                            className="p-2 rounded hover:bg-white/10 text-white transition-colors"
                        >
                            <RotateCcw size={20} />
                        </button>
                        <div className="w-px h-5 bg-white/20 mx-1" />
                        <button
                            onClick={handleCopyToClipboard}
                            className="p-2 rounded hover:bg-white/10 text-white transition-colors"
                            title="Copy to clipboard"
                        >
                            <Copy size={20} />
                        </button>
                        <div className="relative" ref={downloadMenuRef}>
                            <button
                                onClick={handleDownload}
                                className="p-2 rounded hover:bg-white/10 text-white transition-colors flex items-center gap-1"
                                title="Download"
                            >
                                <Download size={20} />
                                {contentType === "svg" && <ChevronDown size={14} />}
                            </button>
                            <AnimatePresence>
                                {showDownloadMenu && (
                                    <motion.div
                                        initial={{ opacity: 0, y: 4 }}
                                        animate={{ opacity: 1, y: 0 }}
                                        exit={{ opacity: 0, y: 4 }}
                                        transition={{ duration: 0.15 }}
                                        className="absolute bottom-full mb-2 right-0 bg-black/90 backdrop-blur-md rounded-lg border border-white/10 overflow-hidden min-w-[140px]"
                                    >
                                        <button
                                            onClick={handleDownloadSvg}
                                            className="w-full px-3 py-2 text-sm text-white hover:bg-white/10 text-left transition-colors"
                                        >
                                            Download SVG
                                        </button>
                                        <button
                                            onClick={handleDownloadPng}
                                            className="w-full px-3 py-2 text-sm text-white hover:bg-white/10 text-left transition-colors"
                                        >
                                            Download PNG
                                        </button>
                                    </motion.div>
                                )}
                            </AnimatePresence>
                        </div>
                    </div>

                    {/* Toast */}
                    <AnimatePresence>
                        {toast && (
                            <motion.div
                                initial={{ opacity: 0, y: 20 }}
                                animate={{ opacity: 1, y: 0 }}
                                exit={{ opacity: 0, y: 20 }}
                                transition={{ duration: 0.2 }}
                                className="fixed bottom-20 left-1/2 -translate-x-1/2 z-[60] flex items-center gap-2 px-4 py-2 bg-black/90 backdrop-blur-md text-white text-sm rounded-full border border-white/10 shadow-lg"
                            >
                                <Check size={16} className="text-green-400" />
                                {toast}
                            </motion.div>
                        )}
                    </AnimatePresence>
                </motion.div>
            )}
        </AnimatePresence>
    );
}
