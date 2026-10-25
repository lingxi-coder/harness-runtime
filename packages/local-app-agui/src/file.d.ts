import type {ContentPart} from '@ag-ui/core';

export declare const MAX_FILE_BYTES: number;
export declare function fileToContentPart(file: File): Promise<ContentPart>;
