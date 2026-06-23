export type BrowserInfo = {
	browser: string;
	browserVersion: string;
	engineFamily: 'Chromium' | 'Firefox' | 'Safari' | 'IE' | 'Unknown';
	engineVersion: string;
}

export type PlatformInfo = {
	os: string;
	osVersion: string;
}

export function getPlatformInfo(): PlatformInfo {
	const ua = navigator.userAgent;
	let os = 'Unknown';
	let osVersion = 'Unknown';

	if (/Windows/.test(ua)) {
		os = 'Windows';
		const match = ua.match(/Windows NT ([\d.]+)/);
		if (match) {
			const versionMap: { [key: string]: string } = {
				'10.0': '10/11',
				'6.3': '8.1',
				'6.2': '8',
				'6.1': '7',
				'6.0': 'Vista',
				'5.1': 'XP',
				'5.0': '2000'
			};
			osVersion = versionMap[match[1]] || match[1];
		}
	} else if (/Macintosh/.test(ua)) {
		os = 'Mac';
		const match = ua.match(/Mac OS X ([\d_]+)/);
		if (match) {
			osVersion = match[1].replace(/_/g, '.');
		}
	} else if (/Linux/.test(ua)) {
		os = 'Linux';
		// Linux versions are rarely in UA, but we can check for distro if present, though unlikely
	} else if (/Android/.test(ua)) {
		os = 'Android';
		const match = ua.match(/Android ([\d.]+)/);
		if (match) {
			osVersion = match[1];
		}
	} else if (/iOS|iPhone|iPad|iPod/.test(ua)) {
		os = 'iOS';
		const match = ua.match(/OS ([\d_]+)/);
		if (match) {
			osVersion = match[1].replace(/_/g, '.');
		}
	}

	return { os, osVersion };
}

export function getBrowserInfo(): BrowserInfo {
	const ua = navigator.userAgent;

	let browser = 'Unknown';
	let browserVersion = 'Unknown';
	let engineFamily: BrowserInfo['engineFamily'] = 'Unknown';

	let engineVersion = 'Unknown';

	if (/Edg\/([\d.]+)/.test(ua)) {
		browser = 'Edge';
		browserVersion = ua.match(/Edg\/([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'Chromium';

		engineVersion = ua.match(/Chrome\/([\d.]+)/)?.[1] || browserVersion;
	} else if (/OPR\/([\d.]+)/.test(ua)) {
		browser = 'Opera';
		browserVersion = ua.match(/OPR\/([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'Chromium';
		engineVersion = ua.match(/Chrome\/([\d.]+)/)?.[1] || browserVersion;
	} else if (/Chrome\/([\d.]+)/.test(ua)) {
		browser = 'Chrome';
		browserVersion = ua.match(/Chrome\/([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'Chromium';
		engineVersion = browserVersion;
	} else if (/Firefox\/([\d.]+)/.test(ua)) {
		browser = 'Firefox';
		browserVersion = ua.match(/Firefox\/([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'Firefox';
		engineVersion = ua.match(/rv:([\d.]+)/)?.[1] || browserVersion;
	} else if (/Safari\/([\d.]+)/.test(ua) && /Version\/([\d.]+)/.test(ua) && !/Chrome|Chromium|Edg|OPR/.test(ua)) {
		browser = 'Safari';
		browserVersion = ua.match(/Version\/([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'Safari';
		engineVersion = ua.match(/AppleWebKit\/([\d.]+)/)?.[1] || 'Unknown';
	} else if (/Trident\/([\d.]+)/.test(ua)) {
		browser = 'Internet Explorer';
		browserVersion = ua.match(/rv:([\d.]+)/)?.[1] || 'Unknown';
		engineFamily = 'IE';
		engineVersion = ua.match(/Trident\/([\d.]+)/)?.[1] || 'Unknown';
	}

	return {
		browser,
		browserVersion,
		engineFamily,
		engineVersion,
	};
}
