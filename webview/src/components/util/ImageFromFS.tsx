/**
 * Loads images from the filesystem via the backend.
 */

import { Component, createSignal, createEffect, onCleanup, Show } from 'solid-js';
import { readResourceFile } from '../../utils/resourceReader';

interface ImageFromFSProps {
  path: string;
}

const ImageFromFS: Component<ImageFromFSProps> = (props) => {
  const { path } = props;

  const [url, setUrl] = createSignal<string | null>(null);

  createEffect(() => {
    let newUrl: string | null = null;
    let cancelled = false;

    (async () => {
      try {
        console.log(`[ImageFromFS] Loading image from path: ${path}`);
        const response = await readResourceFile(path);

        if (cancelled) return;

        // Check if response is base64 encoded
        let fileData: string;
        if (typeof response === 'object' && response.encoding === 'base64') {
          fileData = response.content;
        } else {
          fileData = typeof response === 'string' ? response : response.content;
        }

        // Decode base64 to binary
        const binaryString = atob(fileData);
        const bytes = new Uint8Array(binaryString.length);
        for (let i = 0; i < binaryString.length; i++) {
          bytes[i] = binaryString.charCodeAt(i);
        }

        // Determine MIME type from extension
        const mimeType = path.endsWith('.jpg') || path.endsWith('.jpeg')
          ? 'image/jpeg'
          : path.endsWith('.gif') ? 'image/gif' : 'image/png';

        const blob = new Blob([bytes], { type: mimeType });
        newUrl = URL.createObjectURL(blob);
        setUrl(newUrl);
      } catch (e: unknown) {
        console.error(`[ImageFromFS] Failed to read file with path ${path}`);
        if (e instanceof Error) {
          console.error(`[ImageFromFS] Error message: ${e.message}`);
        }
      }
    })();

    onCleanup(() => {
      cancelled = true;
      if (newUrl) {
        URL.revokeObjectURL(newUrl);
      }
    });
  });

  return (
    <Show when={url()}>
      <img src={url()!} alt={url()!} draggable={false} />
    </Show>
  );
};

export default ImageFromFS;
