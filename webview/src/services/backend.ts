export enum BackendEnvironment {
	EMSCRIPTEN = 'emscripten',
	WEBVIEW = 'webview'
}

class Backend {
	private environment: BackendEnvironment;

	constructor(environment: BackendEnvironment) {
		this.environment = environment;
	}

	getEnvironment(): BackendEnvironment {
		return this.environment;
	}
}

function detectEnvironment(): BackendEnvironment {
	if (typeof window !== 'undefined' && (window as any).isWebview) {
		return BackendEnvironment.WEBVIEW;
	}
	return BackendEnvironment.EMSCRIPTEN;
}

export const backend = new Backend(detectEnvironment());
