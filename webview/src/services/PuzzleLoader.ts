import { appCommand, state } from '../adapter';
import { getUIState } from '../hooks/state';
import { getPuzzles } from '../data/puzzleData'
import { trackEvent } from './Matomo';

export const loadPuzzle = (puzzleId: number) => {
	// Close the puzzle menu (and any victory surfaces) before dispatching.
	const ui = getUIState();
	ui.hideWidget('puzzleMenu');
	ui.hideWidget('puzzleCompleteMenu');
	ui.hideWidget('puzzleCompleteWidget');

	trackEvent("Puzzle", "Load", `${puzzleId}`);

	appCommand({ type: 'LoadPuzzle', puzzle_id: puzzleId });
}

// Reload the current puzzle by id.
export const replayPuzzle = () => {
	loadPuzzle(state.puzzle.puzzle_id);
}

// Find the next puzzle in the global puzzle list and load it. No-op if the
// current id isn't in the list or there is no successor.
export const playNextPuzzle = () => {
	const puzzles = getPuzzles();
	const currentIndex = puzzles.findIndex(p => p.id === state.puzzle.puzzle_id);
	if (currentIndex < 0) return;
	const nextPuzzle = puzzles[currentIndex + 1];
	if (!nextPuzzle) return;
	loadPuzzle(nextPuzzle.id);
}
