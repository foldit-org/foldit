declare global {
	interface Window {
		// Set by webview C++ bindings to detect webview environment
		isWebview?: boolean;
	}
}
