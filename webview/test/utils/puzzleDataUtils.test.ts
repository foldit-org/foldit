import { getNumPuzzles, getCategoryForPuzzle, getFirstPuzzleInCategory } from '../../src/utils/puzzleDataUtils'
import { vi, describe, it, expect } from 'vitest'

vi.mock('../../src/data/puzzleData.ts', () => {
	return {
		getPuzzles: () => [
			{ id: 1, title: 'puzzle 1', description: 'puzzle 1 description', categoryId: 1 },
			{ id: 2, title: 'puzzle 2', description: 'puzzle 2 description', categoryId: 1 },
			{ id: 3, title: 'puzzle 3', description: 'puzzle 3 description', categoryId: 2 },
			{ id: 4, title: 'puzzle 4', description: 'puzzle 4 description', categoryId: 2 },
			{ id: 5, title: 'puzzle 5', description: 'puzzle 5 description', categoryId: 3 },
			{ id: 6, title: 'puzzle 6', description: 'puzzle 6 description', categoryId: 3 },
			{ id: 7, title: 'puzzle 7', description: 'puzzle 7 description', categoryId: 5 },
		],
		getCategories: () => [
			{ id: 1, title: 'category 1', description: 'category 1 description' },
			{ id: 2, title: 'category 2', description: 'category 2 description' },
			{ id: 3, title: 'category 3', description: 'category 3 description' },
			{ id: 4, title: 'category 4', description: 'category 4 description' },
		]
	}
})

describe('getNumPuzzles', () => {
	it('returns correct number of puzzles from populated category', () => {
		const result = getNumPuzzles(1);
		expect(result).toBe(2);
	})

	it('returns correct number of puzzles from empty category', () => {
		const result = getNumPuzzles(4);
		expect(result).toBe(0);
	})

	it('returns 0 for a category that does not exist', () => {
		const result = getNumPuzzles(100);
		expect(result).toBe(0);
	})
})

describe('getCategoryForPuzzle', () => {
	it('returns correct category for a puzzle', () => {
		const result = getCategoryForPuzzle(1);
		expect(result).toEqual({ id: 1, title: 'category 1', description: 'category 1 description' });
	})

	it('returns null for a puzzle that does not exist', () => {
		const result = getCategoryForPuzzle(100);
		expect(result).toBeNull();
	})

	it('returns null when the puzzle exists but the category does not', () => {
		const result = getCategoryForPuzzle(7);
		expect(result).toBeNull();
	})
})

describe('getFirstPuzzleInCategory', () => {
	it('returns the first puzzle in a populated category', () => {
		const result = getFirstPuzzleInCategory(1);
		expect(result).toEqual({ id: 1, title: 'puzzle 1', description: 'puzzle 1 description', categoryId: 1 });
	})
	it('returns null for an empty category', () => {
		const result = getFirstPuzzleInCategory(4);
		expect(result).toBeNull();
	})
	it('returns null for a category that does not exist', () => {
		const result = getFirstPuzzleInCategory(100);
		expect(result).toBeNull();
	})
});
