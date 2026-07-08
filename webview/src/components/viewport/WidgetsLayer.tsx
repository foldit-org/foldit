import LogoDropdown from '../widgets/LogoDropdown.tsx';
import PuzzleComplete from '../widgets/PuzzleComplete';
import ScoreBar from '../widgets/ScoreBar.tsx';
import SideBar from '../widgets/SideBar.tsx';
import TextBubble from '../widgets/TextBubble.tsx';
import ActionButtonWidget from '../widgets/ActionButtonWidget.tsx';

export default function WidgetsLayer() {
	// Note: inGame check is now handled by parent (App.tsx) to avoid subscribing to entire store
	return (
		<div class="z-20">
			<LogoDropdown />
			<ScoreBar />
			<SideBar />
			<PuzzleComplete />
			<TextBubble />
			<ActionButtonWidget />
		</div>
	);
}
