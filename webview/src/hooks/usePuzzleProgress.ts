// store imports
import { useBackendData, useGameProgress } from '../services/adapters'

// data imports
import { getPuzzles } from '../data/puzzleData'
import { Puzzle } from '../models/PuzzleListItem';

// SolidJS hooks related to puzzle list data
export function usePuzzleProgress() {
	const puzzleData = useBackendData(state => state.puzzleData);
	const isPuzzleComplete = useGameProgress(state => state.isPuzzleComplete);

	// figures out what the id of the next puzzle is 
	const getNextPuzzle = (): Puzzle | null => {
		const puzzles = getPuzzles();
		const currentPuzzleData = puzzleData();

		const currentIndex = puzzles.findIndex((puzzle) => puzzle.id === currentPuzzleData.puzzleId);
		return (currentIndex >= 0 && currentIndex < puzzles.length - 1)
			? puzzles[currentIndex + 1] : null;
	}

	// calculate the number of puzzles for this category that have been completed
	const getNumCompletePuzzles = (categoryId: number): number => {
		const puzzlesInCategory = getPuzzles().filter((puzzle) => puzzle.categoryId === categoryId);
		const checkComplete = isPuzzleComplete();
		const completePuzzles = puzzlesInCategory.filter((puzzle) => checkComplete(puzzle.id));
		return completePuzzles.length;
	}

	// determind whether a category is unlocked by checking all puzzles from previous category for completion
	const categoryUnlocked = (categoryId: number): boolean => {
		if (categoryId === 1) return true;

		const checkComplete = isPuzzleComplete();
		return getPuzzles()
			.filter((puzzle) => puzzle.categoryId === categoryId - 1)
			.reduce((acc, puzzle) => acc && checkComplete(puzzle.id), true);
	}

	return { getNextPuzzle, getNumCompletePuzzles, categoryUnlocked };
}
