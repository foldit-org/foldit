declare global {
	interface Window {
		_paq?: any[];
	}
}

const SITE_ID = "11";

export const initMatomo = () => {
	if (window._paq) return; // already initialized
	if ((window as any).isWebview) return; // no analytics in native webview

	window._paq = [];
	window._paq.push(["setTrackerUrl", "/metrics/"]);
	window._paq.push(["setSiteId", SITE_ID]);
	window._paq.push(["trackPageView"]);
	window._paq.push(["enableLinkTracking"]);

	const script = document.createElement("script");
	script.async = true;
	script.src = "/metrics.js";
	document.head.appendChild(script);
};

export const trackPageView = () => {
	window._paq?.push(["trackPageView"]);
};

export const trackEvent = (
	category: string,
	action: string,
	name?: string,

	value?: number
) => {
	window._paq?.push(["trackEvent", category, action, name, value]);
};
