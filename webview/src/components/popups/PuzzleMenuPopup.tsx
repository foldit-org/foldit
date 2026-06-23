import { createSignal, createMemo, Show } from 'solid-js';

// model & state imports
import { Category } from '../../models/PuzzleListItem';
import { useUI, useBackendData, useFrontendState, useGameProgress } from '../../services/adapters';
import { usePuzzleProgress } from '../../hooks/usePuzzleProgress';

// util & service imports
import {
	getNumPuzzles, getCategoryForPuzzle, getPuzzleList,
	getCategoryList, getFirstPuzzleInCategory
} from '../../utils/puzzleDataUtils';
import { loadPuzzle } from '../../services/PuzzleLoader';

// component imports
import Button from '../util/Button';

// Vite Asset Globbing - moved outside component to avoid recreation on each render
const puzzleImages: Record<string, any> = import.meta.glob('../../assets/puzzle_previews/*.png', { eager: true, import: 'default' });
const getPuzzleImage = (id: number) => puzzleImages[`../../assets/puzzle_previews/${id}.png`];

const PuzzleItem = ({ puzzle, isInGame, isComplete, highScore, image, onPlay }: any) => (
	<div class={isInGame ? "ingame-menu-list-item" : "landing-menu-list-item"}>
		<div class="flex flex-col w-full h-full p-4">
			<div class="text-lg font-semibold text-gray-100 my-2">{puzzle.title}</div>
			<div class="flex flex-row w-full items-center justify-between">
				<div class="w-[50%]">
					<img src={image} alt="preview" class="rounded shadow-sm" />
				</div>
				<div class="flex flex-col w-[50%] items-start pl-6">
					{isComplete ? (
						<div class="text-sm font-semibold text-green-400">High Score: {highScore?.toFixed(2)}</div>
					) : (
						<div class="text-sm font-semibold text-yellow-400">Incomplete</div>
					)}
					<p class="text-sm text-gray-400 pb-4 line-clamp-3">{puzzle.description}</p>
					<Button effect="gleam" activeEffect="translate" callback={onPlay}>
						PLAY LEVEL
					</Button>
				</div>
			</div>
		</div>
	</div>
);

const CategoryItem = ({ category, isInGame, progress, numComplete, total, previewImage, onSelect }: any) => (
	<div class={isInGame ? "ingame-menu-list-item cursor-pointer" : "landing-menu-list-item cursor-pointer"}
		 onClick={onSelect}>
		<div class="flex flex-col w-full h-full p-4">
			<h4 class="text-xl font-bold">{category.title}</h4>
			<div class="flex flex-row w-full my-4 justify-between items-center">
				<img src={previewImage} alt="category" class="w-20 h-20 object-cover rounded" />
				<div class="flex flex-col items-end mr-4">
					<div class="w-32 h-2 bg-gray-800 rounded-full overflow-hidden">
						<div class="h-full bg-blue-500 transition-all duration-500" style={{ width: `${progress}%` }} />
					</div>
					<div class="text-[10px] text-gray-500 mt-1 uppercase tracking-tighter">
						{numComplete}/{total} Complete
					</div>
				</div>
			</div>
			<p class="text-sm text-gray-400 italic">{category.description}</p>
		</div>
	</div>
);

export default function PuzzleMenuPopup() {
	const toggleWidget = useUI(state => state.toggleWidget);
	const puzzleMenu = useUI(state => state.puzzleMenu);
	const puzzleData = useBackendData(state => state.puzzleData);
	const appScreen = useFrontendState(state => state.appScreen);
	const isPuzzleComplete = useGameProgress(state => state.isPuzzleComplete);
	const getHighScore = useGameProgress(state => state.getHighScore);
	const { getNumCompletePuzzles } = usePuzzleProgress();

	const isInGame = createMemo(() => appScreen() === 'IN_SESSION');

	const [category, setCategory] = createSignal<Category | null>(
		isInGame() ? getCategoryForPuzzle(puzzleData().puzzleId) : null
	);

	// Stable callback for loading puzzles
	const handlePlayPuzzle = (puzzleId: number) => {
		loadPuzzle(puzzleId);
	};

	// Stable callback for selecting categories
	const handleSelectCategory = (cat: Category) => {
		setCategory(cat);
	};

	// memoize the entire array of JSX elements to minimize re-renders
	const itemList = createMemo(() => {
		const currentCategory = category();
		if (currentCategory) {
			return getPuzzleList()
				.filter(p => p.categoryId === currentCategory.id)
				.map(puzzle => (
					<PuzzleItem
						puzzle={puzzle}
						isInGame={isInGame()}
						isComplete={isPuzzleComplete()(puzzle.id)}
						highScore={getHighScore()(puzzle.id)}
						image={getPuzzleImage(puzzle.id)}
						onPlay={() => handlePlayPuzzle(puzzle.id)}
					/>
				));
		}

		return getCategoryList().map(cat => {
			const total = getNumPuzzles(cat.id);
			const complete = getNumCompletePuzzles(cat.id);
			const firstPuzzle = getFirstPuzzleInCategory(cat.id);

			return (
				<CategoryItem
					category={cat}
					isInGame={isInGame()}
					onSelect={() => handleSelectCategory(cat)}
					progress={total > 0 ? (complete / total) * 100 : 0}
					numComplete={complete}
					total={total}
					previewImage={firstPuzzle ? getPuzzleImage(firstPuzzle.id) : ''}
				/>
			);
		});
	});

	// Effect to scroll to top when category changes
	createMemo(() => {
		const currentCategory = category();
		const container = document.getElementById('scroll-container');
		if (container) container.scrollTop = 0;
		return currentCategory; // Track category for reactivity
	});

	return (
		<Show when={puzzleMenu()}>
			<div class={isInGame() ? "ingame-popup-menu" : "landing-popup-menu"}>
				<div class="popup-menu-content w-[70%] h-[70%] relative bg-[#121212] rounded-2xl shadow-2xl border border-white/10">
					<button class="absolute top-6 right-6 text-2xl opacity-50 hover:opacity-100" onClick={() => toggleWidget()('puzzleMenu')}>✕</button>

					<Show when={category()}>
						<button class="absolute top-6 left-6 text-blue-400 hover:text-blue-300 font-semibold" onClick={() => setCategory(null)}>
							← BACK
						</button>
					</Show>

					<h2 class="text-center py-8 text-2xl font-light tracking-widest text-gray-200 border-b border-white/5">
						{category() ? category()!.title : 'SELECT CATEGORY'}
					</h2>

					<div id="scroll-container" class="h-[calc(100%-100px)] w-full overflow-y-auto divide-y divide-white/5 px-4 scroll-smooth">
						{itemList()}
					</div>
				</div>
			</div>
		</Show>
	);
}

