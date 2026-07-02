import { onMount } from 'solid-js';

// state imports
import { useFrontendState } from './hooks/state';
import { loadPluginPanelCatalog } from './pluginPanelLoader';
import { loadSettingsCatalog } from './pluginSettingsLoader';

// component imports
import LoadingScreen from './components/util/LoadingScreen'
import ErrorScreen from './components/util/ErrorScreen'
import LoginMenuPopup from './components/popups/LoginMenuPopup'
import PuzzleMenuPopup from './components/popups/PuzzleMenuPopup'
import LandingScreen from './components/landing/LandingScreen.tsx';
import WidgetsLayer from './components/viewport/WidgetsLayer.tsx';
import PanelsLayer from './components/viewport/PanelsLayer.tsx';
import GlobalTooltipSystem from './components/viewport/GlobalTooltipSystem.tsx';
import RisingTextManager from './components/util/RisingTextManager.tsx';
import NotificationToaster from './components/util/NotificationToaster.tsx';

// hook imports
import { useMatomo } from './hooks/useMatomo.ts';
import { useInputForwarding } from './hooks/useInputForwarding.ts';

export default function App() {
	useInputForwarding();
	let gameRef: HTMLDivElement | undefined;

	// Global state
	const frontendState = useFrontendState();

	useMatomo();

	// One-shot: pull the plugin-panel catalog so launchers and panel
	// chrome can register. Empty until a plugin declares panels.
	onMount(() => { void loadPluginPanelCatalog(); });

	// One-shot: pull the plugin settings-tab catalog so settings buttons
	// and panels can register. Empty until a plugin declares settings.
	onMount(() => { void loadSettingsCatalog(); });

	return (
		<div ref={gameRef} id="app-container">
			<div id="ui-overlay">
				{/* should be z-50 and render over anything else */}
				<LoadingScreen />
				<ErrorScreen />
				{frontendState.appScreen() === 'LANDING' && <LandingScreen />}

				{/* should be z-40 and render over everything in the viewport */}
				<LoginMenuPopup />
				<PuzzleMenuPopup />

				{/* z-10 to z-30, rendering over wgpu surface with its own hierarchy */}
				{frontendState.appScreen() === 'IN_SESSION' && <WidgetsLayer />}
				{frontendState.appScreen() === 'IN_SESSION' && <PanelsLayer />}

				{/* Global tooltip system */}
				<GlobalTooltipSystem />

				{/* Rising text system */}
				<RisingTextManager />

				{/* Host notification toasts (top-right) */}
				<NotificationToaster />
			</div>
		</div>
	);
}

