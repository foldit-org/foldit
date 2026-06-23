import { Puzzle, Category } from '../models/PuzzleListItem'
import { getPuzzles, getCategories } from '../data/puzzleData'

// returns the number of puzzles in a given category
export const getNumPuzzles = (categoryId: number): number => {
	const puzzlesInCategory = getPuzzles().filter((puzzle) => puzzle.categoryId === categoryId);
	return puzzlesInCategory.length;
}

export const getCategoryForPuzzle = (puzzleId: number): Category | null => {
	const puzzle = getPuzzles().find((puzzle) => puzzle.id === puzzleId);
	if (!puzzle) return null;


	const category = getCategories().find((category) => category.id === puzzle.categoryId);
	return category ? category : null;
}

export const getPuzzleList = (): Puzzle[] => {
	return getPuzzles().map((data) => new Puzzle(data.id, data.title, data.description, data.categoryId));
}

export const getCategoryList = (): Category[] => {
	return getCategories().map((data) => new Category(data.id, data.title, data.description));
}

export const getFirstPuzzleInCategory = (categoryId: number): Puzzle | null => {
	const puzzle = getPuzzles().find((puzzle) => puzzle.categoryId === categoryId);
	return puzzle ? new Puzzle(puzzle.id, puzzle.title, puzzle.description, puzzle.categoryId) : null;
}
