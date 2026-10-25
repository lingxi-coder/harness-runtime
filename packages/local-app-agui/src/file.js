import {validateContentPart} from './validators.js';

export const MAX_FILE_BYTES = 25 * 1024 * 1024;

function safeName(name) {
  return typeof name === 'string' && name.length > 0 && name.length <= 255
    && name !== '.' && name !== '..' && !/[\/\\\u0000-\u001f\u007f]/u.test(name);
}

function newPartId() {
  if (typeof globalThis.crypto?.randomUUID === 'function') return globalThis.crypto.randomUUID();
  if (!globalThis.crypto?.getRandomValues) throw new Error('Secure random IDs are unavailable in this browser.');
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  return [...bytes].map((value, index) => `${index === 4 || index === 6 || index === 8 || index === 10 ? '-' : ''}${value.toString(16).padStart(2, '0')}`).join('');
}

function encodeBase64(bytes) {
  let binary = '';
  const chunkSize = 0x8000;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, Math.min(offset + chunkSize, bytes.length)));
  }
  return btoa(binary);
}

/**
 * Read a browser File into an official inline AG-UI ContentPart. The returned
 * part contains the standard inline DataSource and no File object is retained.
 */
export async function fileToContentPart(file) {
  if (typeof File !== 'function' || !(file instanceof File)) {
    throw new TypeError('file must be a browser File object.');
  }
  if (!safeName(file.name)) throw new TypeError('The file needs a safe file name.');
  if (file.size > MAX_FILE_BYTES) {
    const error = new RangeError('Each attachment must be 25 MiB or smaller.');
    error.code = 'attachment_too_large';
    throw error;
  }
  if (/^(audio|video)\//iu.test(file.type)) {
    const error = new TypeError('Audio and video file parts are not supported by this Local App host.');
    error.code = 'unsupported_media_type';
    throw error;
  }
  const extension = file.name.toLowerCase().split('.').pop();
  const textMimeTypes = {
    txt: 'text/plain;charset=utf-8',
    md: 'text/markdown;charset=utf-8',
    csv: 'text/csv;charset=utf-8',
    json: 'application/json',
  };
  const mimeType = file.type || textMimeTypes[extension] || 'application/octet-stream';
  const part = {
    type: mimeType.toLowerCase().startsWith('image/') ? 'image' : 'document',
    id: newPartId(),
    source: {
      type: 'data',
      value: encodeBase64(new Uint8Array(await file.arrayBuffer())),
      mimeType,
    },
    metadata: {'com.lingxi.localApp': {version: 1, filename: file.name}},
  };
  const checked = validateContentPart(part);
  return checked;
}
