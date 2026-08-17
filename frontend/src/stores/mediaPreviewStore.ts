import { create } from "zustand";

interface ImageListItem {
    content: string;
    contentType: "svg" | "image";
    filename?: string;
}

interface MediaPreviewState {
    isOpen: boolean;
    content: string;
    contentType: "svg" | "image";
    filename?: string;
    imageList?: ImageListItem[];
    currentIndex: number;
    hasNext: boolean;
    hasPrev: boolean;
    openPreview: (params: {
        content: string;
        contentType: "svg" | "image";
        filename?: string;
        imageList?: ImageListItem[];
        currentIndex?: number;
    }) => void;
    closePreview: () => void;
    navigateNext: () => void;
    navigatePrev: () => void;
}

export const useMediaPreviewStore = create<MediaPreviewState>()((set, get) => ({
    isOpen: false,
    content: "",
    contentType: "svg",
    filename: undefined,
    imageList: undefined,
    currentIndex: 0,
    hasNext: false,
    hasPrev: false,
    openPreview: ({ content, contentType, filename, imageList, currentIndex }) => {
        const index = currentIndex ?? 0;
        set({
            isOpen: true,
            content,
            contentType,
            filename,
            imageList,
            currentIndex: index,
            hasNext: imageList ? index < imageList.length - 1 : false,
            hasPrev: imageList ? index > 0 : false,
        });
    },
    closePreview: () =>
        set({
            isOpen: false,
            content: "",
            contentType: "svg",
            filename: undefined,
            imageList: undefined,
            currentIndex: 0,
            hasNext: false,
            hasPrev: false,
        }),
    navigateNext: () => {
        const { imageList, currentIndex } = get();
        if (!imageList || currentIndex >= imageList.length - 1) return;
        const newIndex = currentIndex + 1;
        const item = imageList[newIndex];
        set({
            currentIndex: newIndex,
            content: item.content,
            contentType: item.contentType,
            filename: item.filename,
            hasNext: newIndex < imageList.length - 1,
            hasPrev: true,
        });
    },
    navigatePrev: () => {
        const { imageList, currentIndex } = get();
        if (!imageList || currentIndex <= 0) return;
        const newIndex = currentIndex - 1;
        const item = imageList[newIndex];
        set({
            currentIndex: newIndex,
            content: item.content,
            contentType: item.contentType,
            filename: item.filename,
            hasNext: true,
            hasPrev: newIndex > 0,
        });
    },
}));
