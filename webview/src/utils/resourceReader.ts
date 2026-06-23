import { request } from '../transport';

/**
 * Read a resource file from the backend's bundle and return the response
 * envelope `{ encoding: 'base64', content: string }` (other shapes accepted
 * by callers that handle plain strings).
 */
export async function readResourceFile(filepath: string): Promise<any> {
	return request('read_resource_file', { filepath });
}
